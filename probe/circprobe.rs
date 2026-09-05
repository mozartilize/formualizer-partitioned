//! Check whether an unbounded column range triggers a false circular error
//! under bulk ingest, and whether clamping the range to the used rows avoids it.
//!
//! Run: cargo run --release --no-default-features --example circprobe

use formualizer::common::value::LiteralValue;
use formualizer::parse::parser::parse;
use formualizer::workbook::{Workbook, WorkbookConfig};

fn try_case(range: &str, bulk: bool) -> String {
    let mut wb = Workbook::new_with_config(WorkbookConfig::ephemeral());
    wb.add_sheet("Sheet1").unwrap();
    for r in 1..=6u32 {
        wb.set_value("Sheet1", r, 1, LiteralValue::Number(r as f64))
            .unwrap();
        wb.set_value("Sheet1", r, 2, LiteralValue::Text(format!("c{r}")))
            .unwrap();
        wb.set_value("Sheet1", r, 4, LiteralValue::Number(r as f64))
            .unwrap();
    }
    let asts: Vec<(u32, u32, _)> = (1..=6u32)
        .map(|r| {
            let text = format!("=VLOOKUP(D{r},{range},2,0)");
            (r, 5, parse(&text).unwrap())
        })
        .collect();

    if bulk {
        let mut b = wb.engine_mut().begin_bulk_ingest();
        let sid = b.add_sheet("Sheet1");
        b.add_formulas(sid, asts);
        b.finish().unwrap();
    } else {
        for (r, c, ast) in asts {
            wb.engine_mut()
                .set_cell_formula("Sheet1", r, c, ast)
                .unwrap();
        }
    }
    wb.evaluate_all().unwrap();
    format!("{:?}", wb.get_value("Sheet1", 2, 5))
}

fn main() {
    for bulk in [false, true] {
        let how = if bulk { "bulk  " } else { "single" };
        for range in ["A:B", "A1:B6", "A1:B1048576"] {
            println!("{how} {range:>12}  ->  {}", try_case(range, bulk));
        }
    }
}
