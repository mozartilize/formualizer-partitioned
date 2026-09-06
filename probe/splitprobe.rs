//! Check whether a formula ingested in one bulk call can read a formula
//! ingested in an earlier bulk call on the same workbook.
//!
//! Run: cargo run --release --manifest-path probe/Cargo.toml --bin splitprobe

use formualizer::common::value::LiteralValue;
use formualizer::parse::parser::parse;
use formualizer::workbook::{Workbook, WorkbookConfig};

fn run(split: bool) -> String {
    let mut wb = Workbook::new_with_config(WorkbookConfig::ephemeral());
    wb.add_sheet("Sheet1").unwrap();
    wb.set_value("Sheet1", 1, 1, LiteralValue::Number(1.0)).unwrap();

    // B1 = A1 + 1, C1 = B1 + 1. C1 reads a formula, not a value.
    let first = (1u32, 2u32, parse("=A1+1").unwrap());
    let second = (1u32, 3u32, parse("=B1+1").unwrap());

    if split {
        let mut b = wb.engine_mut().begin_bulk_ingest();
        let s = b.add_sheet("Sheet1");
        b.add_formulas(s, vec![first]);
        b.finish().unwrap();

        let mut b = wb.engine_mut().begin_bulk_ingest();
        let s = b.add_sheet("Sheet1");
        b.add_formulas(s, vec![second]);
        b.finish().unwrap();
    } else {
        let mut b = wb.engine_mut().begin_bulk_ingest();
        let s = b.add_sheet("Sheet1");
        b.add_formulas(s, vec![first, second]);
        b.finish().unwrap();
    }
    wb.evaluate_all().unwrap();
    format!(
        "B1={:?} C1={:?}",
        wb.get_value("Sheet1", 1, 2),
        wb.get_value("Sheet1", 1, 3)
    )
}

fn main() {
    println!("one call        {}", run(false));
    println!("two calls       {}   (expected B1=2, C1=3)", run(true));
    println!("reversed order  {}   (expected B1=2, C1=3)", run_reversed());
    println!("chain split     {}", chain(5_000, 2_048));
    println!("chain one call  {}", chain(5_000, 100_000));
}

/// A1=1 and every row below reads the row above. The chain is ingested in
/// calls of `per_call` formulas.
fn chain(rows: u32, per_call: usize) -> String {
    let mut wb = Workbook::new_with_config(WorkbookConfig::ephemeral());
    wb.add_sheet("Sheet1").unwrap();
    wb.set_value("Sheet1", 1, 1, LiteralValue::Number(1.0)).unwrap();

    let mut staged: Vec<(u32, u32, _)> = Vec::new();
    for r in 2..=rows {
        staged.push((r, 1, parse(&format!("=A{}+1", r - 1)).unwrap()));
    }
    while !staged.is_empty() {
        let take = per_call.min(staged.len());
        let chunk: Vec<_> = staged.drain(..take).collect();
        let mut b = wb.engine_mut().begin_bulk_ingest();
        let s = b.add_sheet("Sheet1");
        b.add_formulas(s, chunk);
        b.finish().unwrap();
    }
    wb.evaluate_all().unwrap();
    format!(
        "A2={:?} A2048={:?} A2049={:?} A{rows}={:?}  (expected 2, 2048, 2049, {rows})",
        wb.get_value("Sheet1", 2, 1),
        wb.get_value("Sheet1", 2048, 1),
        wb.get_value("Sheet1", 2049, 1),
        wb.get_value("Sheet1", rows, 1)
    )
}

/// The dependent formula is ingested before the formula it reads.
fn run_reversed() -> String {
    let mut wb = Workbook::new_with_config(WorkbookConfig::ephemeral());
    wb.add_sheet("Sheet1").unwrap();
    wb.set_value("Sheet1", 1, 1, LiteralValue::Number(1.0)).unwrap();

    let mut b = wb.engine_mut().begin_bulk_ingest();
    let s = b.add_sheet("Sheet1");
    b.add_formulas(s, vec![(1u32, 3u32, parse("=B1+1").unwrap())]);
    b.finish().unwrap();

    let mut b = wb.engine_mut().begin_bulk_ingest();
    let s = b.add_sheet("Sheet1");
    b.add_formulas(s, vec![(1u32, 2u32, parse("=A1+1").unwrap())]);
    b.finish().unwrap();

    wb.evaluate_all().unwrap();
    format!(
        "B1={:?} C1={:?}",
        wb.get_value("Sheet1", 1, 2),
        wb.get_value("Sheet1", 1, 3)
    )
}
