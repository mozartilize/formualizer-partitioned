//! Does a workbook-scoped defined name bind correctly under bulk ingest?
//!
//! A batch workbook defines the names its formulas use, then places the
//! formulas with bulk ingest. This checks whether a formula reading such a
//! name gets the name's value or an empty operand, for the shape the TI
//! calculator uses: the name targets a formula cell on another sheet.
//!
//! Run: cargo run --release --manifest-path probe/Cargo.toml --bin nameprobe

use formualizer::common::value::LiteralValue;
use formualizer::common::RangeAddress;
use formualizer::parse::parser::parse;
use formualizer::workbook::{NamedRangeScope, Workbook, WorkbookConfig};

/// `bulk`   - place formulas through bulk ingest instead of `set_cell_formula`.
/// `name_first` - define the name before placing formulas.
fn try_case(bulk: bool, name_first: bool) -> String {
    let mut wb = Workbook::new_with_config(WorkbookConfig::ephemeral());
    wb.add_sheet("Design Calculator").unwrap();
    wb.add_sheet("Equations").unwrap();

    // Equations!E15 = 45 (data), Equations!F24 = E15*0.5 (formula, the target)
    wb.set_value("Equations", 15, 5, LiteralValue::Number(45.0))
        .unwrap();

    let define = |wb: &mut Workbook| {
        let addr = RangeAddress::new("Equations".to_string(), 24, 6, 24, 6).unwrap();
        wb.define_named_range("CLMIN", &addr, NamedRangeScope::Workbook)
            .unwrap();
    };
    if name_first {
        define(&mut wb);
    }

    // Equations!F24 = E15*0.5   ->  22.5
    // Design Calculator!F46 = CLMIN  -> should also be 22.5
    let eq: Vec<(u32, u32, _)> = vec![(24, 6, parse("=E15*0.5").unwrap())];
    let dc: Vec<(u32, u32, _)> = vec![(46, 6, parse("=CLMIN").unwrap())];

    if bulk {
        let mut b = wb.engine_mut().begin_bulk_ingest();
        // Same order place_formulas uses: sheets in workbook order.
        let dc_id = b.add_sheet("Design Calculator");
        b.add_formulas(dc_id, dc);
        let eq_id = b.add_sheet("Equations");
        b.add_formulas(eq_id, eq);
        b.finish().unwrap();
    } else {
        for (r, c, ast) in eq {
            wb.engine_mut()
                .set_cell_formula("Equations", r, c, ast)
                .unwrap();
        }
        for (r, c, ast) in dc {
            wb.engine_mut()
                .set_cell_formula("Design Calculator", r, c, ast)
                .unwrap();
        }
    }
    if !name_first {
        define(&mut wb);
    }
    wb.evaluate_all().unwrap();

    format!(
        "Equations!F24={:<28} DesignCalculator!F46={:?}",
        format!("{:?}", wb.get_value("Equations", 24, 6)),
        wb.get_value("Design Calculator", 46, 6)
    )
}

fn main() {
    println!("expected: both 22.5\n");
    for (bulk, first) in [(false, true), (true, true), (false, false), (true, false)] {
        println!(
            "{:>6} ingest, name {:>6} -> {}",
            if bulk { "bulk" } else { "single" },
            if first { "first" } else { "last" },
            try_case(bulk, first)
        );
    }
}
