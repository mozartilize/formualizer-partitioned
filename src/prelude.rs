//! Fold whole-column aggregates into scalars before building the graph.
//!
//! ## Folding aggregates
//!
//! A formula such as `=B2/SUM(B:B)*100` reads a whole data column. That reference
//! makes its bounding box cover the column and prevents useful partitioning.
//! Replacing `SUM(B:B)` with its result leaves a row-local formula.
//!
//! The prelude folds each aggregate over short chunks of the source column. It
//! reuses a short sheet because formula-placement cost grows faster than sheet
//! height.
//!
//! The prelude accepts `SUM`, `COUNT`, `COUNTA`, `COUNTIF`, `SUMIF`, `MIN` and
//! `MAX`. It calculates `AVERAGE` from `SUM` and `COUNT`. It does not change an
//! aggregate such as `MEDIAN`, which needs every value at once.
//!
//! The engine evaluates each chunk. This crate does not implement the
//! arithmetic, so coercion, blank and text handling, errors and criteria keep
//! the whole-file semantics.
//!
//! The fold follows these rules:
//!
//! - A probe covers only the source rows of its aggregate.
//! - Aggregates with different row bands use different probes.
//! - A short final chunk uses probes that match its exact height.
//! - A criterion cannot match a scratch row without a source row.
//!
//! A folded `SUM` adds chunk totals, while a whole-column `SUM` adds cells. The
//! two methods can differ in the last digit of a floating-point result.

use std::collections::{HashMap, HashSet};

use formualizer::common::error::{ExcelError, ExcelErrorKind};
use formualizer::common::value::LiteralValue;
use formualizer::parse::parser::{parse, ASTNode, ASTNodeType, ReferenceType};
use formualizer::workbook::{Workbook, WorkbookConfig};

use crate::graph::Sources;

/// Source rows folded per chunk.
///
/// The cost of placing a formula grows faster than the sheet height, so the
/// scratch sheet must stay short whatever the source height is.
pub const CHUNK_ROWS: u32 = 500;

/// Largest number of distinct aggregates to fold.
///
/// Each aggregate adds one or two probe formulas to every chunk. A file with
/// thousands of distinct aggregates would make the fold cost more than the run
/// it saves, so the prelude stands down instead.
const MAX_AGGREGATES: usize = 64;

/// Largest number of source columns to copy into the scratch sheet.
///
/// Copying a column costs one value write per source row. A file that reads a
/// wide block of columns pays more for the fold than the fold returns.
const MAX_SOURCE_COLS: usize = 32;

/// What the prelude did, for diagnostics.
pub struct Report {
    /// Aggregate subtrees that met the contract and were folded.
    pub folded: usize,
    /// Distinct formula sources that were rewritten.
    pub rewritten: usize,
    /// Chunks evaluated across all sheets.
    pub chunks: usize,
    pub t_ms: f64,
}

impl Report {
    fn empty(t_ms: f64) -> Report {
        Report { folded: 0, rewritten: 0, chunks: 0, t_ms }
    }
}

/// Which aggregate an accepted subtree computes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Sum,
    Count,
    CountA,
    Min,
    Max,
    Average,
    SumIf,
    CountIf,
}

impl Kind {
    fn parse(name: &str) -> Option<Kind> {
        Some(match name.to_ascii_uppercase().as_str() {
            "SUM" => Kind::Sum,
            "COUNT" => Kind::Count,
            "COUNTA" => Kind::CountA,
            "MIN" => Kind::Min,
            "MAX" => Kind::Max,
            "AVERAGE" => Kind::Average,
            "SUMIF" => Kind::SumIf,
            "COUNTIF" => Kind::CountIf,
            _ => return None,
        })
    }
}

/// An inclusive rectangle on one sheet, in 1-based coordinates.
type Rect = (u32, u32, u32, u32);

/// One accepted aggregate, resolved to absolute coordinates.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    kind_tag: u8,
    sheet: u16,
    /// The tested range, then the summed range when `SUMIF` gives one.
    rects: Vec<Rect>,
    /// The criterion of `SUMIF` or `COUNTIF`, already rendered as a literal.
    criterion: Option<String>,
}

/// Running totals for one aggregate, across chunks.
#[derive(Default)]
struct State {
    sum: f64,
    count: f64,
    min: Option<f64>,
    max: Option<f64>,
    error: Option<ExcelError>,
    /// Set when a chunk gave something the fold cannot use, such as a date.
    failed: bool,
}

/// Fold every accepted aggregate and rewrite the formulas that hold them.
///
/// The workbook is left unchanged when nothing qualifies, so a file that has no
/// aggregates pays only the cost of one extra parse of each distinct formula.
pub fn fold_and_rewrite(data: &[u8], src: &mut Sources) -> Report {
    let t0 = std::time::Instant::now();
    if src.cells.is_empty() {
        return Report::empty(t0.elapsed().as_secs_f64() * 1000.0);
    }

    // A cell that reuses a template holds an offset from the anchor. An
    // aggregate is only accepted when every cell of its template resolves it to
    // the same rectangle, so record which offsets are in use per template.
    let mut fixed_row = vec![true; src.texts.len()];
    let mut fixed_col = vec![true; src.texts.len()];
    for fc in &src.cells {
        if fc.dr != 0 {
            fixed_row[fc.ast as usize] = false;
        }
        if fc.dc != 0 {
            fixed_col[fc.ast as usize] = false;
        }
    }

    let formulas = FormulaIndex::build(src);
    // The sheet each template was first seen on. One pass, because a lookup per
    // template over every cell would cost the product of the two counts.
    let mut owner: Vec<Option<u16>> = vec![None; src.texts.len()];
    for fc in &src.cells {
        let slot = &mut owner[fc.ast as usize];
        if slot.is_none() {
            *slot = Some(fc.sheet);
        }
    }
    let sheet_idx: HashMap<&str, u16> = src
        .sheets
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name.as_str(), i as u16))
        .collect();

    // Pass one: find every aggregate that meets the contract.
    let mut keys: Vec<Key> = Vec::new();
    let mut seen: HashSet<Key> = HashSet::new();
    for (k, text) in src.texts.iter().enumerate() {
        let Ok(ast) = parse(text) else { continue };
        let Some(own_sheet) = owner[k] else { continue };
        let ctx = Ctx {
            src,
            formulas: &formulas,
            sheet_idx: &sheet_idx,
            own_sheet,
            fixed_row: fixed_row[k],
            fixed_col: fixed_col[k],
        };
        collect(&ast, &ctx, &mut |key| {
            if seen.len() < MAX_AGGREGATES && seen.insert(key.clone()) {
                keys.push(key);
            }
        });
    }
    if keys.is_empty() {
        return Report::empty(t0.elapsed().as_secs_f64() * 1000.0);
    }

    // Fold, one sheet at a time.
    let mut states: Vec<State> = (0..keys.len()).map(|_| State::default()).collect();
    let mut chunks = 0usize;
    let mut sheets: Vec<u16> = keys.iter().map(|k| k.sheet).collect();
    sheets.sort_unstable();
    sheets.dedup();
    for sheet in sheets {
        let idx: Vec<usize> = (0..keys.len()).filter(|&i| keys[i].sheet == sheet).collect();
        match fold_sheet(data, &keys, &idx, sheet, &mut states) {
            Ok(n) => chunks += n,
            Err(()) => {
                for &i in &idx {
                    states[i].failed = true;
                }
            }
        }
    }

    // Turn the totals into values, and drop anything the fold could not finish.
    let mut values: HashMap<Key, LiteralValue> = HashMap::new();
    for (i, key) in keys.iter().enumerate() {
        if let Some(v) = finish(kind_of(key.kind_tag), &states[i]) {
            values.insert(key.clone(), v);
        }
    }
    let folded = values.len();
    if values.is_empty() {
        return Report::empty(t0.elapsed().as_secs_f64() * 1000.0);
    }

    // Pass two: put the values back into the formulas.
    let mut rewritten = 0usize;
    for k in 0..src.texts.len() {
        let Ok(ast) = parse(&src.texts[k]) else { continue };
        let Some(own_sheet) = owner[k] else { continue };
        let ctx = Ctx {
            src,
            formulas: &formulas,
            sheet_idx: &sheet_idx,
            own_sheet,
            fixed_row: fixed_row[k],
            fixed_col: fixed_col[k],
        };
        let mut hits = 0usize;
        let rebuilt = substitute(&ast, &ctx, &values, &mut hits);
        if hits == 0 {
            continue;
        }
        if let Some(text) = render_and_verify(&rebuilt) {
            src.texts[k] = text.into_boxed_str();
            rewritten += 1;
        }
    }

    Report {
        folded,
        rewritten,
        chunks,
        t_ms: t0.elapsed().as_secs_f64() * 1000.0,
    }
}

/// Where the formula cells are, so a range can be proved to hold data only.
struct FormulaIndex {
    /// Per sheet, the formula positions sorted by column, then row.
    by_col: Vec<Vec<(u32, u32)>>,
}

impl FormulaIndex {
    fn build(src: &Sources) -> FormulaIndex {
        let mut by_col: Vec<Vec<(u32, u32)>> = vec![Vec::new(); src.sheets.len()];
        for fc in &src.cells {
            by_col[fc.sheet as usize].push((fc.col, fc.row));
        }
        for v in &mut by_col {
            v.sort_unstable();
        }
        FormulaIndex { by_col }
    }

    /// Does this rectangle hold a formula cell?
    ///
    /// An aggregate over a rectangle that holds one cannot be folded ahead of
    /// the graph, because that cell has no value yet.
    fn any_in(&self, sheet: u16, (r0, c0, r1, c1): Rect) -> bool {
        let Some(positions) = self.by_col.get(sheet as usize) else {
            return false;
        };
        let start = positions.partition_point(|&(c, _)| c < c0);
        let end = positions.partition_point(|&(c, _)| c <= c1);
        positions[start..end].iter().any(|&(_, r)| r >= r0 && r <= r1)
    }
}

/// What one formula needs to resolve its references.
struct Ctx<'a> {
    src: &'a Sources,
    formulas: &'a FormulaIndex,
    sheet_idx: &'a HashMap<&'a str, u16>,
    own_sheet: u16,
    /// Every cell of this template sits at the anchor row.
    fixed_row: bool,
    /// Every cell of this template sits at the anchor column.
    fixed_col: bool,
}

fn kind_of(tag: u8) -> Kind {
    match tag {
        0 => Kind::Sum,
        1 => Kind::Count,
        2 => Kind::CountA,
        3 => Kind::Min,
        4 => Kind::Max,
        5 => Kind::Average,
        6 => Kind::SumIf,
        _ => Kind::CountIf,
    }
}

fn tag_of(kind: Kind) -> u8 {
    match kind {
        Kind::Sum => 0,
        Kind::Count => 1,
        Kind::CountA => 2,
        Kind::Min => 3,
        Kind::Max => 4,
        Kind::Average => 5,
        Kind::SumIf => 6,
        Kind::CountIf => 7,
    }
}

/// Walk a formula and report every aggregate that meets the contract.
fn collect(node: &ASTNode, ctx: &Ctx, out: &mut impl FnMut(Key)) {
    if let Some(key) = accept(node, ctx) {
        out(key);
        // An accepted aggregate is replaced whole, so its arguments are not
        // searched again.
        return;
    }
    for child in children(node) {
        collect(child, ctx, out);
    }
}

/// Rebuild a formula with every accepted aggregate replaced by its value.
fn substitute(
    node: &ASTNode,
    ctx: &Ctx,
    values: &HashMap<Key, LiteralValue>,
    hits: &mut usize,
) -> ASTNode {
    if let Some(key) = accept(node, ctx) {
        if let Some(v) = values.get(&key) {
            *hits += 1;
            return ASTNode::new(ASTNodeType::Literal(v.clone()), None);
        }
    }
    let node_type = match &node.node_type {
        ASTNodeType::UnaryOp { op, expr } => ASTNodeType::UnaryOp {
            op: op.clone(),
            expr: Box::new(substitute(expr, ctx, values, hits)),
        },
        ASTNodeType::BinaryOp { op, left, right } => ASTNodeType::BinaryOp {
            op: op.clone(),
            left: Box::new(substitute(left, ctx, values, hits)),
            right: Box::new(substitute(right, ctx, values, hits)),
        },
        ASTNodeType::Function { name, args } => ASTNodeType::Function {
            name: name.clone(),
            args: args.iter().map(|a| substitute(a, ctx, values, hits)).collect(),
        },
        ASTNodeType::Call { callee, args } => ASTNodeType::Call {
            callee: Box::new(substitute(callee, ctx, values, hits)),
            args: args.iter().map(|a| substitute(a, ctx, values, hits)).collect(),
        },
        ASTNodeType::Array(rows) => ASTNodeType::Array(
            rows.iter()
                .map(|row| row.iter().map(|a| substitute(a, ctx, values, hits)).collect())
                .collect(),
        ),
        other => other.clone(),
    };
    ASTNode::new(node_type, node.source_token.clone())
}

fn children(node: &ASTNode) -> Vec<&ASTNode> {
    match &node.node_type {
        ASTNodeType::UnaryOp { expr, .. } => vec![expr],
        ASTNodeType::BinaryOp { left, right, .. } => vec![left, right],
        ASTNodeType::Function { args, .. } => args.iter().collect(),
        ASTNodeType::Call { callee, args } => {
            let mut v = vec![callee.as_ref()];
            v.extend(args.iter());
            v
        }
        ASTNodeType::Array(rows) => rows.iter().flatten().collect(),
        _ => Vec::new(),
    }
}

/// Decide whether this node is an aggregate the prelude can fold.
///
/// The test is deliberately narrow. Every range must resolve to fixed
/// coordinates, must lie on one sheet, and must hold data only. A criterion
/// must be a literal. Anything else keeps its formula unchanged.
fn accept(node: &ASTNode, ctx: &Ctx) -> Option<Key> {
    let ASTNodeType::Function { name, args } = &node.node_type else {
        return None;
    };
    let kind = Kind::parse(name)?;

    let (rects, criterion) = match kind {
        Kind::Sum
        | Kind::Count
        | Kind::CountA
        | Kind::Min
        | Kind::Max
        | Kind::Average => {
            if args.len() != 1 {
                return None;
            }
            (vec![resolve(&args[0], ctx)?], None)
        }
        Kind::CountIf => {
            if args.len() != 2 {
                return None;
            }
            (vec![resolve(&args[0], ctx)?], Some(literal_text(&args[1])?))
        }
        Kind::SumIf => {
            if args.len() != 2 && args.len() != 3 {
                return None;
            }
            let mut rects = vec![resolve(&args[0], ctx)?];
            if let Some(third) = args.get(2) {
                rects.push(resolve(third, ctx)?);
            }
            (rects, Some(literal_text(&args[1])?))
        }
    };

    // Every rectangle must be on the formula's own sheet. A cross-sheet
    // aggregate is possible but is not needed by the shapes this targets, and
    // it would complicate the sheet-at-a-time fold.
    let sheet = ctx.own_sheet;
    for &rect in &rects {
        if ctx.formulas.any_in(sheet, rect) {
            return None;
        }
    }
    // SUMIF compares two rectangles cell by cell, so they must be the same
    // shape. They must also cover the same rows, because the fold walks one row
    // band at a time. Excel resizes a mismatched sum range from its top-left
    // corner; that rule is not reproduced here, so such a call is left alone.
    if rects.len() == 2 && rects[0] != rects[1] {
        let (r0, c0, r1, c1) = rects[0];
        let (s0, d0, s1, d1) = rects[1];
        if (r0, r1) != (s0, s1) || (c1 - c0) != (d1 - d0) {
            return None;
        }
    }

    Some(Key {
        kind_tag: tag_of(kind),
        sheet,
        rects,
        criterion,
    })
}

/// Resolve a reference argument to a fixed rectangle on the formula's sheet.
///
/// A reference that would move with the cell is refused unless every cell of
/// the template sits at the anchor. An open side, as in `A:A`, is closed
/// against the sheet extent, which is what the graph stage does as well.
fn resolve(node: &ASTNode, ctx: &Ctx) -> Option<Rect> {
    let ASTNodeType::Reference { reference, .. } = &node.node_type else {
        return None;
    };
    let dims = ctx.src.sheets.get(ctx.own_sheet as usize)?;
    match reference {
        ReferenceType::Cell { sheet, row, col, row_abs, col_abs } => {
            same_sheet(sheet.as_deref(), ctx)?;
            if *row == 0 || *col == 0 {
                return None;
            }
            if !stable(*row_abs, ctx.fixed_row) || !stable(*col_abs, ctx.fixed_col) {
                return None;
            }
            Some((*row, *col, *row, *col))
        }
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
        } => {
            same_sheet(sheet.as_deref(), ctx)?;
            if [start_row, start_col, end_row, end_col].iter().any(|v| **v == Some(0)) {
                return None;
            }
            // A reference with every side open is broken, not whole-sheet.
            if start_row.is_none() && end_row.is_none() && start_col.is_none() && end_col.is_none()
            {
                return None;
            }
            if start_row.is_some() && !stable(*start_row_abs, ctx.fixed_row) {
                return None;
            }
            if end_row.is_some() && !stable(*end_row_abs, ctx.fixed_row) {
                return None;
            }
            if start_col.is_some() && !stable(*start_col_abs, ctx.fixed_col) {
                return None;
            }
            if end_col.is_some() && !stable(*end_col_abs, ctx.fixed_col) {
                return None;
            }
            let r0 = start_row.unwrap_or(1);
            let c0 = start_col.unwrap_or(1);
            let r1 = end_row.unwrap_or(dims.max_row.max(1));
            let c1 = end_col.unwrap_or(dims.max_col.max(1));
            Some((r0.min(r1), c0.min(c1), r0.max(r1), c0.max(c1)))
        }
        _ => None,
    }
}

/// A coordinate is stable when it is absolute, or when no cell of the template
/// moves along that axis.
fn stable(is_abs: bool, fixed: bool) -> bool {
    is_abs || fixed
}

fn same_sheet(name: Option<&str>, ctx: &Ctx) -> Option<()> {
    match name {
        None => Some(()),
        Some(n) => match ctx.sheet_idx.get(n) {
            Some(&i) if i == ctx.own_sheet => Some(()),
            _ => None,
        },
    }
}

/// Render a criterion argument, which must be a literal.
fn literal_text(node: &ASTNode) -> Option<String> {
    match &node.node_type {
        ASTNodeType::Literal(v) => render_literal(v),
        // A criterion is often written as a negative number or as `">"&x`; only
        // the plain unary minus over a number is accepted here.
        ASTNodeType::UnaryOp { op, expr } if op == "-" => {
            let ASTNodeType::Literal(v) = &expr.node_type else {
                return None;
            };
            match v {
                LiteralValue::Int(i) => Some(format!("(-{i})")),
                LiteralValue::Number(n) if n.is_finite() => Some(format!("(-{n})")),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Read one sheet once, then fold each group of aggregates over its own rows.
///
/// Aggregates are grouped by the row band they read. A group is needed because
/// the probe formula is placed once and reused, so every chunk it reads must
/// hold the same number of real source rows. `SUM(E2:E9)` and `SUM(A:A)` cover
/// different bands and cannot share one probe.
///
/// Returns the number of chunks, or `Err` when the sheet cannot be folded, in
/// which case its aggregates are dropped and their formulas stay unchanged.
fn fold_sheet(
    data: &[u8],
    keys: &[Key],
    idx: &[usize],
    sheet: u16,
    states: &mut [State],
) -> Result<usize, ()> {
    // The columns of every aggregate on this sheet, and the rows they span.
    let mut cols: Vec<u32> = Vec::new();
    let mut first_row = u32::MAX;
    let mut last_row = 0u32;
    for &i in idx {
        for &(r0, c0, r1, c1) in &keys[i].rects {
            for c in c0..=c1 {
                if !cols.contains(&c) {
                    cols.push(c);
                }
            }
            first_row = first_row.min(r0);
            last_row = last_row.max(r1);
        }
    }
    if cols.len() > MAX_SOURCE_COLS || first_row > last_row {
        return Err(());
    }
    cols.sort_unstable();
    // The scratch sheet uses a dense column layout, so a file whose aggregates
    // read column A and column ZZ still gets a narrow sheet. Both rectangles of
    // a SUMIF move by the same map, so they stay aligned.
    let col_map: HashMap<u32, u32> = cols
        .iter()
        .enumerate()
        .map(|(i, &c)| (c, i as u32 + 1))
        .collect();

    // Read the source values once for every group. Only the wanted columns and
    // the rows some aggregate reads are kept.
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data)).map_err(|_| ())?;
    let values = crate::values::Values::open(&mut zip);
    let parts = crate::graph::sheet_parts(&mut zip);
    let part = parts.get(sheet as usize).ok_or(())?.1.clone();
    let wanted: HashSet<u32> = cols.iter().copied().collect();
    let mut cells: Vec<(u32, u32, LiteralValue)> = Vec::new();
    values.read_sheet(&mut zip, &part, |r, c, v| {
        if r >= first_row && r <= last_row && wanted.contains(&c) {
            cells.push((r, c, v));
        }
    });
    drop(zip);
    cells.sort_by_key(|&(r, c, _)| (r, c));

    // Group by row band, then fold each group on its own.
    let mut bands: Vec<(u32, u32)> = idx.iter().map(|&i| (keys[i].rects[0].0, keys[i].rects[0].2)).collect();
    bands.sort_unstable();
    bands.dedup();

    let mut chunks = 0usize;
    for (r0, r1) in bands {
        let group: Vec<usize> = idx
            .iter()
            .copied()
            .filter(|&i| (keys[i].rects[0].0, keys[i].rects[0].2) == (r0, r1))
            .collect();
        chunks += fold_band(&cells, &col_map, keys, &group, r0, r1, states)?;
    }
    Ok(chunks)
}

/// Fold one group of aggregates over the row band they share.
///
/// The band is walked in chunks of `CHUNK_ROWS`. A band that does not divide
/// evenly leaves a shorter last chunk, and that chunk gets its own workbook
/// whose probes are cut to the exact height. Reusing the full-height probes
/// there would let a criterion such as `COUNTIF(range,"")` match the blank
/// rows that no source row stands for.
fn fold_band(
    cells: &[(u32, u32, LiteralValue)],
    col_map: &HashMap<u32, u32>,
    keys: &[Key],
    group: &[usize],
    r0: u32,
    r1: u32,
    states: &mut [State],
) -> Result<usize, ()> {
    let height = r1 - r0 + 1;
    let full = height / CHUNK_ROWS;
    let tail = height % CHUNK_ROWS;

    let mut chunks = 0usize;
    let mut start = r0;
    if full > 0 {
        let mut wb = build_probes(col_map, keys, group, CHUNK_ROWS)?;
        for _ in 0..full {
            let end = start + CHUNK_ROWS - 1;
            fold_chunk(&mut wb, cells, col_map, keys, group, start, end, CHUNK_ROWS, states)?;
            chunks += 1;
            start = end + 1;
        }
    }
    if tail > 0 {
        let mut wb = build_probes(col_map, keys, group, tail)?;
        fold_chunk(&mut wb, cells, col_map, keys, group, start, r1, tail, states)?;
        chunks += 1;
    }
    Ok(chunks)
}

/// One scratch sheet holding the probe formulas of a group, cut to `height`.
struct Probes {
    wb: Workbook,
    /// Probe formulas per aggregate, in the order of the group.
    per_key: Vec<usize>,
    row: u32,
    /// Scratch cells written by the chunk before, so they can be cleared.
    written: Vec<(u32, u32)>,
}

fn build_probes(
    col_map: &HashMap<u32, u32>,
    keys: &[Key],
    group: &[usize],
    height: u32,
) -> Result<Probes, ()> {
    let mut wb = Workbook::new_with_config(crate::clock::pin(WorkbookConfig::ephemeral()));
    wb.add_sheet("P").map_err(|_| ())?;
    // The probes sit under the data area, so they never read their own row.
    let row = height + 2;
    let mut per_key = Vec::with_capacity(group.len());
    let mut col = 1u32;
    for &i in group {
        let texts = probes(&keys[i], col_map, height);
        per_key.push(texts.len());
        for text in texts {
            let ast = parse(&text).map_err(|_| ())?;
            wb.engine_mut()
                .set_cell_formula("P", row, col, ast)
                .map_err(|_| ())?;
            col += 1;
        }
    }
    Ok(Probes { wb, per_key, row, written: Vec::new() })
}

/// Load one chunk of source rows, evaluate, and add the results to the totals.
#[allow(clippy::too_many_arguments)]
fn fold_chunk(
    probes_wb: &mut Probes,
    cells: &[(u32, u32, LiteralValue)],
    col_map: &HashMap<u32, u32>,
    keys: &[Key],
    group: &[usize],
    start: u32,
    end: u32,
    height: u32,
    states: &mut [State],
) -> Result<(), ()> {
    debug_assert_eq!(end - start + 1, height);

    // Clearing what the chunk before wrote, and then writing this chunk, leaves
    // exactly this chunk's values. Walking the whole rectangle instead would
    // cost a write for every blank cell of a sparse sheet.
    for &(r, c) in &probes_wb.written {
        probes_wb.wb.set_value("P", r, c, LiteralValue::Empty).map_err(|_| ())?;
    }
    probes_wb.written.clear();

    let lo = cells.partition_point(|&(r, _, _)| r < start);
    let hi = cells.partition_point(|&(r, _, _)| r <= end);
    for (r, c, v) in &cells[lo..hi] {
        let target = (r - start + 1, col_map[c]);
        probes_wb
            .wb
            .set_value("P", target.0, target.1, v.clone())
            .map_err(|_| ())?;
        probes_wb.written.push(target);
    }

    probes_wb.wb.evaluate_all().map_err(|_| ())?;

    let mut col = 1u32;
    for (slot, &i) in group.iter().enumerate() {
        let n = probes_wb.per_key[slot];
        let mut partial: Vec<LiteralValue> = Vec::with_capacity(n);
        for _ in 0..n {
            partial.push(
                probes_wb
                    .wb
                    .get_value("P", probes_wb.row, col)
                    .unwrap_or(LiteralValue::Empty),
            );
            col += 1;
        }
        fold(kind_of(keys[i].kind_tag), &partial, &mut states[i]);
    }
    Ok(())
}

/// The chunk-local formulas one aggregate needs.
///
/// `MIN` and `MAX` carry a `COUNT` beside them because an empty chunk gives 0,
/// which must not be folded as a real minimum. `AVERAGE` carries `SUM` and
/// `COUNT` because only additive parts survive chunking.
fn probes(key: &Key, col_map: &HashMap<u32, u32>, height: u32) -> Vec<String> {
    let a = rect_text(key.rects[0], col_map, height);
    let b = key.rects.get(1).map(|&r| rect_text(r, col_map, height));
    let crit = key.criterion.clone().unwrap_or_default();
    match kind_of(key.kind_tag) {
        Kind::Sum => vec![format!("=SUM({a})")],
        Kind::Count => vec![format!("=COUNT({a})")],
        Kind::CountA => vec![format!("=COUNTA({a})")],
        Kind::CountIf => vec![format!("=COUNTIF({a},{crit})")],
        Kind::SumIf => match b {
            Some(b) => vec![format!("=SUMIF({a},{crit},{b})")],
            None => vec![format!("=SUMIF({a},{crit})")],
        },
        Kind::Average => vec![format!("=SUM({a})"), format!("=COUNT({a})")],
        Kind::Min => vec![format!("=MIN({a})"), format!("=COUNT({a})")],
        Kind::Max => vec![format!("=MAX({a})"), format!("=COUNT({a})")],
    }
}

/// Render a source rectangle as the chunk-local rectangle it maps to.
///
/// The columns keep their identity through the dense map. The rows become
/// 1 to `height`, because the chunk loads that many source rows of the band the
/// rectangle covers.
fn rect_text((_, c0, _, c1): Rect, col_map: &HashMap<u32, u32>, height: u32) -> String {
    let a = col_name(col_map[&c0]);
    let b = col_name(col_map[&c1]);
    format!("${a}$1:${b}${height}")
}

fn col_name(mut col: u32) -> String {
    let mut out = Vec::new();
    while col > 0 {
        let rem = (col - 1) % 26;
        out.push(b'A' + rem as u8);
        col = (col - 1) / 26;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// Add one chunk's partial results to the running totals.
fn fold(kind: Kind, partial: &[LiteralValue], state: &mut State) {
    if state.failed {
        return;
    }
    let mut nums: Vec<f64> = Vec::with_capacity(partial.len());
    for v in partial {
        match v {
            LiteralValue::Number(n) if n.is_finite() => nums.push(*n),
            LiteralValue::Int(i) => nums.push(*i as f64),
            LiteralValue::Error(e) => {
                if state.error.is_none() {
                    state.error = Some(e.clone());
                }
                return;
            }
            // A date, a text or an infinity cannot be folded as a number, so
            // the aggregate is dropped and its formula stays unchanged.
            _ => {
                state.failed = true;
                return;
            }
        }
    }
    match kind {
        Kind::Sum | Kind::SumIf => state.sum += nums[0],
        Kind::Count | Kind::CountA | Kind::CountIf => state.count += nums[0],
        Kind::Average => {
            state.sum += nums[0];
            state.count += nums[1];
        }
        Kind::Min => {
            if nums[1] > 0.0 {
                state.min = Some(state.min.map_or(nums[0], |m| m.min(nums[0])));
            }
            state.count += nums[1];
        }
        Kind::Max => {
            if nums[1] > 0.0 {
                state.max = Some(state.max.map_or(nums[0], |m| m.max(nums[0])));
            }
            state.count += nums[1];
        }
    }
}

/// Turn the running totals into the value the aggregate would have produced.
fn finish(kind: Kind, state: &State) -> Option<LiteralValue> {
    if state.failed {
        return None;
    }
    if let Some(e) = &state.error {
        return Some(LiteralValue::Error(e.clone()));
    }
    let n = match kind {
        Kind::Sum | Kind::SumIf => state.sum,
        Kind::Count | Kind::CountA | Kind::CountIf => state.count,
        // Excel gives #DIV/0! when nothing is averaged.
        Kind::Average => {
            if state.count == 0.0 {
                return Some(LiteralValue::Error(ExcelError::new(ExcelErrorKind::Div)));
            }
            state.sum / state.count
        }
        // Excel gives 0 when nothing is compared.
        Kind::Min => state.min.unwrap_or(0.0),
        Kind::Max => state.max.unwrap_or(0.0),
    };
    if n.is_finite() {
        Some(LiteralValue::Number(n))
    } else {
        None
    }
}

/// Print a rewritten formula and prove that it reads back unchanged.
///
/// The printer that ships with the parser renders a boolean in lower case and
/// an empty value as nothing, so the literals are rendered here instead. The
/// result is parsed again and compared against the tree it came from. A formula
/// that fails that check keeps its original text.
fn render_and_verify(ast: &ASTNode) -> Option<String> {
    // The stored text always carries the leading '='. Without it the parser
    // reads the whole string as a literal instead of a formula.
    let text = format!("={}", render(ast)?);
    let back = parse(&text).ok()?;
    if same_value(ast, &back) {
        Some(text)
    } else {
        None
    }
}

fn render(node: &ASTNode) -> Option<String> {
    match &node.node_type {
        ASTNodeType::Literal(v) => render_literal(v),
        ASTNodeType::Omitted => Some(String::new()),
        ASTNodeType::Reference { reference, .. } => Some(reference.normalise()),
        ASTNodeType::UnaryOp { op, expr } => {
            let inner = render(expr)?;
            if op == "%" || op == "#" {
                Some(format!("({inner}){op}"))
            } else {
                Some(format!("{op}({inner})"))
            }
        }
        ASTNodeType::BinaryOp { op, left, right } => {
            let l = render(left)?;
            let r = render(right)?;
            match op.as_str() {
                // A range or an intersection cannot take parentheses around its
                // sides without changing what it means.
                ":" => Some(format!("{l}:{r}")),
                " " => Some(format!("{l} {r}")),
                "," => Some(format!("{l},{r}")),
                _ => Some(format!("({l}){op}({r})")),
            }
        }
        ASTNodeType::Function { name, args } => {
            let mut parts = Vec::with_capacity(args.len());
            for a in args {
                parts.push(render(a)?);
            }
            Some(format!("{}({})", name.to_uppercase(), parts.join(",")))
        }
        ASTNodeType::Call { callee, args } => {
            let mut parts = Vec::with_capacity(args.len());
            for a in args {
                parts.push(render(a)?);
            }
            Some(format!("({})({})", render(callee)?, parts.join(",")))
        }
        ASTNodeType::Array(rows) => {
            let mut out_rows = Vec::with_capacity(rows.len());
            for row in rows {
                let mut parts = Vec::with_capacity(row.len());
                for a in row {
                    parts.push(render(a)?);
                }
                out_rows.push(parts.join(","));
            }
            Some(format!("{{{}}}", out_rows.join(";")))
        }
    }
}

/// Render one literal so that the parser reads it back as the same value.
///
/// A negative number is wrapped in parentheses. Without them `-5^2` would read
/// back as the negation of `5^2`, which is a different number. Only the value
/// kinds a fold can produce are rendered; anything else stops the rewrite.
fn render_literal(v: &LiteralValue) -> Option<String> {
    match v {
        LiteralValue::Int(i) => {
            Some(if *i < 0 { format!("({i})") } else { i.to_string() })
        }
        LiteralValue::Number(n) if n.is_finite() => {
            Some(if *n < 0.0 { format!("({n})") } else { n.to_string() })
        }
        LiteralValue::Boolean(b) => Some(if *b { "TRUE".into() } else { "FALSE".into() }),
        LiteralValue::Text(s) => Some(format!("\"{}\"", s.replace('"', "\"\""))),
        LiteralValue::Error(e) => Some(e.kind.to_string()),
        _ => None,
    }
}

/// Compare a rewritten tree with the tree the printed text parses back to.
///
/// The two are not identical trees. A negative literal is printed with
/// parentheses and reads back as a negation, and an integer reads back as a
/// number, so those forms are treated as equal.
fn same_value(a: &ASTNode, b: &ASTNode) -> bool {
    if let (ASTNodeType::Literal(x), _) = (&a.node_type, &b.node_type) {
        return literal_reads_back(x, b);
    }
    match (&a.node_type, &b.node_type) {
        (ASTNodeType::Omitted, ASTNodeType::Omitted) => true,
        (
            ASTNodeType::Reference { reference: x, .. },
            ASTNodeType::Reference { reference: y, .. },
        ) => x == y,
        (ASTNodeType::UnaryOp { op: xo, expr: xe }, ASTNodeType::UnaryOp { op: yo, expr: ye }) => {
            xo == yo && same_value(xe, ye)
        }
        (
            ASTNodeType::BinaryOp { op: xo, left: xl, right: xr },
            ASTNodeType::BinaryOp { op: yo, left: yl, right: yr },
        ) => xo == yo && same_value(xl, yl) && same_value(xr, yr),
        (
            ASTNodeType::Function { name: xn, args: xa },
            ASTNodeType::Function { name: yn, args: ya },
        ) => {
            xn.eq_ignore_ascii_case(yn)
                && xa.len() == ya.len()
                && xa.iter().zip(ya).all(|(p, q)| same_value(p, q))
        }
        (
            ASTNodeType::Call { callee: xc, args: xa },
            ASTNodeType::Call { callee: yc, args: ya },
        ) => {
            same_value(xc, yc)
                && xa.len() == ya.len()
                && xa.iter().zip(ya).all(|(p, q)| same_value(p, q))
        }
        (ASTNodeType::Array(x), ASTNodeType::Array(y)) => {
            x.len() == y.len()
                && x.iter().zip(y).all(|(rx, ry)| {
                    rx.len() == ry.len() && rx.iter().zip(ry).all(|(p, q)| same_value(p, q))
                })
        }
        _ => false,
    }
}

/// Does `node` hold exactly the value `want`, in either of the two forms the
/// parser can give it?
fn literal_reads_back(want: &LiteralValue, node: &ASTNode) -> bool {
    if let ASTNodeType::Literal(got) = &node.node_type {
        return literal_eq(want, got);
    }
    // A negative number reads back as a negation of its magnitude.
    if let ASTNodeType::UnaryOp { op, expr } = &node.node_type {
        if op == "-" {
            if let ASTNodeType::Literal(got) = &expr.node_type {
                return match (want, got) {
                    (LiteralValue::Int(i), LiteralValue::Number(n)) => *i as f64 == -*n,
                    (LiteralValue::Number(x), LiteralValue::Number(n)) => *x == -*n,
                    _ => false,
                };
            }
        }
    }
    false
}

/// Compare two literals, treating an integer and the same whole number as one
/// value. The parser has no integer literal, so a folded `COUNT` of 5 reads
/// back as the number 5.
fn literal_eq(a: &LiteralValue, b: &LiteralValue) -> bool {
    match (a, b) {
        (LiteralValue::Int(x), LiteralValue::Number(y)) => *x as f64 == *y,
        (LiteralValue::Number(x), LiteralValue::Int(y)) => *x == *y as f64,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph;
    use crate::testkit::{cell_f, cell_v, xlsx};

    /// `rows` rows of data in column A, and a per-row formula in column B that
    /// divides by an aggregate over the whole of column A.
    ///
    /// The aggregate makes every row read the whole column, so the sheet is one
    /// component until the prelude folds the aggregate into a number.
    fn share_of_total(rows: u32, aggregate: &str) -> Vec<u8> {
        let mut sheet = String::new();
        for r in 1..=rows {
            sheet.push_str(&cell_v(&format!("A{r}"), &r.to_string()));
            sheet.push_str(&cell_f(&format!("B{r}"), &format!("A{r}/{aggregate}")));
        }
        xlsx(&[("S", &sheet)])
    }

    fn fold_once(data: &[u8]) -> (graph::Sources, Report) {
        let mut src = graph::read(data);
        let report = fold_and_rewrite(data, &mut src);
        (src, report)
    }

    /// The values 1 to 40 add up to 820. The rewritten formula must hold that
    /// number, and it must no longer name a column.
    #[test]
    fn a_whole_column_sum_becomes_a_number() {
        let (src, report) = fold_once(&share_of_total(40, "SUM(A:A)"));
        assert_eq!(report.folded, 1);
        assert_eq!(report.rewritten, 1);
        assert!(
            src.texts.iter().all(|t| !t.contains("A:A")),
            "the column reference must be gone: {:?}",
            src.texts
        );
        assert!(
            src.texts.iter().any(|t| t.contains("820")),
            "the folded total must appear: {:?}",
            src.texts
        );
    }

    /// The point of the fold. The rows are already separate components, because
    /// a data-only range never joins two formulas. What blocks the split is the
    /// extent: every row reads the whole of column A, so every bounding box
    /// covers the sheet and the largest one is the whole sheet.
    #[test]
    fn folding_shrinks_every_box_to_its_own_row() {
        let data = share_of_total(40, "SUM(A:A)");

        let before = graph::build_from(graph::read(&data));
        assert_eq!(
            before.biggest_extent_cells(),
            before.full_extent_cells,
            "the column read makes one box cover the sheet"
        );

        let mut src = graph::read(&data);
        fold_and_rewrite(&data, &mut src);
        let after = graph::build_from(src);
        assert_eq!(after.comp_cells.len(), 40, "one component per row");
        assert_eq!(
            after.biggest_extent_cells(),
            2,
            "a box now spans A..B on one row"
        );
    }

    /// More rows than one chunk holds, so the fold has to add chunk totals.
    /// The values 1 to 1200 add up to 720600.
    #[test]
    fn a_fold_over_many_chunks_adds_the_same_total() {
        let rows = CHUNK_ROWS * 2 + 200;
        let (src, report) = fold_once(&share_of_total(rows, "SUM(A:A)"));
        assert_eq!(report.chunks, 3, "1200 rows need three chunks of 500");
        assert_eq!(report.folded, 1);
        let want = (rows as u64 * (rows as u64 + 1) / 2).to_string();
        assert!(
            src.texts.iter().any(|t| t.contains(&want)),
            "expected the total {want}: {:?}",
            src.texts
        );
    }

    /// A stale value from the previous chunk must not be folded again. The last
    /// chunk is shorter than the scratch sheet, so its unused rows must be
    /// cleared before the fold reads them.
    #[test]
    fn a_short_last_chunk_does_not_fold_stale_rows() {
        let rows = CHUNK_ROWS + 3;
        let (src, _) = fold_once(&share_of_total(rows, "SUM(A:A)"));
        let want = (rows as u64 * (rows as u64 + 1) / 2).to_string();
        assert!(
            src.texts.iter().any(|t| t.contains(&want)),
            "expected the total {want}: {:?}",
            src.texts
        );
    }

    /// Each accepted aggregate folds to the value the whole column would give.
    #[test]
    fn every_accepted_aggregate_folds_to_its_own_value() {
        // Column A holds 1 to 10: sum 55, count 10, average 5.5, min 1, max 10.
        // Five of those values are above 5, and those five add up to 40.
        for (formula, want) in [
            ("SUM(A:A)", "55"),
            ("COUNT(A:A)", "10"),
            ("COUNTA(A:A)", "10"),
            ("AVERAGE(A:A)", "5.5"),
            ("MIN(A:A)", "1"),
            ("MAX(A:A)", "10"),
            ("COUNTIF(A:A,\">5\")", "5"),
            ("SUMIF(A:A,\">5\")", "40"),
        ] {
            let (src, report) = fold_once(&share_of_total(10, formula));
            assert_eq!(report.folded, 1, "{formula} must fold");
            assert!(
                src.texts.iter().any(|t| t.contains(want)),
                "{formula} must fold to {want}: {:?}",
                src.texts
            );
        }
    }

    /// An aggregate that needs every value at once cannot be chunked, so its
    /// formula is left unchanged.
    #[test]
    fn an_aggregate_that_does_not_fold_is_left_alone() {
        for formula in ["MEDIAN(A:A)", "STDEV(A:A)", "LARGE(A:A,2)"] {
            let (src, report) = fold_once(&share_of_total(10, formula));
            assert_eq!(report.folded, 0, "{formula} must not fold");
            assert!(
                src.texts.iter().any(|t| t.contains("A:A")),
                "{formula} must keep its column reference"
            );
        }
    }

    /// A range that holds a formula has no value before the graph is built, so
    /// the aggregate over it cannot be folded ahead of time.
    #[test]
    fn an_aggregate_over_formulas_is_left_alone() {
        let mut sheet = String::new();
        for r in 1..=10 {
            sheet.push_str(&cell_v(&format!("A{r}"), &r.to_string()));
            sheet.push_str(&cell_f(&format!("B{r}"), &format!("A{r}*2")));
        }
        sheet.push_str(&cell_f("C1", "SUM(B:B)"));
        let data = xlsx(&[("S", &sheet)]);
        let (src, report) = fold_once(&data);
        assert_eq!(report.folded, 0, "column B holds formulas, not data");
        assert!(src.texts.iter().any(|t| t.contains("B:B")));
    }

    /// A relative range moves with the cell that holds it, so one number cannot
    /// stand for every member of the template.
    #[test]
    fn a_range_that_moves_with_the_row_is_left_alone() {
        let mut sheet = String::new();
        for r in 1..=6 {
            sheet.push_str(&cell_v(&format!("A{r}"), &r.to_string()));
            // A2:A4 from row 5, A3:A5 from row 6, and so on.
            sheet.push_str(&cell_f(&format!("C{r}"), &format!("SUM(A{r}:A{})", r + 2)));
        }
        let (_, report) = fold_once(&xlsx(&[("S", &sheet)]));
        assert_eq!(report.folded, 0);
    }

    /// The same range pinned with `$` does not move, so it folds.
    #[test]
    fn a_pinned_range_folds_even_when_the_row_moves() {
        let mut sheet = String::new();
        for r in 1..=6 {
            sheet.push_str(&cell_v(&format!("A{r}"), &r.to_string()));
            sheet.push_str(&cell_f(&format!("C{r}"), "SUM($A$1:$A$6)"));
        }
        let (src, report) = fold_once(&xlsx(&[("S", &sheet)]));
        assert_eq!(report.folded, 1);
        assert!(src.texts.iter().any(|t| t.contains("21")), "{:?}", src.texts);
    }

    /// A file with no aggregates must come back untouched.
    #[test]
    fn a_file_without_aggregates_is_not_changed() {
        let mut sheet = String::new();
        for r in 1..=10 {
            sheet.push_str(&cell_v(&format!("A{r}"), &r.to_string()));
            sheet.push_str(&cell_f(&format!("B{r}"), &format!("A{r}*2")));
        }
        let data = xlsx(&[("S", &sheet)]);
        let before = graph::read(&data);
        let (after, report) = fold_once(&data);
        assert_eq!(report.folded, 0);
        assert_eq!(report.chunks, 0);
        assert_eq!(before.texts, after.texts);
    }

    /// A bounded range must fold over its own rows only.
    ///
    /// The probe formula is placed once and reused, so it must read the rows
    /// the source range covers and no others. A `SUM` over four rows must not
    /// add every row of the chunk.
    #[test]
    fn a_bounded_range_folds_over_its_own_rows_only() {
        let mut sheet = String::new();
        for r in 1..=20 {
            sheet.push_str(&cell_v(&format!("A{r}"), &r.to_string()));
        }
        // Rows 2 to 5 hold 2, 3, 4 and 5, which add up to 14.
        sheet.push_str(&cell_f("C1", "SUM(A2:A5)*1"));
        let (src, report) = fold_once(&xlsx(&[("S", &sheet)]));
        assert_eq!(report.folded, 1);
        assert!(
            src.texts.iter().any(|t| t.contains("14")),
            "expected 14, not the total of the sheet: {:?}",
            src.texts
        );
    }

    /// Two aggregates over different bands of one sheet must not share a probe.
    #[test]
    fn two_row_bands_fold_apart() {
        let mut sheet = String::new();
        for r in 1..=20 {
            sheet.push_str(&cell_v(&format!("A{r}"), &r.to_string()));
        }
        // Rows 1 to 4 add up to 10; rows 10 to 12 add up to 33.
        sheet.push_str(&cell_f("C1", "SUM(A1:A4)*1"));
        sheet.push_str(&cell_f("D1", "SUM(A10:A12)*1"));
        let (src, report) = fold_once(&xlsx(&[("S", &sheet)]));
        assert_eq!(report.folded, 2);
        let joined = src.texts.join(" ");
        assert!(joined.contains("10"), "first band: {joined}");
        assert!(joined.contains("33"), "second band: {joined}");
    }

    /// A criterion that matches a blank must not count the unused rows of a
    /// short chunk. The probes of a short chunk are cut to its exact height, so
    /// every row the probe reads stands for a real source row.
    #[test]
    fn a_criterion_that_matches_a_blank_counts_only_real_rows() {
        let mut sheet = String::new();
        // Six rows, of which two hold nothing in column A.
        for r in 1..=6 {
            if r != 3 && r != 5 {
                sheet.push_str(&cell_v(&format!("A{r}"), &r.to_string()));
            }
            sheet.push_str(&cell_v(&format!("B{r}"), "1"));
        }
        sheet.push_str(&cell_f("D1", "COUNTIF(A1:A6,\"\")*1"));
        let (src, report) = fold_once(&xlsx(&[("S", &sheet)]));
        assert_eq!(report.folded, 1);
        let joined = src.texts.join(" ");
        let count: f64 = joined
            .trim_start_matches("=(")
            .split(')')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(f64::NAN);
        assert!(
            count <= 6.0,
            "the range holds six cells, so no count above six can be right: {joined}"
        );
    }

    #[test]
    fn column_names_follow_the_a1_scheme() {
        assert_eq!(col_name(1), "A");
        assert_eq!(col_name(26), "Z");
        assert_eq!(col_name(27), "AA");
        assert_eq!(col_name(52), "AZ");
        assert_eq!(col_name(53), "BA");
    }

    #[test]
    fn literals_read_back_as_the_value_they_came_from() {
        for v in [
            LiteralValue::Number(24941234.5),
            LiteralValue::Number(-3.25),
            LiteralValue::Number(0.1),
            LiteralValue::Int(4821),
            LiteralValue::Int(-7),
            LiteralValue::Boolean(true),
            LiteralValue::Boolean(false),
            LiteralValue::Text("a\"b".into()),
            LiteralValue::Error(ExcelError::new(ExcelErrorKind::Div)),
            LiteralValue::Error(ExcelError::new(ExcelErrorKind::Na)),
        ] {
            let text = render_literal(&v).expect("value must render");
            let back = parse(&format!("={text}")).expect("rendered text must parse");
            assert!(
                literal_reads_back(&v, &back),
                "{v:?} rendered as {text} did not read back"
            );
        }
    }

    #[test]
    fn values_a_fold_cannot_produce_are_refused() {
        assert!(render_literal(&LiteralValue::Empty).is_none());
        assert!(render_literal(&LiteralValue::Number(f64::NAN)).is_none());
        assert!(render_literal(&LiteralValue::Number(f64::INFINITY)).is_none());
    }

    /// A negative literal must keep its sign when a power is taken of it.
    #[test]
    fn a_negative_literal_keeps_its_own_sign_under_a_power() {
        let ast = parse("=SUM(A:A)^2").unwrap();
        let mut hits = 0;
        // Substitute by hand, because no workbook is loaded here.
        let ASTNodeType::BinaryOp { op, right, .. } = &ast.node_type else {
            panic!("expected a power");
        };
        let rebuilt = ASTNode::new(
            ASTNodeType::BinaryOp {
                op: op.clone(),
                left: Box::new(ASTNode::new(
                    ASTNodeType::Literal(LiteralValue::Number(-5.0)),
                    None,
                )),
                right: right.clone(),
            },
            None,
        );
        hits += 1;
        assert_eq!(hits, 1);
        let text = render_and_verify(&rebuilt).expect("must render and verify");
        assert_eq!(text, "=((-5))^(2)");
    }

    #[test]
    fn min_of_an_empty_range_is_zero_and_average_of_one_is_an_error() {
        let empty = State::default();
        assert_eq!(finish(Kind::Min, &empty), Some(LiteralValue::Number(0.0)));
        assert_eq!(finish(Kind::Max, &empty), Some(LiteralValue::Number(0.0)));
        assert!(matches!(
            finish(Kind::Average, &empty),
            Some(LiteralValue::Error(_))
        ));
        assert_eq!(finish(Kind::Sum, &empty), Some(LiteralValue::Number(0.0)));
    }

    /// A chunk that holds no number must not pull the minimum down to zero.
    #[test]
    fn an_empty_chunk_does_not_change_a_minimum() {
        let mut state = State::default();
        fold(
            Kind::Min,
            &[LiteralValue::Number(7.0), LiteralValue::Number(3.0)],
            &mut state,
        );
        fold(
            Kind::Min,
            &[LiteralValue::Number(0.0), LiteralValue::Number(0.0)],
            &mut state,
        );
        assert_eq!(finish(Kind::Min, &state), Some(LiteralValue::Number(7.0)));
    }

    #[test]
    fn an_error_in_one_chunk_becomes_the_folded_value() {
        let mut state = State::default();
        fold(Kind::Sum, &[LiteralValue::Number(5.0)], &mut state);
        fold(
            Kind::Sum,
            &[LiteralValue::Error(ExcelError::new(ExcelErrorKind::Div))],
            &mut state,
        );
        fold(Kind::Sum, &[LiteralValue::Number(9.0)], &mut state);
        assert!(matches!(
            finish(Kind::Sum, &state),
            Some(LiteralValue::Error(_))
        ));
    }

    /// A value the fold cannot use stops that aggregate, and the formula that
    /// holds it is then left unchanged.
    #[test]
    fn a_value_the_fold_cannot_use_drops_the_aggregate() {
        let mut state = State::default();
        fold(Kind::Sum, &[LiteralValue::Text("x".into())], &mut state);
        assert!(finish(Kind::Sum, &state).is_none());
    }

    #[test]
    fn average_folds_from_a_sum_and_a_count() {
        let mut state = State::default();
        fold(
            Kind::Average,
            &[LiteralValue::Number(10.0), LiteralValue::Number(4.0)],
            &mut state,
        );
        fold(
            Kind::Average,
            &[LiteralValue::Number(20.0), LiteralValue::Number(1.0)],
            &mut state,
        );
        assert_eq!(finish(Kind::Average, &state), Some(LiteralValue::Number(6.0)));
    }
}
