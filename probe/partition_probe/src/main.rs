//! Cost probe for the prelude-aware component-partitioning design.
//!
//! Answers, with measurements rather than estimates, why formualizer-partitioned is slow on
//! aggregate-heavy sheets and which mini-workbook strategy is affordable. The
//! probe never reads a real workbook: it generates the post-rewrite shape of the
//! xlstream benchmark (row-local formulas, the four whole-column aggregates
//! already replaced by scalars) so that evaluation cost can be measured without
//! the XML reader or the dependency graph in the way.
//!
//! Findings this probe exists to reproduce (see README.md for the full table):
//!
//!   * `set_cell_formula` cost grows superlinearly with sheet extent, so a
//!     mini-workbook must stay short (~500 rows) no matter how many rows the
//!     source sheet has.
//!   * Reusing ONE workbook across chunks (formulas placed once, data values
//!     overwritten per chunk) beats one ephemeral workbook per batch by ~2.7x.
//!   * Rebasing chunk rows to 1..N is what makes that reuse legal and cheap.
//!   * Decomposable aggregates must be folded chunk by chunk; a single
//!     full-height prelude workbook costs 20x more time and 10x more memory.
//!   * Formualizer lookup cost is linear in lookup-table height and is not
//!     amortised by batch size, so a fast path must budget for it.
//!
//! Run `cargo run --release -- <subcommand>`; see README.md.

use std::time::Instant;

use formualizer::common::value::LiteralValue;
use formualizer::parse::parser::{parse, ASTNode, ASTNodeType, ReferenceType};
use formualizer::workbook::{Workbook, WorkbookConfig};

fn shift_coord(v: u32, d: i64, is_abs: bool) -> u32 {
    if is_abs {
        return v;
    }
    (v as i64 + d).max(1) as u32
}

fn shift_reference(r: &ReferenceType, dr: i64, dc: i64) -> ReferenceType {
    match r {
        ReferenceType::Cell { sheet, row, col, row_abs, col_abs } => ReferenceType::Cell {
            sheet: sheet.clone(),
            row: shift_coord(*row, dr, *row_abs),
            col: shift_coord(*col, dc, *col_abs),
            row_abs: *row_abs,
            col_abs: *col_abs,
        },
        ReferenceType::Range {
            sheet,
            start_row,
            start_col,
            end_row,
            end_col,
            start_row_abs,
            start_col_abs,
            end_row_abs,
            end_col_abs,
        } => ReferenceType::Range {
            sheet: sheet.clone(),
            start_row: start_row.map(|v| shift_coord(v, dr, *start_row_abs)),
            start_col: start_col.map(|v| shift_coord(v, dc, *start_col_abs)),
            end_row: end_row.map(|v| shift_coord(v, dr, *end_row_abs)),
            end_col: end_col.map(|v| shift_coord(v, dc, *end_col_abs)),
            start_row_abs: *start_row_abs,
            start_col_abs: *start_col_abs,
            end_row_abs: *end_row_abs,
            end_col_abs: *end_col_abs,
        },
        other => other.clone(),
    }
}

/// Rebuild a template AST as seen from another cell. Mirrors
/// `formualizer_partitioned::partition::shift_ast`; kept local so the probe does not link the
/// PyO3 extension crate.
fn shift_ast(node: &ASTNode, dr: i64, dc: i64) -> ASTNode {
    if dr == 0 && dc == 0 {
        return node.clone();
    }
    let node_type = match &node.node_type {
        ASTNodeType::Reference { original, reference } => ASTNodeType::Reference {
            original: original.clone(),
            reference: shift_reference(reference, dr, dc),
        },
        ASTNodeType::UnaryOp { op, expr } => ASTNodeType::UnaryOp {
            op: op.clone(),
            expr: Box::new(shift_ast(expr, dr, dc)),
        },
        ASTNodeType::BinaryOp { op, left, right } => ASTNodeType::BinaryOp {
            op: op.clone(),
            left: Box::new(shift_ast(left, dr, dc)),
            right: Box::new(shift_ast(right, dr, dc)),
        },
        ASTNodeType::Function { name, args } => ASTNodeType::Function {
            name: name.clone(),
            args: args.iter().map(|a| shift_ast(a, dr, dc)).collect(),
        },
        ASTNodeType::Call { callee, args } => ASTNodeType::Call {
            callee: Box::new(shift_ast(callee, dr, dc)),
            args: args.iter().map(|a| shift_ast(a, dr, dc)).collect(),
        },
        ASTNodeType::Array(rows) => ASTNodeType::Array(
            rows.iter()
                .map(|row| row.iter().map(|a| shift_ast(a, dr, dc)).collect())
                .collect(),
        ),
        other => other.clone(),
    };
    let mut out = ASTNode::new(node_type, node.source_token.clone());
    out.contains_volatile = node.contains_volatile;
    out
}

/// Row-local formula templates anchored at row 2.
///
/// Mirrors the xlstream fixture's 30 formulas per row, except that the four
/// whole-column aggregates appear as the literals a prelude pass would have
/// substituted. `lookup_mode`: 0 none, 1 whole-column lookup ranges, 2 bounded
/// lookup ranges.
fn templates(lookup_mode: u8) -> Vec<String> {
    let mut v: Vec<String> = vec![
        "=A2+B2",
        "=C2-D2",
        "=E2*F2",
        "=G2/H2",
        "=I2^2",
        "=-J2",
        "=K2%",
        "=L2&M2",
        "=N2>O2",
        "=P2=Q2",
        "=IF(A2>5000,B2,C2)",
        "=IFS(A2>7500,\"P\",A2>5000,\"G\",TRUE,\"B\")",
        "=AND(A2>0,B2>0)",
        "=IFERROR(G2/H2,0)",
        // The four prelude-rewritten aggregates.
        "=B2/24941234.5*100",
        "=5001.25",
        "=1234567.5",
        "=4821",
        "=LEFT(L2,3)",
        "=UPPER(M2)",
        "=ROUND(E2,2)",
        "=MOD(F2,G2)",
        "=YEAR(S2)",
        "=EDATE(S2,3)",
        "=ISNUMBER(A2)",
        "=TYPE(B2)",
        "=TEXT(E2,\"0.00\")",
        "=VALUE(\"123\")",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    if lookup_mode == 1 {
        v.push("=VLOOKUP(MOD(A2,1000)+1,Lookup1!A:D,2,FALSE)".to_string());
        v.push("=INDEX(Lookup1!C:C,MATCH(MOD(A2,1000)+1,Lookup1!A:A,0))".to_string());
    } else if lookup_mode == 2 {
        v.push("=VLOOKUP(MOD(A2,1000)+1,Lookup1!$A$1:$D$1000,2,FALSE)".to_string());
        v.push(
            "=INDEX(Lookup1!$C$1:$C$1000,MATCH(MOD(A2,1000)+1,Lookup1!$A$1:$A$1000,0))".to_string(),
        );
    }
    v
}

fn data_cell(row: u32, col: u32) -> LiteralValue {
    let seed = (row as f64) * 7.0 + (col as f64) * 13.0;
    match col {
        12 => LiteralValue::Text(format!("code{}", row % 997)),
        13 => LiteralValue::Text(format!("name{}", row % 331)),
        18 => {
            LiteralValue::Text(["EMEA", "APAC", "AMER", "LATAM"][(row % 4) as usize].to_string())
        }
        19 => LiteralValue::Number(45292.0 + (row % 700) as f64),
        20 => LiteralValue::Boolean(row % 2 == 0),
        _ => LiteralValue::Number(1000.0 + (seed % 8000.0)),
    }
}

/// Lookup sheet height is `LOOKUP_ROWS` (default 1000), which is the knob that
/// exposes Formualizer's linear per-call lookup cost.
fn fill_lookup(wb: &mut Workbook) -> Result<(), String> {
    let rows: u32 = std::env::var("LOOKUP_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    wb.add_sheet("Lookup1").map_err(|e| e.to_string())?;
    for r in 1..=rows {
        wb.set_value("Lookup1", r, 1, LiteralValue::Number(r as f64))
            .map_err(|e| e.to_string())?;
        wb.set_value("Lookup1", r, 2, LiteralValue::Text(format!("region{}", r % 4)))
            .map_err(|e| e.to_string())?;
        wb.set_value("Lookup1", r, 3, LiteralValue::Number((r * 3) as f64))
            .map_err(|e| e.to_string())?;
        wb.set_value("Lookup1", r, 4, LiteralValue::Number((r * 7) as f64))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// One ephemeral mini-workbook per batch.
///
/// `rebase` maps the batch onto rows 1..N instead of keeping the source row
/// numbers, which isolates how much the sheet's row offset alone costs.
fn batched(rows: u32, batch_rows: u32, rebase: bool, lookup_mode: u8) {
    let texts = templates(lookup_mode);
    let with_lookup = lookup_mode != 0;
    let asts: Vec<ASTNode> = texts.iter().map(|t| parse(t).expect("parse")).collect();
    let n_formula_cols = asts.len() as u32;
    let data_cols = 20u32;

    let mut t_setup = 0.0f64;
    let mut t_eval = 0.0f64;
    let mut t_read = 0.0f64;
    let mut n_batches = 0u64;
    let mut checksum = 0.0f64;
    let started = Instant::now();

    let mut start = 1u32;
    while start <= rows {
        let end = (start + batch_rows - 1).min(rows);
        n_batches += 1;

        let t0 = Instant::now();
        let mut wb = Workbook::new_with_config(WorkbookConfig::ephemeral());
        wb.add_sheet("Main").expect("add sheet");
        if with_lookup {
            fill_lookup(&mut wb).expect("lookup");
        }

        for src_row in start..=end {
            let target_row = if rebase { src_row - start + 1 } else { src_row };
            for c in 1..=data_cols {
                wb.set_value("Main", target_row, c, data_cell(src_row, c))
                    .expect("set value");
            }
            let dr = target_row as i64 - 2;
            for (i, ast) in asts.iter().enumerate() {
                let shifted = shift_ast(ast, dr, 0);
                wb.engine_mut()
                    .set_cell_formula("Main", target_row, data_cols + 1 + i as u32, shifted)
                    .expect("set formula");
            }
        }
        t_setup += t0.elapsed().as_secs_f64();

        let t1 = Instant::now();
        wb.evaluate_all().expect("evaluate");
        t_eval += t1.elapsed().as_secs_f64();

        let t2 = Instant::now();
        for src_row in start..=end {
            let target_row = if rebase { src_row - start + 1 } else { src_row };
            for i in 0..n_formula_cols {
                if let Some(LiteralValue::Number(n)) =
                    wb.get_value("Main", target_row, data_cols + 1 + i)
                {
                    checksum += n;
                }
            }
        }
        t_read += t2.elapsed().as_secs_f64();

        start = end + 1;
    }

    let total = started.elapsed().as_secs_f64();
    let formulas = rows as u64 * n_formula_cols as u64;
    println!(
        "BATCHED rows={rows} batch={batch_rows} rebase={rebase} lookup={lookup_mode} \
batches={n_batches} formula_cells={formulas} total_sec={total:.3} setup_sec={t_setup:.3} \
eval_sec={t_eval:.3} read_sec={t_read:.3} us_per_formula={:.2} checksum={checksum:.0}",
        total * 1e6 / formulas as f64
    );
}

/// Reuse one scratch workbook across chunks.
///
/// Formulas and lookup inputs are created once at rows 1..=batch_rows; each
/// chunk only overwrites the data values and re-evaluates. The checksum must
/// match `batched` for the same parameters.
fn scratch(rows: u32, batch_rows: u32, lookup_mode: u8) {
    let texts = templates(lookup_mode);
    let with_lookup = lookup_mode != 0;
    let asts: Vec<ASTNode> = texts.iter().map(|t| parse(t).expect("parse")).collect();
    let n_formula_cols = asts.len() as u32;
    let data_cols = 20u32;

    let started = Instant::now();
    let t0 = Instant::now();
    let mut wb = Workbook::new_with_config(WorkbookConfig::ephemeral());
    wb.add_sheet("Main").expect("add sheet");
    if with_lookup {
        fill_lookup(&mut wb).expect("lookup");
    }
    for target_row in 1..=batch_rows {
        let dr = target_row as i64 - 2;
        for (i, ast) in asts.iter().enumerate() {
            let shifted = shift_ast(ast, dr, 0);
            wb.engine_mut()
                .set_cell_formula("Main", target_row, data_cols + 1 + i as u32, shifted)
                .expect("set formula");
        }
    }
    let t_build = t0.elapsed().as_secs_f64();

    let mut t_setup = 0.0f64;
    let mut t_eval = 0.0f64;
    let mut t_read = 0.0f64;
    let mut n_batches = 0u64;
    let mut checksum = 0.0f64;

    let mut start = 1u32;
    while start <= rows {
        let end = (start + batch_rows - 1).min(rows);
        n_batches += 1;

        let t1 = Instant::now();
        for src_row in start..=end {
            let target_row = src_row - start + 1;
            for c in 1..=data_cols {
                wb.set_value("Main", target_row, c, data_cell(src_row, c))
                    .expect("set value");
            }
        }
        t_setup += t1.elapsed().as_secs_f64();

        let t2 = Instant::now();
        wb.evaluate_all().expect("evaluate");
        t_eval += t2.elapsed().as_secs_f64();

        let t3 = Instant::now();
        for src_row in start..=end {
            let target_row = src_row - start + 1;
            for i in 0..n_formula_cols {
                if let Some(LiteralValue::Number(n)) =
                    wb.get_value("Main", target_row, data_cols + 1 + i)
                {
                    checksum += n;
                }
            }
        }
        t_read += t3.elapsed().as_secs_f64();

        start = end + 1;
    }

    let total = started.elapsed().as_secs_f64();
    let formulas = rows as u64 * n_formula_cols as u64;
    println!(
        "SCRATCH rows={rows} batch={batch_rows} lookup={lookup_mode} batches={n_batches} \
formula_cells={formulas} total_sec={total:.3} build_sec={t_build:.3} setvalue_sec={t_setup:.3} \
eval_sec={t_eval:.3} read_sec={t_read:.3} us_per_formula={:.2} checksum={checksum:.0}",
        total * 1e6 / formulas as f64
    );
}

/// Naive prelude: materialise every source row in one workbook, then aggregate.
/// Included to show the cost that chunked folding avoids.
fn prelude(rows: u32) {
    let started = Instant::now();
    let mut wb = Workbook::new_with_config(WorkbookConfig::ephemeral());
    wb.add_sheet("P").expect("add sheet");

    let t0 = Instant::now();
    for r in 1..=rows {
        wb.set_value("P", r, 1, data_cell(r, 2)).expect("b");
        wb.set_value("P", r, 2, data_cell(r, 3)).expect("c");
        wb.set_value("P", r, 3, data_cell(r, 18)).expect("r");
    }
    let t_fill = t0.elapsed().as_secs_f64();

    let t1 = Instant::now();
    for (i, f) in [
        "=SUM(A:A)",
        "=AVERAGE(B:B)",
        "=SUMIF(C:C,\"EMEA\",A:A)",
        "=COUNTIF(A:A,\">500\")",
    ]
    .iter()
    .enumerate()
    {
        let ast = parse(f).expect("parse");
        wb.engine_mut()
            .set_cell_formula("P", 1, 5 + i as u32, ast)
            .expect("set formula");
    }
    wb.evaluate_all().expect("evaluate");
    let t_eval = t1.elapsed().as_secs_f64();

    let values: Vec<String> = (0..4)
        .map(|i| format!("{:?}", wb.get_value("P", 1, 5 + i)))
        .collect();
    println!(
        "PRELUDE rows={rows} total_sec={:.3} fill_sec={t_fill:.3} eval_sec={t_eval:.3} values={}",
        started.elapsed().as_secs_f64(),
        values.join(" | ")
    );
}

/// Chunked prelude: fold decomposable aggregates over a reused compact sheet.
///
/// `AVERAGE` is derived from `SUM`/`COUNT` because only additive aggregates
/// survive chunking; a non-decomposable aggregate such as `MEDIAN` cannot be
/// folded this way and must disqualify the fast path.
fn prelude_chunked(rows: u32, chunk: u32) {
    let started = Instant::now();
    let mut wb = Workbook::new_with_config(WorkbookConfig::ephemeral());
    wb.add_sheet("P").expect("add sheet");
    for (i, f) in [
        "=SUM(A1:A100000)",
        "=COUNT(A1:A100000)",
        "=SUM(B1:B100000)",
        "=COUNT(B1:B100000)",
        "=SUMIF(C1:C100000,\"EMEA\",A1:A100000)",
        "=COUNTIF(A1:A100000,\">500\")",
    ]
    .iter()
    .enumerate()
    {
        let bounded = f.replace("100000", &chunk.to_string());
        let ast = parse(&bounded).expect("parse");
        wb.engine_mut()
            .set_cell_formula("P", 1, 5 + i as u32, ast)
            .expect("set formula");
    }

    let mut acc = [0.0f64; 6];
    let mut t_fill = 0.0;
    let mut t_eval = 0.0;
    let mut start = 1u32;
    while start <= rows {
        let end = (start + chunk - 1).min(rows);
        let t0 = Instant::now();
        for src_row in start..=end {
            let r = src_row - start + 1;
            wb.set_value("P", r, 1, data_cell(src_row, 2)).expect("b");
            wb.set_value("P", r, 2, data_cell(src_row, 3)).expect("c");
            wb.set_value("P", r, 3, data_cell(src_row, 18)).expect("r");
        }
        // Blank the tail of a short final chunk so stale rows cannot be folded.
        for r in (end - start + 2)..=chunk {
            for c in 1..=3 {
                wb.set_value("P", r, c, LiteralValue::Empty).expect("blank");
            }
        }
        t_fill += t0.elapsed().as_secs_f64();

        let t1 = Instant::now();
        wb.evaluate_all().expect("evaluate");
        for (i, slot) in acc.iter_mut().enumerate() {
            if let Some(LiteralValue::Number(n)) = wb.get_value("P", 1, 5 + i as u32) {
                *slot += n;
            }
        }
        t_eval += t1.elapsed().as_secs_f64();
        start = end + 1;
    }

    println!(
        "PRELUDE_CHUNKED rows={rows} chunk={chunk} total_sec={:.3} fill_sec={t_fill:.3} \
eval_sec={t_eval:.3} sum_b={:.1} avg_c={:.4} sumif={:.1} countif={:.0}",
        started.elapsed().as_secs_f64(),
        acc[0],
        acc[2] / acc[3],
        acc[4],
        acc[5]
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("batched") => batched(
            args[2].parse().unwrap(),
            args[3].parse().unwrap(),
            args[4] == "1",
            args[5].parse().unwrap(),
        ),
        Some("scratch") => scratch(
            args[2].parse().unwrap(),
            args[3].parse().unwrap(),
            args[4].parse().unwrap(),
        ),
        Some("prelude") => prelude(args[2].parse().unwrap()),
        Some("prelude_chunked") => {
            prelude_chunked(args[2].parse().unwrap(), args[3].parse().unwrap())
        }
        _ => {
            eprintln!("usage:");
            eprintln!("  batched <rows> <batch_rows> <rebase 0|1> <lookup 0|1|2>");
            eprintln!("  scratch <rows> <batch_rows> <lookup 0|1|2>");
            eprintln!("  prelude <rows>");
            eprintln!("  prelude_chunked <rows> <chunk_rows>");
            eprintln!("env: LOOKUP_ROWS (default 1000)");
            std::process::exit(2);
        }
    }
}
