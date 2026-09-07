//! Why do text-criteria aggregates (COUNTIF/SUMIF family with a text
//! criterion) answer differently in a loader-built workbook and an
//! incrementally-built one? Reproduce the disagreement on one file, then
//! rebuild the component with different value-write mechanisms and see
//! which one matches the loader.
//!
//! Run: cargo run --release --manifest-path probe/Cargo.toml --bin textcrit -- <file.xlsx>

use formualizer::common::value::LiteralValue;
use formualizer::workbook::backends::CalamineAdapter;
use formualizer::workbook::traits::SpreadsheetReader;
use formualizer::workbook::{LoadStrategy, Workbook, WorkbookConfig};
use formualizer_partitioned::{clock, graph, partition, prelude};
use std::collections::HashSet;
use std::env;

fn whole_workbook(data: Vec<u8>) -> Workbook {
    let adapter = <CalamineAdapter as SpreadsheetReader>::open_bytes(data).unwrap();
    let mut config = WorkbookConfig::interactive();
    config.ingest_limits.max_sheet_logical_cells =
        config.ingest_limits.sparse_sheet_cell_threshold;
    let mut wb =
        Workbook::from_reader(adapter, LoadStrategy::EagerAll, clock::pin(config)).unwrap();
    wb.evaluate_all().unwrap();
    wb
}

/// One component's inputs, row-major, deduped.
fn component_inputs(
    store: &partition::DataStore,
    topo: &graph::Topology,
    comp: u32,
) -> Vec<(u16, u32, u32, LiteralValue)> {
    let mut seen: HashSet<(u16, u32, u32)> = HashSet::new();
    let mut out = Vec::new();
    for &(s, r0, c0, r1, c1) in &topo.comp_refs[comp as usize] {
        store.for_range(s, r0, c0, r1, c1, |r, c, v| {
            if seen.insert((s, r, c)) {
                out.push((s, r, c, v.clone()));
            }
        });
    }
    out.sort_unstable_by_key(|&(s, r, c, _)| (s, r, c));
    out
}

fn fresh_wb() -> Workbook {
    Workbook::new_with_config(clock::pin(WorkbookConfig::ephemeral()))
}

fn add_sheets(wb: &mut Workbook, topo: &graph::Topology, comp: u32) {
    let mut added: HashSet<u16> = HashSet::new();
    for &(s, ..) in &topo.comp_refs[comp as usize] {
        if added.insert(s) {
            wb.add_sheet(&topo.sheets[s as usize].name).unwrap();
        }
    }
    for &c in &topo.comp_cells[comp as usize] {
        let s = topo.cells[c as usize].sheet;
        if added.insert(s) {
            wb.add_sheet(&topo.sheets[s as usize].name).unwrap();
        }
    }
}

/// The formula text of a cell, without a leading '='.
fn formula_text(topo: &graph::Topology, cell: u32) -> String {
    let t = topo.texts[topo.cells[cell as usize].ast as usize].as_ref();
    t.strip_prefix('=').unwrap_or(t).to_string()
}

/// Write values the way the batch does today: per-cell set_value, range-list
/// order, skipping duplicates.
fn write_values_setvalue_range_order(
    wb: &mut Workbook,
    store: &partition::DataStore,
    topo: &graph::Topology,
    comp: u32,
) {
    let mut seen: HashSet<(u16, u32, u32)> = HashSet::new();
    for &(s, r0, c0, r1, c1) in &topo.comp_refs[comp as usize] {
        let name = &topo.sheets[s as usize].name;
        store.for_range(s, r0, c0, r1, c1, |r, c, v| {
            if seen.insert((s, r, c)) {
                wb.set_value(name, r, c, v.clone()).unwrap();
            }
        });
    }
}

/// Per-cell set_value, but row-major across the whole component.
fn write_values_setvalue_rowmajor(
    wb: &mut Workbook,
    store: &partition::DataStore,
    topo: &graph::Topology,
    comp: u32,
) {
    for (s, r, c, v) in component_inputs(store, topo, comp) {
        wb.set_value(&topo.sheets[s as usize].name, r, c, v).unwrap();
    }
}

/// Dense-rectangle set_values rows, the shape the loader streams.
fn write_values_setvalues_rows(
    wb: &mut Workbook,
    store: &partition::DataStore,
    topo: &graph::Topology,
    comp: u32,
) {
    let inputs = component_inputs(store, topo, comp);
    let mut by_sheet: std::collections::BTreeMap<u16, Vec<(u32, u32, LiteralValue)>> =
        Default::default();
    for (s, r, c, v) in inputs {
        by_sheet.entry(s).or_default().push((r, c, v));
    }
    for (s, cells) in by_sheet {
        let name = &topo.sheets[s as usize].name;
        let min_r = cells.iter().map(|c| c.0).min().unwrap();
        let max_r = cells.iter().map(|c| c.0).max().unwrap();
        let min_c = cells.iter().map(|c| c.1).min().unwrap();
        let max_c = cells.iter().map(|c| c.1).max().unwrap();
        let mut rows: Vec<Vec<LiteralValue>> = Vec::new();
        for r in min_r..=max_r {
            let mut row = vec![LiteralValue::Empty; (max_c - min_c + 1) as usize];
            for &(cr, cc, ref v) in &cells {
                if cr == r {
                    row[(cc - min_c) as usize] = v.clone();
                }
            }
            rows.push(row);
        }
        wb.set_values(name, min_r, min_c, &rows).unwrap();
    }
}

fn main() {
    let path = env::args().nth(1).expect("usage: textcrit <file.xlsx>");
    let data = std::fs::read(&path).unwrap();

    // Whole-file answer, loader-built.
    let mut whole = whole_workbook(data.clone());

    // Partitioned answer today (the verdict gate is bypassed on purpose).
    let mut src = graph::read(&data);
    prelude::fold_and_rewrite(&data, &mut src);
    let mut topo = graph::build_from(src);
    let store = partition::DataStore::load(&mut topo);
    let evaluated = partition::run(&store, &topo, partition::DEFAULT_BUDGET_CELLS).unwrap();

    let mut diffs: Vec<(String, u32, u32, usize)> = Vec::new();
    for (i, fc) in topo.cells.iter().enumerate() {
        let name = &topo.sheets[fc.sheet as usize].name;
        let w = whole.get_value(name, fc.row, fc.col);
        if w != Some(evaluated.values[i].clone()) {
            diffs.push((name.clone(), fc.row, fc.col, i));
        }
    }
    println!(
        "{} formula cells, {} diffs (whole vs partitioned)",
        topo.cells.len(),
        diffs.len()
    );
    for (name, r, c, i) in diffs.iter().take(20) {
        let fc = &topo.cells[*i as usize];
        println!(
            "  {name}!{r},{c} = {} | whole={:?} part={:?}",
            topo.texts[fc.ast as usize],
            whole.get_value(name, *r, *c),
            evaluated.values[*i]
        );
    }

    // Rebuild each differing cell's component three ways and compare.
    let mut seen_comp: HashSet<u32> = HashSet::new();
    for (name, r, c, i) in diffs.iter().take(10) {
        let comp = topo.comp_of[*i as usize];
        if !seen_comp.insert(comp) {
            continue;
        }
        let w_val = whole.get_value(name, *r, *c);
        println!(
            "--- component {comp}: {name}!{r},{c} whole={:?} ({} formulas)",
            w_val,
            topo.comp_cells[comp as usize].len()
        );

        let mut results: Vec<(&str, Option<LiteralValue>)> = Vec::new();
        let variants: Vec<(&str, Box<dyn Fn(&mut Workbook, &partition::DataStore, &graph::Topology, u32)>)> = vec![
            ("setvalue-range-order", Box::new(write_values_setvalue_range_order)),
            ("setvalue-row-major", Box::new(write_values_setvalue_rowmajor)),
            ("setvalues-rows", Box::new(write_values_setvalues_rows)),
        ];
        for (label, writer) in variants {
            let mut wb = fresh_wb();
            add_sheets(&mut wb, &topo, comp);
            writer(&mut wb, &store, &topo, comp);
            let text = formula_text(&topo, *i as u32);
            wb.set_formula(name, *r, *c, &text).unwrap();
            if let Err(e) = wb.evaluate_all() {
                results.push((label, None));
                println!("  {label}: evaluate error: {e}");
                continue;
            }
            let v = wb.get_value(name, *r, *c);
            println!(
                "  {label}: {:?} {}",
                v,
                if v == w_val { "MATCH" } else { "DIFF" }
            );
            results.push((label, v));
        }
        let _ = results;
    }
}
