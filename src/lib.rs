//! Memory-bounded xlsx formula evaluation.
//!
//! Split a workbook into bounded pieces and evaluate each one in isolation
//! (small extent = small peak RSS), then assemble the results.
//!
//! ## When it falls back
//!
//! The evaluator uses the whole-file path under these conditions:
//!
//! - A formula fails to parse.
//! - The reader cannot resolve a table, external reference, 3D reference, or
//!   broken `#REF!` statically.
//! - A formula uses a defined name that this crate cannot resolve to a fixed
//!   range, or the file's shape cannot use the chunked layout. Only the
//!   reused chunk workbook defines names. See `partition`.
//! - A formula uses `INDIRECT` or `OFFSET`. Their precedents exist only during
//!   evaluation.
//! - An array formula can spill into cells that a mini-workbook does not hold.
//! - A formula names its own cell. The mini-workbook edit path rejects it.
//! - The file has fewer than `DEFAULT_MIN_FORMULAS` formulas.
//! - One component spans more than `DEFAULT_MAX_RATIO` of the sheet.
//!
//! Unresolved references and array formulas prevent a complete dependency
//! closure. The remaining conditions protect correctness or prevent work that
//! cannot repay the cost of partitioning.
//!
//! ## Trimming for row consumers
//!
//! `eval_rows(trim=true)` reports each sheet through its last value or formula.
//! Without trimming, output extends through the declared `<dimension>` and
//! cells that hold only a style.
//!
//! Trimming is off by default, so `eval_rows` and `eval_grid` report the same
//! dimensions. Trimming does not remove blank rows inside the reported extent.
//! The XML scan also prevents backend bounds from omitting cells with values.

//!
//! ## One adaptive path, two layouts
//!
//! A partitionable workbook is evaluated one of two ways, chosen
//! automatically and reported as `partition_plan`'s and `RowIter`'s
//! `strategy`:
//!
//! - `"partitioned"`: a solid block of row-local formulas reuses one small
//!   chunk workbook across row chunks. `eval_rows` streams that sheet's input
//!   rows from the file when it can; otherwise the same chunks are evaluated
//!   from a preloaded value store. Both give identical rows, so a caller never
//!   chooses between them.
//! - `"components"`: every other shape the dependency closure allows. Each
//!   batch of components gets a fresh workbook.
//!
//! Neither layout changes a result. The choice only trades peak memory and
//! time; see [`bench/README.md`](../bench/README.md).

pub mod clock;
pub mod graph;
pub mod partition;
pub mod prelude;
#[cfg(test)]
mod testkit;
mod values;


use std::collections::HashMap;

use chrono::{Datelike, Timelike};
use pyo3::conversion::IntoPyObjectExt;
use pyo3::exceptions::{PyIOError, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use formualizer::common::value::LiteralValue;
use formualizer::common::RangeAddress;
use formualizer::workbook::backends::CalamineAdapter;
use formualizer::workbook::traits::SpreadsheetReader;
use formualizer::workbook::{LoadStrategy, Workbook, WorkbookConfig};

/// Convert an engine LiteralValue to a Python object, byte-for-byte matching
/// the formualizer v0.8.4 python binding's `literal_to_py` so the downstream
/// `_fz_cell_value` coercion in s3_excel2.py behaves identically. Errors become
/// `{"type": "Error", "kind": "<Debug>"}`; dates/times become native objects.
fn literal_to_py(py: Python<'_>, value: &LiteralValue) -> PyResult<PyObject> {
    match value {
        LiteralValue::Int(v) => Ok((*v).into_py_any(py)?),
        LiteralValue::Number(v) => Ok((*v).into_py_any(py)?),
        LiteralValue::Boolean(v) => Ok((*v).into_py_any(py)?),
        LiteralValue::Text(v) => Ok(v.clone().into_py_any(py)?),
        LiteralValue::Empty | LiteralValue::Pending => Ok(py.None()),
        // datetime C-API is outside the limited ABI (abi3), so build these
        // through the stdlib datetime module instead of pyo3's PyDate types.
        LiteralValue::Date(d) => {
            let cls = py.import("datetime")?.getattr("date")?;
            Ok(cls.call1((d.year(), d.month(), d.day()))?.unbind())
        }
        LiteralValue::Time(t) => {
            let cls = py.import("datetime")?.getattr("time")?;
            Ok(cls
                .call1((t.hour(), t.minute(), t.second(), t.nanosecond() / 1000))?
                .unbind())
        }
        LiteralValue::DateTime(dt) => {
            let cls = py.import("datetime")?.getattr("datetime")?;
            Ok(cls
                .call1((
                    dt.year(),
                    dt.month(),
                    dt.day(),
                    dt.hour(),
                    dt.minute(),
                    dt.second(),
                    dt.nanosecond() / 1000,
                ))?
                .unbind())
        }
        LiteralValue::Duration(d) => {
            let cls = py.import("datetime")?.getattr("timedelta")?;
            let kwargs = PyDict::new(py);
            kwargs.set_item("seconds", d.num_seconds())?;
            kwargs.set_item(
                "microseconds",
                d.num_microseconds().unwrap_or(0).rem_euclid(1_000_000),
            )?;
            Ok(cls.call((), Some(&kwargs))?.unbind())
        }
        LiteralValue::Array(rows) => {
            let out = PyList::empty(py);
            for row in rows {
                let py_row = PyList::empty(py);
                for cell in row {
                    py_row.append(literal_to_py(py, cell)?)?;
                }
                out.append(py_row)?;
            }
            Ok(out.into())
        }
        LiteralValue::Error(err) => {
            let dict = PyDict::new(py);
            dict.set_item("type", "Error")?;
            dict.set_item("kind", format!("{:?}", err.kind))?;
            if let Some(msg) = &err.message {
                dict.set_item("message", msg)?;
            }
            Ok(dict.into())
        }
    }
}

fn load_workbook(data: Vec<u8>) -> PyResult<Workbook> {
    let adapter = <CalamineAdapter as SpreadsheetReader>::open_bytes(data)
        .map_err(|e| PyIOError::new_err(format!("open failed: {e}")))?;
    Workbook::from_reader(
        adapter,
        LoadStrategy::EagerAll,
        clock::pin(WorkbookConfig::interactive()),
    )
    .map_err(|e| PyIOError::new_err(format!("load failed: {e}")))
}

/// Load xlsx bytes, evaluate every formula, and return per-sheet dimensions.
///
/// Returns a list of `(sheet_name, rows, cols)` tuples.
#[pyfunction]
fn probe_dims(data: Vec<u8>) -> PyResult<Vec<(String, u32, u32)>> {
    clock::begin_run();
    let mut wb = load_workbook(data)?;
    wb.evaluate_all()
        .map_err(|e| PyRuntimeError::new_err(format!("evaluate_all failed: {e}")))?;
    let out = wb
        .sheet_names()
        .into_iter()
        .map(|name| {
            let (rows, cols) = wb.sheet_dimensions(&name).unwrap_or((0, 0));
            (name, rows, cols)
        })
        .collect();
    Ok(out)
}

/// Load xlsx bytes, evaluate every formula, and return per-sheet value grids.
///
/// Returns `{sheet_name: [[cell, ...], ...]}` covering rows 1..=max_row and
/// cols 1..=max_col, with values converted exactly as the formualizer python
/// binding would. This is the whole-file baseline that the partitioned path is
/// compared against.
#[pyfunction]
#[pyo3(signature = (data, now = None))]
fn eval_grid(py: Python<'_>, data: Vec<u8>, now: Option<f64>) -> PyResult<PyObject> {
    clock::begin_run_at(now);
    eval_whole(py, data)
}

/// Load and evaluate the entire workbook at once. Also the fallback path when
/// a file cannot be partitioned safely.
fn eval_whole(py: Python<'_>, data: Vec<u8>) -> PyResult<PyObject> {
    let mut wb = load_workbook(data)?;
    wb.evaluate_all()
        .map_err(|e| PyRuntimeError::new_err(format!("evaluate_all failed: {e}")))?;
    let out = PyDict::new(py);
    for name in wb.sheet_names() {
        let (max_row, max_col) = wb.sheet_dimensions(&name).unwrap_or((0, 0));
        let sheet_list = PyList::empty(py);
        if max_row >= 1 && max_col >= 1 {
            let addr = RangeAddress::new(name.clone(), 1, 1, max_row, max_col)
                .map_err(|e| PyRuntimeError::new_err(format!("range failed: {e}")))?;
            for row in wb.read_range(&addr) {
                let py_row = PyList::empty(py);
                for cell in &row {
                    py_row.append(literal_to_py(py, cell)?)?;
                }
                sheet_list.append(py_row)?;
            }
        }
        out.set_item(name, sheet_list)?;
    }
    Ok(out.into())
}

/// Extract dependency components without evaluating.
///
/// Streams the sheet XML, parses each distinct formula with `formualizer-parse`
/// (shared formulas parsed once at their master anchor and reused per member
/// with an offset), and groups formula cells into weakly-connected components
/// with union-find. Only formula-to-formula edges merge components; data
/// precedents widen the bounding box but never merge two formulas, so a shared
/// input cell cannot glue unrelated components together.
#[pyfunction]
fn components(py: Python<'_>, data: Vec<u8>) -> PyResult<PyObject> {
    let a = graph::analyze(&data);
    let out = PyDict::new(py);
    out.set_item("n_formula_cells", a.n_formula_cells)?;
    out.set_item("n_components", a.n_components)?;
    out.set_item("n_parsed", a.n_parsed)?;
    out.set_item("full_extent_cells", a.full_extent_cells)?;
    out.set_item("biggest_extent_cells", a.biggest_extent_cells)?;
    out.set_item(
        "biggest_pct",
        if a.full_extent_cells > 0 {
            100.0 * a.biggest_extent_cells as f64 / a.full_extent_cells as f64
        } else {
            0.0
        },
    )?;
    out.set_item("cross_sheet", a.cross_sheet)?;
    out.set_item("cross_row", a.cross_row)?;
    out.set_item("unsupported_refs", a.unsupported_refs)?;
    out.set_item("parse_errors", a.parse_errors)?;
    out.set_item("xml_formula_cells", a.xml_formula_cells)?;
    set_parse_failure(&out, a.first_parse_error.as_ref())?;
    out.set_item("t_read_ms", a.t_read_ms)?;
    out.set_item("t_graph_ms", a.t_graph_ms)?;
    let top = PyList::empty(py);
    for (ext, sr, sc) in &a.top {
        top.append((*ext, *sr, *sc))?;
    }
    out.set_item("top", top)?;
    Ok(out.into())
}


/// Largest component extent accepted before a file falls back to whole-file
/// evaluation.
///
/// A component that spans more than this share of the sheet sets the peak on
/// its own, so splitting it returns nothing worth its cost.
const DEFAULT_MAX_RATIO: f64 = 0.9;

/// One of the two shapes a partitioned evaluation can take.
///
/// Chosen automatically; reported to callers as `strategy` (`"partitioned"`
/// or `"components"`).
enum Layout {
    /// A solid block of row-local formulas reuses one small chunk workbook
    /// across row chunks. `eval_rows` streams that sheet's input rows from
    /// the file when it can; other callers, and files whose sheet cannot
    /// stream, use a preloaded value store instead. Both give identical rows.
    Chunks(partition::ScratchPlan),
    /// Every other shape the dependency closure allows: one workbook per
    /// batch of components.
    Components,
}

/// The result of deciding how a workbook would be evaluated.
struct Decision {
    /// `Some` names why the file must use the whole-file path.
    verdict: Option<&'static str>,
    /// The layout to use when `verdict` is `None`.
    layout: Option<Layout>,
    /// Why the chunk layout was refused, even when components ran instead.
    /// `None` when the chunk layout was used or the file falls back whole.
    chunk_reason: Option<&'static str>,
}

/// Decide the layout for a workbook, once, so no caller plans twice.
fn decide(
    topo: &graph::Topology,
    max_ratio: f64,
    min_formulas: usize,
    chunk_rows: u32,
    lookup_budget: u64,
) -> Decision {
    let chunk_plan = partition::plan_scratch(topo, chunk_rows, lookup_budget);
    let verdict = partition_verdict(topo, max_ratio, min_formulas, chunk_plan.is_ok());
    match (verdict, chunk_plan) {
        // A whole-file verdict keeps the chunk plan's own refusal: the plan
        // may have been fine while another gate (say `min_formulas`) refused.
        (Some(v), plan) => Decision { verdict: Some(v), layout: None, chunk_reason: plan.err() },
        (None, Ok(plan)) => {
            // A chunk workbook holds only the columns the plan copies, so a
            // spilled array could occupy cells the chunk never wrote. The
            // components layout resolves spills against whole-file
            // occupancy (see `partition::run`), so array-capable templates
            // must not take the chunk path.
            if plan.array_capable(topo) {
                return Decision {
                    verdict: None,
                    layout: Some(Layout::Components),
                    chunk_reason: Some("array-capable formulas"),
                };
            }
            Decision { verdict: None, layout: Some(Layout::Chunks(plan)), chunk_reason: None }
        }
        (None, Err(reason)) => {
            Decision { verdict: None, layout: Some(Layout::Components), chunk_reason: Some(reason) }
        }
    }
}

/// Decide whether a workbook can be partitioned at all.
///
/// Partitioning is only sound when every reference was resolved statically. A
/// table, 3D or INDIRECT/OFFSET reference means the dependency closure is
/// incomplete and the file must be evaluated whole. A call that two engines
/// cannot agree on, such as `RAND`, keeps its file whole for the same reason;
/// `NOW` and `TODAY` do not, because `clock` pins one instant per run.
/// Fixed defined names, including sheet-scoped names, are recreated in
/// per-component batches with their original scope and target.
fn partition_verdict(
    topo: &graph::Topology,
    max_ratio: f64,
    min_formulas: usize,
    chunk_ok: bool,
) -> Option<&'static str> {
    // A file whose formulas all failed to parse is not a formula-free file.
    if topo.parse_errors > 0 {
        return Some("unparsed formulas");
    }
    if topo.xml_formula_cells != topo.cells.len() as u64 {
        return Some("unresolved formula cells");
    }
    if topo.cells.is_empty() {
        return Some("no formulas");
    }
    // Partitioning replaces one load with an XML pass, a value store and a
    // workbook per batch. Below a few thousand formulas there is not enough
    // evaluation left to pay that back.
    if topo.cells.len() < min_formulas {
        return Some("too few formulas to be worth splitting");
    }
    if topo.unsupported_refs > 0 {
        return Some("names, tables or 3D references");
    }
    if topo.nondeterministic_fns > 0 {
        return Some("unreproducible formulas");
    }
    if topo.dynamic_refs > 0 {
        return Some("INDIRECT/OFFSET references");
    }
    if topo.self_refs > 0 {
        return Some("self-referencing formulas");
    }
    if !chunk_ok
        && topo.full_extent_cells > 0
        && topo.biggest_extent_cells() as f64 > max_ratio * topo.full_extent_cells as f64
    {
        return Some("one component spans most of the sheet");
    }
    None
}

fn set_parse_failure(out: &Bound<'_, PyDict>, failure: Option<&graph::ParseFailure>) -> PyResult<()> {
    if let Some(f) = failure {
        let detail = PyDict::new(out.py());
        detail.set_item("stage", f.stage)?;
        detail.set_item("sheet", &f.sheet)?;
        detail.set_item("row", f.row)?;
        detail.set_item("col", f.col)?;
        detail.set_item("formula", &f.formula)?;
        detail.set_item("error", &f.error)?;
        out.set_item("first_parse_error", detail)
    } else {
        out.set_item("first_parse_error", out.py().None())
    }
}

/// Report how a workbook would be evaluated, without evaluating it.
#[pyfunction]
#[pyo3(signature = (data, budget_cells = None, max_ratio = DEFAULT_MAX_RATIO, min_formulas = DEFAULT_MIN_FORMULAS, prelude = true, lookup_budget = partition::DEFAULT_LOOKUP_BUDGET, now = None))]
fn partition_plan(
    py: Python<'_>,
    data: Vec<u8>,
    budget_cells: Option<u64>,
    max_ratio: f64,
    min_formulas: usize,
    prelude: bool,
    lookup_budget: u64,
    now: Option<f64>,
) -> PyResult<PyObject> {
    clock::begin_run_at(now);
    let prepared = prepare(&data, prelude);
    let topo = prepared.topo;
    let budget = budget_cells.unwrap_or(partition::DEFAULT_BUDGET_CELLS);
    let decision = decide(&topo, max_ratio, min_formulas, partition::DEFAULT_CHUNK_ROWS, lookup_budget);

    let out = PyDict::new(py);
    out.set_item("partitioned", decision.verdict.is_none())?;
    out.set_item("fallback_reason", decision.verdict)?;
    out.set_item("budget_cells", budget)?;
    out.set_item("n_formula_cells", topo.cells.len())?;
    out.set_item("n_formula_sources", topo.texts.len())?;
    out.set_item("xml_formula_cells", topo.xml_formula_cells)?;
    out.set_item("parse_errors", topo.parse_errors)?;
    set_parse_failure(&out, topo.first_parse_error.as_ref())?;
    out.set_item("unsupported_refs", topo.unsupported_refs)?;
    out.set_item("dynamic_refs", topo.dynamic_refs)?;
    out.set_item("n_components", topo.comp_cells.len())?;
    // The component-batch estimate. Files that use the chunked layout do not
    // batch this way; see `strategy`.
    out.set_item("n_batches", partition::plan_batches(&topo, budget).len())?;
    out.set_item("biggest_extent_cells", topo.biggest_extent_cells())?;
    out.set_item("full_extent_cells", topo.full_extent_cells)?;
    out.set_item("prelude_folded", prepared.prelude.folded)?;
    out.set_item("prelude_rewritten", prepared.prelude.rewritten)?;
    out.set_item("prelude_chunks", prepared.prelude.chunks)?;
    out.set_item("prelude_ms", prepared.prelude.t_ms)?;
    out.set_item("nondeterministic_fns", topo.nondeterministic_fns)?;
    out.set_item("row_sensitive_fns", topo.row_sensitive_fns)?;
    out.set_item("named_refs", topo.named_refs)?;
    out.set_item("lookup_work", topo.lookup_work())?;
    out.set_item("lookup_budget", lookup_budget)?;
    out.set_item(
        "strategy",
        match &decision.layout {
            None => "whole",
            Some(Layout::Chunks(_)) => "partitioned",
            Some(Layout::Components) => "components",
        },
    )?;
    match &decision.layout {
        Some(Layout::Chunks(plan)) => {
            out.set_item("chunk_reason", py.None())?;
            out.set_item("n_chunks", plan.n_chunks())?;
            out.set_item("chunk_rows", plan.last_row - plan.first_row + 1)?;
            out.set_item("carry_rows", plan.carry)?;
            out.set_item("data_cols", plan.n_data_cols())?;
            // `eval_rows` streams the rows of the formula sheet when the sheet
            // holds them in ascending order. Every other caller uses the store.
            out.set_item(
                "streamable",
                topo.sheets[plan.sheet as usize].rows_ascending,
            )?;
        }
        _ => match decision.chunk_reason {
            Some(reason) => out.set_item("chunk_reason", reason)?,
            None => out.set_item("chunk_reason", py.None())?,
        },
    }
    Ok(out.into())
}

/// Formula count below which splitting costs more than it saves. Set to 0 to
/// partition whenever it is safe, which the correctness sweeps do.
const DEFAULT_MIN_FORMULAS: usize = 2_000;

/// A workbook read, folded and grouped into components.
struct Prepared {
    topo: graph::Topology,
    prelude: prelude::Report,
}

/// Read a workbook, fold its whole-column aggregates, and build the graph.
///
/// The fold runs between the two stages because it changes the formula text.
/// It is skipped when `use_prelude` is false, which lets a caller compare the
/// effect of folding on the same file.
fn prepare(data: &[u8], use_prelude: bool) -> Prepared {
    let mut src = graph::read(data);
    let report = if use_prelude {
        prelude::fold_and_rewrite(data, &mut src)
    } else {
        prelude::Report { folded: 0, rewritten: 0, chunks: 0, t_ms: 0.0 }
    };
    Prepared { topo: graph::build_from(src), prelude: report }
}

/// Evaluate a partitionable workbook by the layout `decide` chose.
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

/// How a row run was actually executed, for the benchmark harness.
///
/// `RowIter::strategy` intentionally merges both chunk executions into
/// `partitioned`; a benchmark needs to tell them apart, so `_benchmark_rows`
/// reports this instead.
struct RunInfo {
    /// `streamed`, `scratch`, `components` or `whole`.
    impl_mode: &'static str,
    /// Why streaming was refused when the chunk layout ran from the store.
    stream_refusal: Option<String>,
    /// Chunk-plan stats: chunk count, spanned rows, carry rows, data columns.
    chunk: Option<(usize, u32, u32, usize)>,
    /// The component-batch estimate for this file.
    n_batches: usize,
}

/// Sheet extents as a whole-file run would report them.
///
/// Backend bounds cannot be trusted on their own. They are derived from cells
/// carrying values, so a column holding only formulas is invisible when a file
/// has no cached results, and on some files they under-report the populated
/// area. The XML scan counts every `<c>` element, so the wider of the two is
/// used.
fn sheet_extents(topo: &graph::Topology, trim: bool) -> Vec<(String, Option<u16>, u32, u32)> {
    topo.sheets
        .iter()
        .enumerate()
        .map(|(i, info)| {
            let (r, c) = if trim {
                (info.data_row, info.data_col)
            } else {
                (info.max_row, info.max_col)
            };
            (info.name.clone(), Some(i as u16), r, c)
        })
        // A defined name pointing at a sheet the workbook does not declare
        // makes the whole-file loader create that sheet, empty. Report it too,
        // so both paths return the same set of sheets.
        .chain(
            topo.name_only_sheets
                .iter()
                .map(|name| (name.clone(), None, 0, 0)),
        )
        .collect()
}

/// Grow the reported extents to cover spilled cells.
///
/// The engine's own dimensions grow with a spill, so the whole-file side
/// reports rows and columns a spill added; the partitioned side must
/// report them too or the row sets disagree.
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

/// Where a streamed row gets its values from.
enum RowSource {
    /// Component-by-component results plus the data cells behind them.
    Partitioned {
        topo: graph::Topology,
        store: partition::DataStore,
        values: Vec<LiteralValue>,
        /// Cells a spilled array wrote that no formula reads; served in
        /// place of the store, which does not hold them.
        spilled: HashMap<(u16, u32, u32), LiteralValue>,
    },
    /// One chunk of the formula sheet at a time. The rows of that sheet are
    /// read from the file as the caller asks for them, so no run holds the
    /// whole sheet.
    Streamed {
        run: partition::ScratchRun,
        /// The sheet the run streams. Every other sheet comes from its store.
        sheet: u16,
    },
    /// Whole-file fallback: rows are read straight off the evaluated workbook.
    Whole { wb: Workbook },
}

/// Yields `(sheet, row_number, values)` one row at a time.
///
/// Returning the entire grid materialises every cell as a Python object at
/// once, which on a large sheet costs more than the evaluation itself. Streaming
/// lets a caller convert each row and drop it, so only one row is live at a
/// time.
///
/// Marked unsendable because it owns the evaluation state and is meant to be
/// consumed by the thread that created it.
#[pyclass(unsendable)]
struct RowIter {
    source: RowSource,
    /// Per sheet: name, its index in the topology, and its extent.
    sheets: Vec<(String, Option<u16>, u32, u32)>,
    sheet: usize,
    row: u32,
    /// The strategy that produced these rows: `partitioned` (a reused chunk
    /// workbook, whether its inputs streamed or came from a store),
    /// `components` or `whole`.
    strategy: &'static str,
    /// Why a faster layout was not used, when one was refused.
    reason: Option<String>,
}

#[pymethods]
impl RowIter {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// The strategy that produced these rows.
    #[getter]
    fn strategy(&self) -> &'static str {
        self.strategy
    }

    /// Why a faster layout was not used, or `None` when none was refused.
    #[getter]
    fn fallback_reason(&self) -> Option<String> {
        self.reason.clone()
    }

    fn __next__(&mut self, py: Python<'_>) -> PyResult<Option<PyObject>> {
        loop {
            // The sheet entry is copied because the row source is borrowed
            // again below, and both live in `self`.
            let Some((name, si, max_row, max_col)) = self.sheets.get(self.sheet).cloned() else {
                return Ok(None);
            };
            let (name, si, max_row, max_col) = (name, si, max_row, max_col);
            if self.row > max_row || max_row == 0 || max_col == 0 {
                self.sheet += 1;
                self.row = 1;
                continue;
            }
            let rr = self.row;
            self.row += 1;
            let py_row = PyList::empty(py);
            match &mut self.source {
                RowSource::Partitioned { topo, store, values, spilled } => {
                    for cc in 1..=max_col {
                        // A formula cell takes its computed value; anything
                        // else comes from the store, and a miss means it is
                        // blank. Cells a spill wrote override the store.
                        match si.and_then(|s| topo.index.get(&(s, rr, cc))) {
                            Some(&i) => py_row.append(literal_to_py(py, &values[i as usize])?)?,
                            None => {
                                let v = si
                                    .and_then(|s| spilled.get(&(s, rr, cc)).cloned())
                                    .or_else(|| si.and_then(|s| store.get(s, rr, cc)))
                                    .unwrap_or(LiteralValue::Empty);
                                py_row.append(literal_to_py(py, &v)?)?;
                            }
                        }
                    }
                }
                RowSource::Streamed { run, sheet } => {
                    if si == Some(*sheet) {
                        // The row holds the values of the data cells and the
                        // results of the formula cells, in column order.
                        let cells = run.row(rr).map_err(|e| {
                            PyRuntimeError::new_err(format!("streamed row failed: {e}"))
                        })?;
                        let mut next = 0usize;
                        for cc in 1..=max_col {
                            let v = match cells.get(next) {
                                Some((c, v)) if *c == cc => {
                                    next += 1;
                                    v.clone()
                                }
                                _ => LiteralValue::Empty,
                            };
                            py_row.append(literal_to_py(py, &v)?)?;
                        }
                    } else {
                        // Sheets the run does not stream hold data only, so
                        // their rows come from its store.
                        for cc in 1..=max_col {
                            let v = si
                                .and_then(|s| run.other_value(s, rr, cc))
                                .unwrap_or(LiteralValue::Empty);
                            py_row.append(literal_to_py(py, &v)?)?;
                        }
                    }
                }
                RowSource::Whole { wb } => {
                    let addr = RangeAddress::new(name.clone(), rr, 1, rr, max_col)
                        .map_err(|e| PyRuntimeError::new_err(format!("range failed: {e}")))?;
                    for row in wb.read_range(&addr) {
                        for cell in &row {
                            py_row.append(literal_to_py(py, cell)?)?;
                        }
                    }
                }
            }
            return Ok(Some((name.clone(), rr, py_row).into_py_any(py)?));
        }
    }
}

/// Build the `RowIter` for the whole-file fallback.
///
/// `eval_rows` uses this whenever `decide` refuses to partition the file.
fn whole_row_iter(data: Vec<u8>, topo: &graph::Topology, trim: bool, verdict: &'static str) -> PyResult<RowIter> {
    let mut wb = load_workbook(data)?;
    wb.evaluate_all()
        .map_err(|e| PyRuntimeError::new_err(format!("evaluate_all failed: {e}")))?;
    // The fallback path reports the engine's dimensions, so trimming has to
    // come from the same XML scan that the partitioned path uses.
    let trimmed = sheet_extents(topo, true);
    let sheets = wb
        .sheet_names()
        .into_iter()
        .map(|n| {
            let (r, c) = wb.sheet_dimensions(&n).unwrap_or((0, 0));
            if !trim {
                return (n, None, r, c);
            }
            match trimmed.iter().find(|(name, ..)| *name == n) {
                Some(&(_, _, dr, dc)) => (n, None, r.min(dr), c.min(dc)),
                None => (n, None, 0, 0),
            }
        })
        .collect();
    Ok(RowIter {
        source: RowSource::Whole { wb },
        sheets,
        sheet: 0,
        row: 1,
        strategy: "whole",
        reason: Some(verdict.to_string()),
    })
}

/// Evaluate a workbook and stream its rows, partitioning when that is safe.
#[pyfunction]
#[pyo3(signature = (data, budget_cells = None, max_ratio = DEFAULT_MAX_RATIO, min_formulas = DEFAULT_MIN_FORMULAS, trim = false, prelude = true, lookup_budget = partition::DEFAULT_LOOKUP_BUDGET, now = None))]
fn eval_rows(
    data: Vec<u8>,
    budget_cells: Option<u64>,
    max_ratio: f64,
    min_formulas: usize,
    trim: bool,
    prelude: bool,
    lookup_budget: u64,
    now: Option<f64>,
) -> PyResult<RowIter> {
    clock::begin_run_at(now);
    eval_rows_impl(data, budget_cells, max_ratio, min_formulas, trim, prelude, lookup_budget)
        .map(|(rows, _)| rows)
}

/// Evaluate a workbook and stream its rows, partitioning when that is safe.
///
/// Shared by the `eval_rows` pyfunction and `_benchmark_rows(mode="auto")`.
fn eval_rows_impl(
    data: Vec<u8>,
    budget_cells: Option<u64>,
    max_ratio: f64,
    min_formulas: usize,
    trim: bool,
    prelude: bool,
    lookup_budget: u64,
) -> PyResult<(RowIter, RunInfo)> {
    let mut topo = prepare(&data, prelude).topo;
    let decision = decide(&topo, max_ratio, min_formulas, partition::DEFAULT_CHUNK_ROWS, lookup_budget);
    let budget = budget_cells.unwrap_or(partition::DEFAULT_BUDGET_CELLS);
    let n_batches = partition::plan_batches(&topo, budget).len();

    let Some(layout) = decision.layout else {
        let verdict = decision.verdict.expect("no layout implies a verdict");
        let rows = whole_row_iter(data, &topo, trim, verdict)?;
        let info = RunInfo { impl_mode: "whole", stream_refusal: None, chunk: None, n_batches };
        return Ok((rows, info));
    };

    let mut sheets = sheet_extents(&topo, trim);

    match layout {
        Layout::Chunks(plan) => {
            let chunk = Some((
                plan.n_chunks(),
                plan.last_row - plan.first_row + 1,
                plan.carry,
                plan.n_data_cols(),
            ));
            // Streaming reads the formula sheet's input rows from the file as
            // the caller asks for them, so it never holds the whole sheet. A
            // clone survives a failed attempt, so the store path below
            // evaluates the same chunks without replanning.
            match partition::ScratchRun::open(&data, &mut topo, plan.clone()) {
                Ok(run) => {
                    let sheet = run.sheet();
                    let rows = RowIter {
                        source: RowSource::Streamed { run, sheet },
                        sheets,
                        sheet: 0,
                        row: 1,
                        strategy: "partitioned",
                        reason: None,
                    };
                    let info = RunInfo { impl_mode: "streamed", stream_refusal: None, chunk, n_batches };
                    Ok((rows, info))
                }
                // The file meets the chunk contract but its sheet cannot be
                // streamed (rows out of order, unsupported compression). The
                // store path evaluates the same chunks and returns the same
                // rows.
                Err(e) => {
                    let store = partition::DataStore::reread(&data, &topo)
                        .map_err(|e| PyRuntimeError::new_err(format!("reading values failed: {e}")))?;
                    let evaluated = run_partitioned(&store, &topo, budget, &Layout::Chunks(plan))
                        .map_err(|e| PyRuntimeError::new_err(format!("partitioned eval failed: {e}")))?;
                    grow_extents(&mut sheets, &evaluated.spilled);
                    let rows = RowIter {
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
                        reason: None,
                    };
                    let info = RunInfo { impl_mode: "scratch", stream_refusal: Some(e), chunk, n_batches };
                    Ok((rows, info))
                }
            }
        }
        Layout::Components => {
            let store = partition::DataStore::load(&mut topo);
            let evaluated = run_partitioned(&store, &topo, budget, &Layout::Components)
                .map_err(|e| PyRuntimeError::new_err(format!("partitioned eval failed: {e}")))?;
            grow_extents(&mut sheets, &evaluated.spilled);
            let rows = RowIter {
                source: RowSource::Partitioned {
                    topo,
                    store,
                    values: evaluated.values,
                    spilled: evaluated.spilled.into_iter().map(|(s, r, c, v)| ((s, r, c), v)).collect(),
                },
                sheets,
                sheet: 0,
                row: 1,
                strategy: "components",
                reason: decision.chunk_reason.map(|s| s.to_string()),
            };
            let info = RunInfo { impl_mode: "components", stream_refusal: None, chunk: None, n_batches };
            Ok((rows, info))
        }
    }
}

#[pyfunction]
#[pyo3(signature = (data, budget_cells = None, max_ratio = DEFAULT_MAX_RATIO, min_formulas = DEFAULT_MIN_FORMULAS, prelude = true, lookup_budget = partition::DEFAULT_LOOKUP_BUDGET, now = None))]
fn eval_partitioned(
    py: Python<'_>,
    data: Vec<u8>,
    budget_cells: Option<u64>,
    max_ratio: f64,
    min_formulas: usize,
    prelude: bool,
    lookup_budget: u64,
    now: Option<f64>,
) -> PyResult<PyObject> {
    clock::begin_run_at(now);
    let mut topo = prepare(&data, prelude).topo;
    let decision = decide(&topo, max_ratio, min_formulas, partition::DEFAULT_CHUNK_ROWS, lookup_budget);
    let Some(layout) = decision.layout else {
        return eval_whole(py, data);
    };

    let store = partition::DataStore::load(&mut topo);
    let budget = budget_cells.unwrap_or(partition::DEFAULT_BUDGET_CELLS);
    let evaluated = run_partitioned(&store, &topo, budget, &layout)
        .map_err(|e| PyRuntimeError::new_err(format!("partitioned eval failed: {e}")))?;
    let mut extents = sheet_extents(&topo, false);
    grow_extents(&mut extents, &evaluated.spilled);
    let spilled: HashMap<(u16, u32, u32), LiteralValue> =
        evaluated.spilled.into_iter().map(|(s, r, c, v)| ((s, r, c), v)).collect();

    let out = PyDict::new(py);
    for (name, si, max_row, max_col) in extents {
        let sheet_list = PyList::empty(py);
        if max_row >= 1 && max_col >= 1 {
            for rr in 1..=max_row {
                let py_row = PyList::empty(py);
                for cc in 1..=max_col {
                    // A formula cell takes its computed value; everything else
                    // comes from the store, and a miss means the cell is blank.
                    match si.and_then(|s| topo.index.get(&(s, rr, cc))) {
                        Some(&i) => {
                            py_row.append(literal_to_py(py, &evaluated.values[i as usize])?)?
                        }
                        None => {
                            let v = si
                                .and_then(|s| spilled.get(&(s, rr, cc)).cloned())
                                .or_else(|| si.and_then(|s| store.get(s, rr, cc)))
                                .unwrap_or(LiteralValue::Empty);
                            py_row.append(literal_to_py(py, &v)?)?;
                        }
                    }
                }
                sheet_list.append(py_row)?;
            }
        }
        out.set_item(name, sheet_list)?;
    }
    Ok(out.into())
}

/// Force one execution mode for a benchmark comparison.
///
/// Not part of the public API: identical files may exercise a different mode
/// as the code evolves, and the default (`"auto"`) is what `eval_rows` does.
/// `mode` is `"auto"`, `"streamed"` (chunk layout, inputs read from the
/// file), `"scratch"` (chunk layout, inputs from a preloaded store),
/// `"components"` or `"whole"`. A forced mode fails with a clear error when
/// the file does not qualify for it, so a benchmark never silently measures a
/// different implementation than the one it asked for.
///
/// Returns `(rows, report)`. `report` holds `requested_mode`, `selected_mode`
/// (which, unlike `rows.strategy`, distinguishes streamed from store-fed chunk
/// runs), `fallback_reason`, `stream_refusal`, the component-batch estimate
/// `n_batches`, and the chunk-plan stats when the chunk layout ran.
#[pyfunction]
#[pyo3(signature = (data, mode = "auto".to_string(), budget_cells = None, max_ratio = DEFAULT_MAX_RATIO, min_formulas = DEFAULT_MIN_FORMULAS, trim = false, prelude = true, lookup_budget = partition::DEFAULT_LOOKUP_BUDGET))]
fn _benchmark_rows(
    py: Python<'_>,
    data: Vec<u8>,
    mode: String,
    budget_cells: Option<u64>,
    max_ratio: f64,
    min_formulas: usize,
    trim: bool,
    prelude: bool,
    lookup_budget: u64,
) -> PyResult<(RowIter, PyObject)> {
    clock::begin_run();
    fn report(py: Python<'_>, requested: &str, rows: &RowIter, info: &RunInfo) -> PyResult<PyObject> {
        let out = PyDict::new(py);
        out.set_item("requested_mode", requested)?;
        out.set_item("selected_mode", info.impl_mode)?;
        out.set_item("fallback_reason", rows.fallback_reason())?;
        match &info.stream_refusal {
            Some(e) => out.set_item("stream_refusal", e.clone())?,
            None => out.set_item("stream_refusal", py.None())?,
        }
        out.set_item("n_batches", info.n_batches)?;
        if let Some((n_chunks, rows, carry, data_cols)) = info.chunk {
            out.set_item("n_chunks", n_chunks)?;
            out.set_item("chunk_rows", rows)?;
            out.set_item("carry_rows", carry)?;
            out.set_item("data_cols", data_cols)?;
        }
        Ok(out.into())
    }

    let budget_cells = budget_cells.unwrap_or(partition::DEFAULT_BUDGET_CELLS);
    match mode.as_str() {
        "auto" => {
            let (rows, info) = eval_rows_impl(
                data,
                Some(budget_cells),
                max_ratio,
                min_formulas,
                trim,
                prelude,
                lookup_budget,
            )?;
            let rep = report(py, "auto", &rows, &info)?;
            Ok((rows, rep))
        }
        "streamed" | "scratch" => {
            let mut topo = prepare(&data, prelude).topo;
            let decision =
                decide(&topo, max_ratio, min_formulas, partition::DEFAULT_CHUNK_ROWS, lookup_budget);
            let Some(Layout::Chunks(plan)) = decision.layout else {
                let why = decision
                    .verdict
                    .or(decision.chunk_reason)
                    .unwrap_or("the file has no formulas");
                return Err(PyRuntimeError::new_err(format!(
                    "mode={mode:?} requires the chunk layout, which this file does not meet: {why}"
                )));
            };
            let sheets = sheet_extents(&topo, trim);
            let n_batches = partition::plan_batches(&topo, budget_cells).len();
            let chunk = Some((
                plan.n_chunks(),
                plan.last_row - plan.first_row + 1,
                plan.carry,
                plan.n_data_cols(),
            ));
            let (rows, info) = if mode == "streamed" {
                let run = partition::ScratchRun::open(&data, &mut topo, plan).map_err(|e| {
                    PyRuntimeError::new_err(format!(
                        "mode=\"streamed\" cannot stream this file's sheet: {e}"
                    ))
                })?;
                let sheet = run.sheet();
                let rows = RowIter {
                    source: RowSource::Streamed { run, sheet },
                    sheets,
                    sheet: 0,
                    row: 1,
                    strategy: "partitioned",
                    reason: None,
                };
                (rows, RunInfo { impl_mode: "streamed", stream_refusal: None, chunk, n_batches })
            } else {
                let store = partition::DataStore::load(&mut topo);
                let evaluated = run_partitioned(&store, &topo, budget_cells, &Layout::Chunks(plan))
                    .map_err(|e| PyRuntimeError::new_err(format!("partitioned eval failed: {e}")))?;
                let rows = RowIter {
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
                    reason: None,
                };
                (rows, RunInfo { impl_mode: "scratch", stream_refusal: None, chunk, n_batches })
            };
            let rep = report(py, &mode, &rows, &info)?;
            Ok((rows, rep))
        }
        "components" => {
            let mut topo = prepare(&data, prelude).topo;
            // Eligibility as if the chunk layout were unavailable: the
            // component evaluator does not recreate defined names, and it
            // always needs the ratio gate.
            if let Some(why) = partition_verdict(&topo, max_ratio, min_formulas, false) {
                return Err(PyRuntimeError::new_err(format!(
                    "mode=\"components\" is not legal for this file: {why}"
                )));
            }
            let sheets = sheet_extents(&topo, trim);
            let n_batches = partition::plan_batches(&topo, budget_cells).len();
            let store = partition::DataStore::load(&mut topo);
            let evaluated = run_partitioned(&store, &topo, budget_cells, &Layout::Components)
                .map_err(|e| PyRuntimeError::new_err(format!("partitioned eval failed: {e}")))?;
            let rows = RowIter {
                source: RowSource::Partitioned {
                    topo,
                    store,
                    values: evaluated.values,
                    spilled: evaluated.spilled.into_iter().map(|(s, r, c, v)| ((s, r, c), v)).collect(),
                },
                sheets,
                sheet: 0,
                row: 1,
                strategy: "components",
                reason: None,
            };
            let info = RunInfo { impl_mode: "components", stream_refusal: None, chunk: None, n_batches };
            let rep = report(py, "components", &rows, &info)?;
            Ok((rows, rep))
        }
        "whole" => {
            let topo = prepare(&data, prelude).topo;
            let n_batches = partition::plan_batches(&topo, budget_cells).len();
            let rows = whole_row_iter(data, &topo, trim, "forced by the caller")?;
            let info = RunInfo { impl_mode: "whole", stream_refusal: None, chunk: None, n_batches };
            let rep = report(py, "whole", &rows, &info)?;
            Ok((rows, rep))
        }
        other => Err(PyRuntimeError::new_err(format!(
            "unknown mode {other:?}: expected auto, streamed, scratch, components or whole"
        ))),
    }
}

#[pymodule]
fn formualizer_partitioned(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(probe_dims, m)?)?;
    m.add_function(wrap_pyfunction!(eval_grid, m)?)?;
    m.add_function(wrap_pyfunction!(components, m)?)?;
    m.add_function(wrap_pyfunction!(partition_plan, m)?)?;
    m.add_function(wrap_pyfunction!(eval_partitioned, m)?)?;
    m.add_function(wrap_pyfunction!(eval_rows, m)?)?;
    m.add_function(wrap_pyfunction!(_benchmark_rows, m)?)?;
    m.add_class::<RowIter>()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{cell_f, cell_v, xlsx};

    /// A solid block of row-local formulas: the chunk layout's shape.
    fn row_local_block(rows: u32) -> Vec<u8> {
        let mut sheet = String::new();
        for r in 1..=rows {
            sheet.push_str(&cell_v(&format!("A{r}"), &r.to_string()));
            sheet.push_str(&cell_f(&format!("B{r}"), &format!("A{r}*2")));
        }
        xlsx(&[("S", &sheet)])
    }

    /// A formula that reads a later row, which the chunk layout must refuse.
    fn forward_ref_block() -> Vec<u8> {
        let sheet = format!("{}{}{}", cell_f("A1", "A2+1"), cell_f("A2", "7*6"), cell_v("A3", "0"));
        xlsx(&[("S", &sheet)])
    }

    fn topology(data: &[u8]) -> graph::Topology {
        prepare(data, true).topo
    }

    #[test]
    fn scoped_fixed_names_use_components() {
        let data = crate::testkit::xlsx_with_defined_names(
            &[("Scope", &cell_f("B1", "Local")), ("Target", &cell_v("A1", "7"))],
            r#"<definedName name="Local" localSheetId="0">Target!$A$1</definedName>"#,
        );
        let topo = topology(&data);
        assert_eq!(topo.named_refs, 1);
        assert_eq!(partition_verdict(&topo, 1.0, 0, false), None);
        assert!(partition::plan_scratch(&topo, 100, partition::DEFAULT_LOOKUP_BUDGET).is_err());
    }

    #[test]
    fn array_annotations_do_not_gate_partitioning() {
        for formula in ["SUM(A1:A3)", "TRANSPOSE(A1:A3)"] {
            let sheet = format!(
                r#"<c r="B5"><f t="array" ref="B5:D5">{formula}</f><v>0</v></c>"#
            );
            let topo = topology(&xlsx(&[("S", &sheet)]));
            assert_eq!(topo.array_formulas, 1);
            assert_eq!(partition_verdict(&topo, 1.0, 0, false), None);
        }
    }

    #[test]
    fn text_criteria_aggregates_partition_now() {
        // At the pinned engine rev both text lanes render numeric and
        // boolean cells to their string forms, so a text criterion answers
        // identically in a loader-built and an incrementally-built
        // workbook (and follows Excel coercion). These calls no longer
        // gate; the counter stays as a diagnostic.
        let cases = [
            "COUNTIF(G$2:G$37,\"\")",
            "COUNTIF(G$2:G$37,\"1\")",
            "COUNTIFS(G$2:G$37,\"\",H$2:H$37,\">0\")",
            "SUMIF(G$2:G$37,\"\")",
            "SUMIF(G$2:G$37,\"abc\")",
            "COUNTIF(G$2:G$37,IF(A1=\"\",\"\",\">5\"))",
        ];
        for formula in cases {
            let sheet = cell_f("G43", formula);
            let t = topology(&xlsx(&[("S", &sheet)]));
            // The text-criteria gate is gone; a later gate such as the
            // ratio check may still fire on this empty fixture.
            assert_ne!(
                partition_verdict(&t, DEFAULT_MAX_RATIO, 0, false),
                Some("text-criteria aggregates"),
                "{formula}"
            );
            assert!(t.text_criteria_ifs > 0, "{formula} must still be counted");
        }
    }

    fn rejected_formulas_are_never_reported_as_formula_free() {
        for min in [0, DEFAULT_MIN_FORMULAS] {
            let t = topology(&xlsx(&[("S", &cell_f("B1", "A1+"))]));
            assert_eq!(partition_verdict(&t, DEFAULT_MAX_RATIO, min, false), Some("unparsed formulas"));
            let t = topology(&xlsx(&[("S", r#"<c r="A1"><f t="shared" si="99"/></c>"#)]));
            assert_eq!(partition_verdict(&t, DEFAULT_MAX_RATIO, min, false), Some("unresolved formula cells"));
            let t = topology(&xlsx(&[("S", &cell_v("A1", "1"))]));
            assert_eq!(partition_verdict(&t, DEFAULT_MAX_RATIO, min, false), Some("no formulas"));
        }
    }

    #[test]
    fn a_chunkable_file_prefers_the_chunk_layout() {
        let topo = topology(&row_local_block(20));
        let d = decide(&topo, DEFAULT_MAX_RATIO, 0, partition::DEFAULT_CHUNK_ROWS, partition::DEFAULT_LOOKUP_BUDGET);
        assert!(d.verdict.is_none());
        assert!(matches!(d.layout, Some(Layout::Chunks(_))));
        assert!(d.chunk_reason.is_none());
    }

    #[test]
    fn an_array_capable_file_takes_the_component_layout() {
        // Row-local, so the chunk contract itself holds; the spill-capable
        // template routes it to components, which resolve spills against
        // whole-file occupancy.
        let mut sheet = String::new();
        for r in 1..=20u32 {
            sheet.push_str(&cell_v(&format!("A{r}"), &r.to_string()));
            sheet.push_str(&cell_f(&format!("B{r}"), &format!("INDEX(A{r}:A{r},0)")));
        }
        let topo = topology(&xlsx(&[("S", &sheet)]));
        let d = decide(&topo, DEFAULT_MAX_RATIO, 0, partition::DEFAULT_CHUNK_ROWS, partition::DEFAULT_LOOKUP_BUDGET);
        assert!(d.verdict.is_none(), "{:?}", d.verdict);
        assert!(matches!(d.layout, Some(Layout::Components)));
        assert_eq!(d.chunk_reason, Some("array-capable formulas"));
    }

    #[test]
    fn a_component_only_file_reports_the_chunk_refusal() {
        let topo = topology(&forward_ref_block());
        let d = decide(&topo, DEFAULT_MAX_RATIO, 0, partition::DEFAULT_CHUNK_ROWS, partition::DEFAULT_LOOKUP_BUDGET);
        assert!(d.verdict.is_none());
        assert!(matches!(d.layout, Some(Layout::Components)));
        assert!(d.chunk_reason.is_some(), "the refusal must reach the report");
    }

    #[test]
    fn whole_fallback_keeps_the_chunk_diagnostic() {
        // Chunk-eligible but below the size gate: the report must not invent
        // a chunk failure for a plan that succeeded.
        let topo = topology(&row_local_block(5));
        let d = decide(&topo, DEFAULT_MAX_RATIO, usize::MAX, partition::DEFAULT_CHUNK_ROWS, partition::DEFAULT_LOOKUP_BUDGET);
        assert!(d.verdict.is_some());
        assert!(d.layout.is_none());
        assert!(d.chunk_reason.is_none(), "the chunk plan was fine; the size gate refused");

        // Not chunk-eligible and below the size gate: the real refusal survives.
        let topo = topology(&forward_ref_block());
        let d = decide(&topo, DEFAULT_MAX_RATIO, usize::MAX, partition::DEFAULT_CHUNK_ROWS, partition::DEFAULT_LOOKUP_BUDGET);
        assert!(d.verdict.is_some());
        assert!(d.chunk_reason.is_some(), "the plan refusal explains the fallback");
    }
}
