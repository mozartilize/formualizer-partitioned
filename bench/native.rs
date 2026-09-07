//! Native benchmark workload. Reuses the core Rust source modules directly;
//! the standalone crate does not compile the Python binding or link libpython.

use crate::{clock, graph, partition, prelude};
use formualizer::common::{value::LiteralValue, RangeAddress};
use formualizer::workbook::backends::CalamineAdapter;
use formualizer::workbook::traits::SpreadsheetReader;
use formualizer::workbook::{LoadStrategy, Workbook, WorkbookConfig};
use serde::ser::{Serialize, SerializeSeq, SerializeStruct, Serializer};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::io::Cursor;
use std::time::Instant;

type Grid = BTreeMap<String, Vec<Vec<LiteralValue>>>;

const DEFAULT_MAX_RATIO: f64 = 0.9;

enum Layout {
    Chunks(partition::ScratchPlan),
    Components,
}

struct Decision {
    verdict: Option<&'static str>,
    layout: Option<Layout>,
    chunk_reason: Option<&'static str>,
}

// Eligibility policy mirrors src/lib.rs. Keep it here, not in the Python
// library's public API: this executable is an independent benchmark client.
fn decide(
    topo: &graph::Topology,
    max_ratio: f64,
    min_formulas: usize,
    chunk_rows: u32,
    lookup_budget: u64,
) -> Decision {
    let plan = partition::plan_scratch(topo, chunk_rows, lookup_budget);
    let verdict = if topo.parse_errors > 0 {
        Some("unparsed formulas")
    } else if topo.xml_formula_cells != topo.cells.len() as u64 {
        Some("unresolved formula cells")
    } else if topo.cells.is_empty() {
        Some("no formulas")
    } else if topo.cells.len() < min_formulas {
        Some("too few formulas to be worth splitting")
    } else if topo.unsupported_refs > 0 {
        Some("names, tables or 3D references")
    } else if topo.named_refs > 0
        && plan.is_err()
        && topo
            .static_names
            .iter()
            .any(|n| n.scope != graph::NameScope::Workbook)
    {
        Some("defined names need the chunked layout")
    } else if topo.nondeterministic_fns > 0 {
        Some("unreproducible formulas")
    } else if topo.dynamic_refs > 0 {
        Some("INDIRECT/OFFSET references")
    } else if topo.array_formulas > 0 {
        Some("array formulas")
    } else if topo.self_refs > 0 {
        Some("self-referencing formulas")
    } else if plan.is_err()
        && topo.full_extent_cells > 0
        && topo.biggest_extent_cells() as f64 > max_ratio * topo.full_extent_cells as f64
    {
        Some("one component spans most of the sheet")
    } else {
        None
    };
    match (verdict, plan) {
        (Some(v), plan) => Decision {
            verdict: Some(v),
            layout: None,
            chunk_reason: plan.err(),
        },
        (None, Ok(plan)) => {
            // Mirrors src/lib.rs: array-capable templates go to the
            // components layout, which resolves spills against whole-file
            // occupancy.
            if plan.array_capable(topo) {
                return Decision {
                    verdict: None,
                    layout: Some(Layout::Components),
                    chunk_reason: Some("array-capable formulas"),
                };
            }
            Decision {
                verdict: None,
                layout: Some(Layout::Chunks(plan)),
                chunk_reason: None,
            }
        }
        (None, Err(reason)) => Decision {
            verdict: None,
            layout: Some(Layout::Components),
            chunk_reason: Some(reason),
        },
    }
}

struct Prepared {
    topo: graph::Topology,
}

fn prepare(data: &[u8], use_prelude: bool) -> Prepared {
    let mut src = graph::read(data);
    if use_prelude {
        prelude::fold_and_rewrite(data, &mut src);
    }
    Prepared {
        topo: graph::build_from(src),
    }
}

fn load_workbook(data: Vec<u8>) -> Result<Workbook, String> {
    let adapter =
        <CalamineAdapter as SpreadsheetReader>::open_bytes(data).map_err(|e| e.to_string())?;
    let mut config = WorkbookConfig::interactive();
    // A declared `<dimension>` is a hint, not a fact. Writers emit full-width
    // or otherwise inflated ranges for sheets holding a handful of cells, and
    // the loader materializes that declared rectangle densely, allocating a
    // `dims_cols`-wide row buffer per row. Two corpus files abort this way:
    // one declares A1:XFD20 (327,680 cells) for 7 formulas, another
    // A1:EWJ1172 (4,673,936 cells) for 54 formulas.
    //
    // Above this many declared cells, start sparse and let the populated cell
    // count decide. Tied to `sparse_sheet_cell_threshold`, the point where the
    // loader already starts testing whether a sheet is sparse: a sheet big
    // enough to warrant that test is too big to pre-materialize densely.
    // Note both files sit far below the 128,000,000 default, so raising the
    // benchmark's memory cap does not help them.
    config.ingest_limits.max_sheet_logical_cells =
        config.ingest_limits.sparse_sheet_cell_threshold;
    Workbook::from_reader(adapter, LoadStrategy::EagerAll, clock::pin(config))
        .map_err(|e| e.to_string())
}

fn run_partitioned(
    store: &partition::DataStore,
    topo: &graph::Topology,
    budget: u64,
    layout: &Layout,
) -> Result<partition::Evaluated, String> {
    match layout {
        Layout::Chunks(plan) => partition::run_scratch(store, topo, plan),
        Layout::Components => partition::run(store, topo, budget),
    }
}

fn sheet_extents(topo: &graph::Topology, _trim: bool) -> Vec<(String, Option<u16>, u32, u32)> {
    topo.sheets
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name.clone(), Some(i as u16), s.max_row, s.max_col))
        .chain(
            topo.name_only_sheets
                .iter()
                .map(|n| (n.clone(), None, 0, 0)),
        )
        .collect()
}

/// Grow the reported extents to cover spilled cells: the engine's own
/// dimensions grow with a spill, so the whole-file side reports rows and
/// columns a spill added.
fn grow_extents(
    sheets: &mut [(String, Option<u16>, u32, u32)],
    spilled: &[(u16, u32, u32, LiteralValue)],
) {
    for &(s, r, c, _) in spilled {
        if let Some(entry) = sheets.iter_mut().find(|(_, si, _, _)| *si == Some(s)) {
            entry.2 = entry.2.max(r);
            entry.3 = entry.3.max(c);
        }
    }
}

enum RowSource {
    Partitioned {
        topo: graph::Topology,
        store: partition::DataStore,
        values: Vec<LiteralValue>,
        /// Cells a spilled array wrote that no formula reads; served in
        /// place of the store, which does not hold them.
        spilled: HashMap<(u16, u32, u32), LiteralValue>,
    },
    Streamed {
        run: partition::ScratchRun,
        sheet: u16,
    },
}

struct RowIter {
    source: RowSource,
    sheets: Vec<(String, Option<u16>, u32, u32)>,
    sheet: usize,
    row: u32,
    strategy: &'static str,
    stream_refusal: Option<String>,
}

impl RowIter {
    fn implementation(&self) -> &'static str {
        match &self.source {
            RowSource::Streamed { .. } => "streamed",
            _ if self.strategy == "components" => "components",
            _ => "scratch",
        }
    }
}

// Serialize borrowed cells directly, without an intermediate JSON value tree.
struct Cell<'a>(&'a LiteralValue);
struct Row<'a>(&'a [LiteralValue]);

impl Serialize for Row<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for cell in self.0 {
            seq.serialize_element(&Cell(cell))?;
        }
        seq.end()
    }
}

impl Serialize for Cell<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            LiteralValue::Int(v) => s.serialize_i64(*v),
            LiteralValue::Number(v) => s.serialize_f64(*v),
            LiteralValue::Text(v) => s.serialize_str(v),
            LiteralValue::Boolean(v) => s.serialize_bool(*v),
            LiteralValue::Empty | LiteralValue::Pending => s.serialize_none(),
            LiteralValue::Date(v) => s.collect_str(v),
            LiteralValue::Time(v) => s.collect_str(v),
            LiteralValue::DateTime(v) => s.collect_str(v),
            LiteralValue::Duration(v) => s.collect_str(v),
            LiteralValue::Array(rows) => {
                let mut seq = s.serialize_seq(Some(rows.len()))?;
                for row in rows {
                    seq.serialize_element(&Row(row))?;
                }
                seq.end()
            }
            LiteralValue::Error(e) => {
                let mut out = s.serialize_struct("Error", 2 + usize::from(e.message.is_some()))?;
                out.serialize_field("type", "Error")?;
                out.serialize_field("kind", &format!("{:?}", e.kind))?;
                if let Some(message) = &e.message {
                    out.serialize_field("message", message)?;
                }
                out.end()
            }
        }
    }
}

fn whole(data: &[u8]) -> Result<Grid, String> {
    let mut wb = load_workbook(data.to_vec()).map_err(|e| e.to_string())?;
    wb.evaluate_all().map_err(|e| e.to_string())?;
    let mut grid = Grid::new();
    for name in wb.sheet_names() {
        let (r, c) = wb.sheet_dimensions(&name).unwrap_or_default();
        let rows = if r == 0 || c == 0 {
            Vec::new()
        } else {
            let addr = RangeAddress::new(name.clone(), 1, 1, r, c).map_err(|e| e.to_string())?;
            wb.read_range(&addr)
        };
        grid.insert(name, rows);
    }
    Ok(grid)
}

fn streamed(data: &[u8], min_formulas: usize) -> Result<RowIter, String> {
    let mut topo = prepare(data, true).topo;
    let decision = decide(
        &topo,
        DEFAULT_MAX_RATIO,
        min_formulas,
        partition::DEFAULT_CHUNK_ROWS,
        partition::DEFAULT_LOOKUP_BUDGET,
    );
    let mut sheets = sheet_extents(&topo, false);
    let strategy = match &decision.layout {
        Some(Layout::Chunks(_)) => "partitioned",
        Some(Layout::Components) => "components",
        None => return Err("streamed workload unexpectedly fell back whole".into()),
    };
    let mut stream_refusal = None;
    let source = match decision.layout.unwrap() {
        Layout::Chunks(plan) => match partition::ScratchRun::open(data, &mut topo, plan.clone()) {
            Ok(run) => RowSource::Streamed {
                sheet: run.sheet(),
                run,
            },
            Err(reason) => {
                stream_refusal = Some(reason);
                let store = partition::DataStore::reread(data, &topo)?;
                let evaluated = partition::run_scratch(&store, &topo, &plan)?;
                grow_extents(&mut sheets, &evaluated.spilled);
                RowSource::Partitioned {
                    topo,
                    store,
                    values: evaluated.values,
                    spilled: evaluated.spilled.into_iter().map(|(s, r, c, v)| ((s, r, c), v)).collect(),
                }
            }
        },
        Layout::Components => {
            let store = partition::DataStore::load(&mut topo);
            let evaluated = partition::run(&store, &topo, partition::DEFAULT_BUDGET_CELLS)?;
            grow_extents(&mut sheets, &evaluated.spilled);
            RowSource::Partitioned {
                topo,
                store,
                values: evaluated.values,
                spilled: evaluated.spilled.into_iter().map(|(s, r, c, v)| ((s, r, c), v)).collect(),
            }
        }
    };
    Ok(RowIter {
        source,
        sheets,
        sheet: 0,
        row: 1,
        strategy,
        stream_refusal,
    })
}

// The materialized partitioned entry point deliberately uses the store, as
// eval_partitioned does, rather than collecting the streamed entry point.
fn materialized(data: &[u8], min_formulas: usize) -> Result<Grid, String> {
    let mut topo = prepare(data, true).topo;
    let decision = decide(
        &topo,
        DEFAULT_MAX_RATIO,
        min_formulas,
        partition::DEFAULT_CHUNK_ROWS,
        partition::DEFAULT_LOOKUP_BUDGET,
    );
    let Some(layout) = decision.layout else {
        return whole(data);
    };
    let store = partition::DataStore::load(&mut topo);
    let evaluated = run_partitioned(&store, &topo, partition::DEFAULT_BUDGET_CELLS, &layout)?;
    let mut sheets = sheet_extents(&topo, false);
    grow_extents(&mut sheets, &evaluated.spilled);
    let mut rows = RowIter {
        source: RowSource::Partitioned {
            topo,
            store,
            values: evaluated.values,
            spilled: evaluated.spilled.into_iter().map(|(s, r, c, v)| ((s, r, c), v)).collect(),
        },
        sheets,
        sheet: 0,
        row: 1,
        strategy: "partitioned",
        stream_refusal: None,
    };
    let mut grid: Grid = rows
        .sheets
        .iter()
        .map(|(name, ..)| (name.clone(), Vec::new()))
        .collect();
    let mut buffer = Vec::new();
    while let Some((si, _)) = next_row(&mut rows, &mut buffer)? {
        grid.get_mut(&rows.sheets[si].0)
            .unwrap()
            .push(std::mem::take(&mut buffer));
    }
    Ok(grid)
}

// Mirrors the Python RowIter consumer, but reuses one native row buffer and
// avoids per-cell Python calls.
fn next_row(
    rows: &mut RowIter,
    buffer: &mut Vec<LiteralValue>,
) -> Result<Option<(usize, u32)>, String> {
    loop {
        let Some((_, si, max_row, max_col)) = rows.sheets.get(rows.sheet) else {
            return Ok(None);
        };
        if rows.row > *max_row || *max_row == 0 || *max_col == 0 {
            rows.sheet += 1;
            rows.row = 1;
            continue;
        }
        let rr = rows.row;
        rows.row += 1;
        buffer.clear();
        match &mut rows.source {
            RowSource::Partitioned {
                topo,
                store,
                values,
                spilled,
            } => {
                buffer.extend((1..=*max_col).map(|cc| {
                    match si.and_then(|s| topo.index.get(&(s, rr, cc))) {
                        Some(&i) => values[i as usize].clone(),
                        None => si
                            .and_then(|s| spilled.get(&(s, rr, cc)).cloned())
                            .or_else(|| si.and_then(|s| store.get(s, rr, cc)))
                            .unwrap_or(LiteralValue::Empty),
                    }
                }));
            }
            RowSource::Streamed { run, sheet } if *si == Some(*sheet) => {
                buffer.resize(*max_col as usize, LiteralValue::Empty);
                for (col, value) in run.row(rr)? {
                    if *col > 0 && *col <= *max_col {
                        buffer[*col as usize - 1] = value.clone();
                    }
                }
            }
            RowSource::Streamed { run, .. } => {
                buffer.extend((1..=*max_col).map(|cc| {
                    si.and_then(|s| run.other_value(s, rr, cc))
                        .unwrap_or(LiteralValue::Empty)
                }));
            }
        }
        return Ok(Some((rows.sheet, rr)));
    }
}

fn encode(row: &[LiteralValue], buffer: &mut Vec<u8>) -> Result<(), String> {
    buffer.clear();
    serde_json::to_writer(&mut *buffer, &Row(row)).map_err(|e| e.to_string())?;
    std::hint::black_box(&*buffer);
    Ok(())
}

fn consume(data: &[u8], mode: &str, min_formulas: usize) -> Result<Value, String> {
    let mut json_buffer = Vec::new();
    let (mut n_rows, mut n_cells, mut n_bytes) = (0u64, 0u64, 0u64);
    let mut consume_row = |row: &[LiteralValue]| -> Result<(), String> {
        encode(row, &mut json_buffer)?;
        n_rows += 1;
        n_cells += row.len() as u64;
        n_bytes += json_buffer.len() as u64;
        Ok(())
    };
    let (implementation, stream_refusal) = match mode {
        "whole" => {
            let grid = whole(data)?;
            for row in grid.values().flatten() {
                consume_row(row)?;
            }
            ("whole", None)
        }
        "partitioned" => {
            let mut rows = streamed(data, min_formulas)?;
            let mut buffer = Vec::new();
            while next_row(&mut rows, &mut buffer)?.is_some() {
                consume_row(&buffer)?;
            }
            (rows.implementation(), rows.stream_refusal.clone())
        }
        _ => return Err(format!("unknown consumption mode: {mode}")),
    };
    Ok(json!({"rows":n_rows, "cells":n_cells, "json_bytes":n_bytes,
        "implementation":implementation, "stream_refusal":stream_refusal}))
}

// Compare before serialization so JSON cannot hide a type mismatch or turn
// non-finite numbers into null. Treat engine Int/Number representations of an
// identical number alike, without rounding large integers through f64.
/// Largest relative difference two engines may report for the same cell and
/// still be called equal.
///
/// Summing a column in a different order changes the rounding of the last bits,
/// so `SUM(N5:N65536)` can answer `-63288861.46350002` on one path and
/// `-63288861.4635` on the other. That is the order of operations, not a
/// disagreement about the value, and counting it as a mismatch buries real
/// defects under thousands of last-bit differences.
///
/// The bound is measured, not chosen. Across the 289 mismatching workbooks of
/// the 3k and 6k samples, 5,344 relative deltas fall into two populations with
/// an empty band between them: 3,536 at or below 1e-10 (peaking at 1e-16, the
/// resolution of a double) and 1,808 at or above 1e-7, of which 1,751 exceed
/// 0.1. Nothing was observed between 1e-9 and 1e-8. This sits in that gap, two
/// decades above the largest rounding difference seen and far below the
/// smallest real one.
///
/// Raising it far enough to swallow a genuine disagreement would defeat the
/// check, so re-measure that gap before widening it.
const NUMERIC_REL_TOL: f64 = 1e-9;

/// Compare two numbers within `NUMERIC_REL_TOL`, scaled by the larger operand.
///
/// Scaling by the larger side keeps the test symmetric and leaves a value
/// against zero a genuine difference: a cell that should be 0 and answers
/// 1e-13 still counts, because the scale is then 1e-13 and the relative
/// difference is 1.
fn numeric_close(a: f64, b: f64) -> bool {
    if a == b {
        return true;
    }
    if !a.is_finite() || !b.is_finite() {
        return false;
    }
    let scale = a.abs().max(b.abs());
    scale > 0.0 && (a - b).abs() / scale <= NUMERIC_REL_TOL
}

fn equal(a: &LiteralValue, b: &LiteralValue) -> bool {
    match (a, b) {
        (LiteralValue::Number(x), LiteralValue::Number(y)) => numeric_close(*x, *y),
        (LiteralValue::Int(i), LiteralValue::Number(n))
        | (LiteralValue::Number(n), LiteralValue::Int(i)) => {
            n.is_finite() && numeric_close(*n, *i as f64)
        }
        (
            LiteralValue::Empty | LiteralValue::Pending,
            LiteralValue::Empty | LiteralValue::Pending,
        ) => true,
        (LiteralValue::Error(a), LiteralValue::Error(b)) => {
            a.kind == b.kind && a.message == b.message
        }
        (LiteralValue::Array(a), LiteralValue::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| row_equal(a, b))
        }
        _ => a == b,
    }
}

fn row_equal(a: &[LiteralValue], b: &[LiteralValue]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| equal(a, b))
}

const SAMPLE_LIMIT: usize = 10;

fn snapshot(value: &LiteralValue) -> Value {
    match value {
        LiteralValue::Text(text) => {
            json!({"type":"Text", "value":text.chars().take(256).collect::<String>(), "bytes":text.len(), "truncated":text.chars().count() > 256})
        }
        LiteralValue::Array(rows) => {
            json!({"type":"Array", "rows":rows.len(), "first_row_width":rows.first().map(Vec::len)})
        }
        LiteralValue::Error(e) => {
            json!({"type":"Error", "kind":format!("{:?}", e.kind), "message":e.message.as_ref().map(|m| m.chars().take(256).collect::<String>())})
        }
        // Debug preserves the native type and non-finite floats instead of
        // silently converting NaN/Infinity to a JSON null.
        _ => json!({"repr":format!("{value:?}")}),
    }
}

fn numeric(value: &LiteralValue) -> Option<f64> {
    match value {
        LiteralValue::Number(v) => Some(*v),
        LiteralValue::Int(v) => Some(*v as f64),
        _ => None,
    }
}

fn numeric_delta(a: &LiteralValue, b: &LiteralValue) -> Value {
    match (numeric(a), numeric(b)) {
        (Some(a), Some(b)) if a.is_finite() && b.is_finite() => {
            let absolute = (a - b).abs();
            json!({"absolute":absolute, "relative": if a == 0.0 { None } else { Some(absolute / a.abs()) }})
        }
        _ => Value::Null,
    }
}

// Numeric formula caches are Excel serials even when styled as dates. Compare
// native temporal values on that scale and honor the workbook's date system.
fn cache_equal(actual: &LiteralValue, cached: &LiteralValue, date1904: bool) -> bool {
    use chrono::{NaiveDate, Timelike};
    let days = |d: NaiveDate| {
        if date1904 {
            (d - NaiveDate::from_ymd_opt(1904, 1, 1).unwrap()).num_days() as f64
        } else {
            (d - NaiveDate::from_ymd_opt(1899, 12, 31).unwrap()).num_days() as f64
                + f64::from(d >= NaiveDate::from_ymd_opt(1900, 3, 1).unwrap())
        }
    };
    let time = |t: chrono::NaiveTime| {
        (t.num_seconds_from_midnight() as f64 + t.nanosecond() as f64 / 1e9) / 86400.0
    };
    let serial = match actual {
        LiteralValue::Date(d) => Some(days(*d)),
        LiteralValue::DateTime(dt) => Some(days(dt.date()) + time(dt.time())),
        LiteralValue::Time(t) => Some(time(*t)),
        LiteralValue::Duration(d) => {
            Some(d.num_seconds() as f64 / 86400.0 + d.subsec_nanos() as f64 / 86400e9)
        }
        _ => None,
    };
    if let (Some(serial), LiteralValue::Number(cached)) = (serial, cached) {
        return serial == *cached;
    }
    match (actual, cached) {
        (LiteralValue::Error(a), LiteralValue::Error(b)) => a.kind == b.kind,
        // The evaluator may canonicalize a cached empty formula string to blank.
        (LiteralValue::Empty | LiteralValue::Pending, LiteralValue::Text(s)) if s.is_empty() => {
            true
        }
        _ => equal(actual, cached),
    }
}

fn cache_sample(cell: &crate::cache::Cell) -> Value {
    json!({"state":cell.state, "formula":cell.formula, "formula_type":cell.formula_type,
        "shared_index":cell.shared_index, "raw":cell.raw.as_ref().map(|s| s.chars().take(256).collect::<String>()),
        "value":cell.value.as_ref().map(snapshot)})
}

#[derive(Default)]
struct Diff {
    count: usize,
    kinds: BTreeMap<&'static str, usize>,
    samples: Vec<Value>,
    /// Reference-side label for samples. `None` means the Formualizer
    /// whole-file baseline; `Some("materialized")` compares the two
    /// partitioned paths when no baseline exists.
    want_label: Option<&'static str>,
}

/// Rename the reference-side value key inside a sample. The samples are built
/// with the whole-baseline label and relabeled only when a comparison runs
/// without a baseline, which keeps one sample shape for both cases.
fn relabel(mut sample: Value, label: &str) -> Value {
    if let Some(obj) = sample.as_object_mut() {
        if let Some(value) = obj.remove("formualizer") {
            obj.insert(label.into(), value);
        }
    }
    sample
}

impl Diff {
    fn add(&mut self, kind: &'static str, sample: impl FnOnce() -> Value) {
        self.count += 1;
        *self.kinds.entry(kind).or_default() += 1;
        if self.samples.len() < SAMPLE_LIMIT {
            let mut value = sample();
            value["kind"] = json!(kind);
            self.samples.push(value);
        }
    }

    fn samples(&self) -> Vec<Value> {
        match self.want_label {
            Some(label) => self
                .samples
                .iter()
                .map(|s| relabel(s.clone(), label))
                .collect(),
            None => self.samples.clone(),
        }
    }

    fn report(&self) -> Value {
        let samples = self.samples();
        json!({"status":if self.count == 0 { "ok" } else { "mismatch" }, "differences":self.count,
            "kinds":self.kinds, "first":samples.first(), "samples":samples,
            "samples_truncated":self.count > self.samples.len()})
    }

    fn row(
        &mut self,
        sheet: &str,
        rn: u32,
        want: Option<&[LiteralValue]>,
        got: &[LiteralValue],
        cache: Option<&crate::cache::Book>,
    ) {
        let Some(want) = want else {
            self.add("unexpected_row", || json!({"sheet":sheet, "row":rn}));
            return;
        };
        if want.len() != got.len() {
            self.add(
                "row_width",
                || json!({"sheet":sheet, "row":rn, "formualizer":want.len(), "actual":got.len()}),
            );
        }
        for (ci, (a, b)) in want.iter().zip(got).enumerate() {
            if !equal(a, b) {
                self.add("value", || {
                    let cached = cache.and_then(|cache| {
                        cache
                            .sheets
                            .get(sheet)
                            .and_then(|rows| rows.get(&rn))
                            .and_then(|cells| cells.iter().find(|cell| cell.col == ci as u32 + 1))
                            .map(|cell| (cache.date1904, cell))
                    });
                    json!({"sheet":sheet, "row":rn, "col":ci+1, "formualizer":snapshot(a), "actual":snapshot(b),
                        "numeric_delta":numeric_delta(a,b), "excel_cached":cached.map(|(_, cell)| cache_sample(cell)),
                        "cache_matches_formualizer":cached.and_then(|(d, cell)| cell.value.as_ref().map(|v| cache_equal(a,v,d))),
                        "cache_matches_actual":cached.and_then(|(d, cell)| cell.value.as_ref().map(|v| cache_equal(b,v,d)))})
                });
            }
        }
    }
}

#[derive(Default)]
struct CachedDiff {
    counts: BTreeMap<&'static str, usize>,
    samples: Vec<Value>,
}

impl CachedDiff {
    fn row(
        &mut self,
        sheet: &str,
        rn: u32,
        row: Option<&[LiteralValue]>,
        cache: Option<&crate::cache::Book>,
    ) {
        let Some(cells) =
            cache.and_then(|cache| cache.sheets.get(sheet).and_then(|rows| rows.get(&rn)))
        else {
            return;
        };
        let date1904 = cache.is_some_and(|cache| cache.date1904);
        for cell in cells {
            let actual = row.and_then(|row| row.get(cell.col as usize - 1));
            let status = match (&cell.value, actual) {
                (None, _) => cell.state,
                (Some(_), None) => "missing_actual",
                (Some(cached), Some(actual)) if cache_equal(actual, cached, date1904) => "matched",
                _ => "mismatch",
            };
            *self.counts.entry(status).or_default() += 1;
            if matches!(status, "mismatch" | "missing_actual" | "unsupported")
                && self.samples.len() < SAMPLE_LIMIT
            {
                self.samples.push(json!({"sheet":sheet, "row":rn, "col":cell.col, "status":status,
                    "actual":actual.map(snapshot), "excel_cached":cache_sample(cell),
                    "numeric_delta":actual.zip(cell.value.as_ref()).map(|(a,b)| numeric_delta(b,a))}));
            }
        }
    }

    fn grid(grid: &Grid, cache: Option<&crate::cache::Book>) -> Self {
        let mut out = Self::default();
        let Some(cache) = cache else {
            return out;
        };
        for (name, rows) in &cache.sheets {
            for rn in rows.keys() {
                out.row(
                    name,
                    *rn,
                    grid.get(name)
                        .and_then(|rows| rows.get(*rn as usize - 1))
                        .map(Vec::as_slice),
                    Some(cache),
                );
            }
        }
        out
    }

    fn report(&self, formulas: usize) -> Value {
        let visited: usize = self.counts.values().sum();
        let compared = self.counts.get("matched").copied().unwrap_or(0)
            + self.counts.get("mismatch").copied().unwrap_or(0);
        json!({"status":if self.counts.get("mismatch").copied().unwrap_or(0) > 0 { "mismatch" } else if compared == 0 { "unavailable" } else { "ok" },
            "counts":self.counts, "compared":compared, "unvisited":formulas.saturating_sub(visited),
            "samples":self.samples, "sample_limit":SAMPLE_LIMIT})
    }
}

fn count_errors(row: &[LiteralValue], errors: &mut BTreeMap<String, usize>) {
    for value in row {
        match value {
            LiteralValue::Error(e) => *errors.entry(format!("{:?}", e.kind)).or_default() += 1,
            LiteralValue::Array(rows) => {
                for row in rows {
                    count_errors(row, errors);
                }
            }
            _ => (),
        }
    }
}

fn grid_errors(grid: &Grid) -> BTreeMap<String, usize> {
    let mut errors = BTreeMap::new();
    for row in grid.values().flatten() {
        count_errors(row, &mut errors);
    }
    errors
}

fn check(data: &[u8], min_formulas: usize) -> Result<Value, (&'static str, String)> {
    let start = Instant::now();
    let base = whole(data).map_err(|e| ("formualizer", e))?;
    let whole_secs = start.elapsed().as_secs_f64();
    // A cache read failure must not fail the check: the caches are
    // non-authoritative evidence, so an unreadable cache just leaves the
    // evidence unavailable.
    let cache = crate::cache::read(data).ok();
    let base_cached = CachedDiff::grid(&base, cache.as_ref());
    let base_errors = grid_errors(&base);
    let start = Instant::now();
    let part = materialized(data, min_formulas).map_err(|e| ("materialized", e))?;
    let part_secs = start.elapsed().as_secs_f64();
    let part_cached = CachedDiff::grid(&part, cache.as_ref());
    let part_errors = grid_errors(&part);
    let mut materialized_diff = Diff::default();
    if base.keys().ne(part.keys()) {
        materialized_diff.add("sheet_set", || json!({"formualizer":base.keys().collect::<Vec<_>>(), "actual":part.keys().collect::<Vec<_>>() }));
    }
    for (sheet, rows) in &part {
        if let Some(want) = base.get(sheet) {
            if want.len() != rows.len() {
                materialized_diff.add(
                    "row_count",
                    || json!({"sheet":sheet,"formualizer":want.len(),"actual":rows.len()}),
                );
            }
            for (ri, row) in rows.iter().enumerate() {
                materialized_diff.row(
                    sheet,
                    ri as u32 + 1,
                    want.get(ri).map(Vec::as_slice),
                    row,
                    cache.as_ref(),
                );
            }
        }
    }
    drop(part);
    let start = Instant::now();
    let mut rows = streamed(data, min_formulas).map_err(|e| ("streamed_setup", e))?;
    let mut stream_secs = start.elapsed().as_secs_f64();
    let mut stream_diff = Diff::default();
    let mut stream_cached = CachedDiff::default();
    let mut stream_errors = BTreeMap::new();
    let mut seen: BTreeMap<String, u32> = rows
        .sheets
        .iter()
        .map(|(name, ..)| (name.clone(), 0))
        .collect();
    if base.keys().ne(seen.keys()) {
        stream_diff.add("sheet_set", || json!({"formualizer":base.keys().collect::<Vec<_>>(), "actual":seen.keys().collect::<Vec<_>>() }));
    }
    let mut buffer = Vec::new();
    loop {
        let start = Instant::now();
        let next = next_row(&mut rows, &mut buffer).map_err(|e| ("streamed_row", e))?;
        stream_secs += start.elapsed().as_secs_f64();
        let Some((si, rn)) = next else { break };
        let name = &rows.sheets[si].0;
        let previous = seen.insert(name.clone(), rn).unwrap_or(0);
        if rn != previous + 1 {
            stream_diff.add(
                "row_sequence",
                || json!({"sheet":name, "expected":previous+1, "actual":rn}),
            );
        }
        stream_diff.row(
            name,
            rn,
            base.get(name)
                .and_then(|r| r.get(rn as usize - 1))
                .map(Vec::as_slice),
            &buffer,
            cache.as_ref(),
        );
        stream_cached.row(name, rn, Some(&buffer), cache.as_ref());
        count_errors(&buffer, &mut stream_errors);
    }
    for (name, want) in &base {
        let got = seen.get(name).copied().unwrap_or(0) as usize;
        if want.len() != got {
            stream_diff.add(
                "row_count",
                || json!({"sheet":name, "formualizer":want.len(), "actual":got}),
            );
        }
    }
    let differences = materialized_diff.count + stream_diff.count;
    Ok(
        json!({"status":if differences == 0 { "ok" } else { "mismatch" }, "differences":differences,
        "numeric_rel_tol":NUMERIC_REL_TOL,
        "first":materialized_diff.samples.first().or(stream_diff.samples.first()), "strategy":rows.strategy,
        "comparisons":{"materialized_vs_formualizer":materialized_diff.report(), "streamed_vs_formualizer":stream_diff.report()},
        "error_values":{"formualizer":base_errors,"materialized":part_errors,"streamed":stream_errors},
        "excel_cached":match &cache {
            Some(cache) => json!({"authoritative":false, "available":true, "formula_cells":cache.formulas, "date1904":cache.date1904,
                "calculation":cache.calculation, "formualizer":base_cached.report(cache.formulas),
                "materialized":part_cached.report(cache.formulas), "streamed":stream_cached.report(cache.formulas)}),
            // An unreadable cache leaves every side unavailable rather than
            // failing a check the caches cannot gate.
            None => json!({"authoritative":false, "available":false, "formula_cells":0,
                "formualizer":CachedDiff::default().report(0), "materialized":CachedDiff::default().report(0),
                "streamed":CachedDiff::default().report(0)}),
        },
        "timings":{"eval_grid":whole_secs, "eval_partitioned":part_secs, "eval_rows":stream_secs}}),
    )
}

/// Partitioned-only check for when the whole-file baseline cannot run. The
/// baseline can abort the process (a bogus declared extent makes the eager
/// whole load reserve gigabytes), and an aborted process reports nothing, so
/// this entry point never touches whole(): it compares the two partitioned
/// paths against each other, with the materialized grid as the reference.
fn check_part(data: &[u8], min_formulas: usize) -> Result<Value, (&'static str, String)> {
    let cache = crate::cache::read(data).ok();
    let start = Instant::now();
    let part = materialized(data, min_formulas).map_err(|e| ("materialized", e))?;
    let part_secs = start.elapsed().as_secs_f64();
    let part_cached = CachedDiff::grid(&part, cache.as_ref());
    let part_errors = grid_errors(&part);
    let start = Instant::now();
    let mut rows = streamed(data, min_formulas).map_err(|e| ("streamed_setup", e))?;
    let mut stream_secs = start.elapsed().as_secs_f64();
    let mut diff = Diff::default();
    diff.want_label = Some("materialized");
    let mut stream_cached = CachedDiff::default();
    let mut stream_errors = BTreeMap::new();
    let mut seen: BTreeMap<String, u32> = rows
        .sheets
        .iter()
        .map(|(name, ..)| (name.clone(), 0))
        .collect();
    if part.keys().ne(seen.keys()) {
        diff.add(
            "sheet_set",
            || json!({"materialized":part.keys().collect::<Vec<_>>(), "actual":seen.keys().collect::<Vec<_>>() }),
        );
    }
    let mut buffer = Vec::new();
    loop {
        let start = Instant::now();
        let next = next_row(&mut rows, &mut buffer).map_err(|e| ("streamed_row", e))?;
        stream_secs += start.elapsed().as_secs_f64();
        let Some((si, rn)) = next else { break };
        let name = &rows.sheets[si].0;
        let previous = seen.insert(name.clone(), rn).unwrap_or(0);
        if rn != previous + 1 {
            diff.add(
                "row_sequence",
                || json!({"sheet":name, "expected":previous+1, "actual":rn}),
            );
        }
        diff.row(
            name,
            rn,
            part.get(name)
                .and_then(|r| r.get(rn as usize - 1))
                .map(Vec::as_slice),
            &buffer,
            cache.as_ref(),
        );
        stream_cached.row(name, rn, Some(&buffer), cache.as_ref());
        count_errors(&buffer, &mut stream_errors);
    }
    for (name, want) in &part {
        let got = seen.get(name).copied().unwrap_or(0) as usize;
        if want.len() != got {
            diff.add(
                "row_count",
                || json!({"sheet":name, "materialized":want.len(), "actual":got}),
            );
        }
    }
    let report = diff.report();
    Ok(
        json!({"status":report["status"].clone(), "differences":diff.count, "first":report["first"].clone(),
        "strategy":rows.strategy, "whole_baseline":"unavailable",
        "comparisons":{"materialized_vs_streamed":report},
        "error_values":{"materialized":part_errors,"streamed":stream_errors},
        "excel_cached":match &cache {
            Some(cache) => json!({"authoritative":false, "available":true, "formula_cells":cache.formulas, "date1904":cache.date1904,
                "calculation":cache.calculation, "materialized":part_cached.report(cache.formulas),
                "streamed":stream_cached.report(cache.formulas)}),
            None => json!({"authoritative":false, "available":false, "formula_cells":0,
                "materialized":CachedDiff::default().report(0), "streamed":CachedDiff::default().report(0)}),
        },
        "timings":{"eval_partitioned":part_secs, "eval_rows":stream_secs}}),
    )
}

/// Run one isolated workload. File I/O and eligibility planning are untimed.
/// One pinned clock is shared by all evaluations in the worker.
pub fn worker(
    mode: &str,
    data: &[u8],
    min_formulas: usize,
) -> Result<Value, (&'static str, String)> {
    clock::begin_run();
    {
        let mut archive =
            zip::ZipArchive::new(Cursor::new(data)).map_err(|e| ("archive", e.to_string()))?;
        archive
            .by_name("xl/workbook.xml")
            .map_err(|e| ("archive", e.to_string()))?;
    }
    let mut result = {
        let topo = prepare(data, true).topo;
        let decision = decide(
            &topo,
            DEFAULT_MAX_RATIO,
            min_formulas,
            partition::DEFAULT_CHUNK_ROWS,
            partition::DEFAULT_LOOKUP_BUDGET,
        );
        let strategy = match &decision.layout {
            None => "whole",
            Some(Layout::Chunks(_)) => "partitioned",
            Some(Layout::Components) => "components",
        };
        let result = json!({"status": if decision.verdict.is_some() { "skipped" } else { "ok" },
            "reason": decision.verdict, "extent": topo.full_extent_cells, "formulas": topo.cells.len(),
            "batches": partition::plan_batches(&topo, partition::DEFAULT_BUDGET_CELLS).len(),
            "strategy":strategy, "chunk_reason": decision.chunk_reason,
            "planner":{"xml_formula_cells":topo.xml_formula_cells, "parse_errors":topo.parse_errors,
                "first_parse_error":topo.first_parse_error.as_ref().map(|e| json!({"stage":e.stage,"sheet":e.sheet,"row":e.row,"col":e.col,"formula":e.formula,"error":e.error}))}});
        if decision.verdict.is_some() {
            return Ok(result);
        }
        result
    };
    match mode {
        "check" => match check(data, min_formulas) {
            Ok(checked) => result
                .as_object_mut()
                .unwrap()
                .extend(checked.as_object().unwrap().clone()),
            Err((stage, reason)) => {
                result["status"] = json!("error");
                result["stage"] = json!(stage);
                result["reason"] = json!(reason);
            }
        },
        // Partitioned-only fallback: when the whole side aborts the process
        // (SIGABRT under the address-space cap), the combined "speed" worker
        // reports nothing, so the parent re-runs just this side alone.
        "speed_partitioned" => {
            let start = Instant::now();
            let output = consume(data, "partitioned", min_formulas);
            result["partitioned"] = json!(start.elapsed().as_secs_f64());
            match output {
                Ok(output) => result["partitioned_output"] = output,
                Err(reason) => {
                    result["status"] = json!("error");
                    result["stage"] = json!("partitioned");
                    result["reason"] = json!(reason);
                }
            }
        }
        // Partitioned-only fallback for the same reason: compares the two
        // partitioned paths without the whole-file baseline.
        "check_part" => match check_part(data, min_formulas) {
            Ok(checked) => result
                .as_object_mut()
                .unwrap()
                .extend(checked.as_object().unwrap().clone()),
            Err((stage, reason)) => {
                result["status"] = json!("error");
                result["stage"] = json!(stage);
                result["reason"] = json!(reason);
            }
        },
        "speed" => {
            for mode in ["whole", "partitioned"] {
                let start = Instant::now();
                let output = consume(data, mode, min_formulas);
                result[mode] = json!(start.elapsed().as_secs_f64());
                match output {
                    Ok(output) => result[format!("{mode}_output")] = output,
                    Err(reason) => {
                        result["status"] = json!("error");
                        result["stage"] = json!(mode);
                        result["reason"] = json!(reason);
                        return Ok(result);
                    }
                }
            }
            result["time_ratio"] =
                json!(result["partitioned"].as_f64().unwrap() / result["whole"].as_f64().unwrap());
        }
        "whole" | "partitioned" => {
            let start = Instant::now();
            let output = consume(data, mode, min_formulas);
            result["secs"] = json!(start.elapsed().as_secs_f64());
            match output {
                Ok(output) => result["output"] = output,
                Err(reason) => {
                    result["status"] = json!("error");
                    result["stage"] = json!(mode);
                    result["reason"] = json!(reason);
                }
            }
        }
        _ => return Err(("arguments", format!("unknown worker mode: {mode}"))),
    }
    Ok(result)
}
