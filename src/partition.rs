//! Evaluate a workbook in bounded pieces, releasing each piece before the
//! next.
//!
//! Two layouts cover the partitionable files, and `lib.rs` picks between them
//! automatically — the caller never chooses. A solid block of repeated
//! row-local formulas uses the chunk layout: one short reused workbook,
//! evaluated a chunk of rows at a time. Every other shape the dependency
//! closure allows uses the component layout: one fresh `Workbook` per batch
//! of components, holding that batch's formulas and their data inputs. Either
//! way the evaluator reads the results and releases the piece, so peak memory
//! tracks the piece, not the whole file.
//!
//! `graph::build` joins each formula with every formula that it reads. Thus,
//! each component contains all its formula dependencies. Every other input is
//! a data cell that the evaluator copies into the batch.
//!
//! ## The chunk layout
//!
//! A solid block of repeated formulas can use one short scratch workbook. The
//! evaluator places the formulas once and reuses them for every row chunk. Each
//! chunk changes only the input values.
//!
//! A chunk plan (`plan_scratch`) must meet these requirements:
//!
//! - One sheet holds formulas.
//! - Each formula column repeats one template over the same rows without gaps.
//! - References on that sheet name the current row or at most
//!   `MAX_CARRY_ROWS` earlier rows.
//! - No reference names a later row or uses an absolute row anchor.
//! - References to another sheet use absolute coordinates.
//! - No formula uses a nondeterministic function or reads its own position.
//! - Lookup work does not exceed `DEFAULT_LOOKUP_BUDGET`.
//! - Every defined name a formula uses is workbook-scoped and targets a fixed,
//!   fully absolute range on another sheet. See "Defined names" below.
//!
//! Carry rows hold plain values from the preceding chunk. They let a dependency
//! chain cross chunk boundaries without moving formulas to different relative
//! rows.
//!
//! ### Defined names
//!
//! The scratch workbook defines each used name over its copied static range
//! before it places the formulas. A name therefore reads the same cells that
//! the source workbook holds.
//!
//! Three limits keep that equality true:
//!
//! - The target must be on another sheet. The formula sheet holds one chunk at
//!   a time, so a target on it names rows the chunk does not hold.
//! - Every coordinate must be absolute. A relative coordinate would move with
//!   the formula while the copied target does not.
//! - The scope must be the workbook. `Workbook::define_named_range` binds a
//!   sheet scope to the target sheet. OOXML `localSheetId` names the sheet the
//!   name is visible on, which can be a different sheet.
//!
//! ### Streaming the chunk inputs
//!
//! `ScratchRun` keeps one chunk of the formula sheet. It reads, evaluates and
//! returns that chunk before it reads the next chunk. Other sheets stay in the
//! value store because they can hold lookup tables and returned data rows.
//!
//! The stream copies one compressed worksheet part from the archive and
//! inflates it as needed. It uses the store path if rows are not in ascending
//! order, or if the part uses an unsupported compression method.
//!
//! ### The lookup budget
//!
//! The engine can index repeated exact lookups. Other lookup modes can still
//! scan the table, and each chunk can require new index work.
//!
//! `Topology::lookup_work` reports a conservative full-scan estimate. The
//! chunk layout rejects work above `DEFAULT_LOOKUP_BUDGET`, and the caller
//! falls back to the component layout.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use formualizer::common::value::LiteralValue;
use formualizer::common::{CellAddress, RangeAddress};
use formualizer::eval::engine::inspect::{SnapshotOptions, SpillRole};
use formualizer::parse::parser::{parse, ASTNode, ASTNodeType, ReferenceType};
use formualizer::workbook::{NamedRangeScope, Workbook, WorkbookConfig};

use crate::graph::{NameScope, RangeRef, RawRef, StaticName, Topology};
use crate::values::{SheetStream, Values};

/// Define one workbook-scoped name over its fixed target range.
fn define_static_name(wb: &mut Workbook, topo: &Topology, name: &StaticName) -> Result<(), String> {
    let (sheet, r0, c0, r1, c1) = name.target;
    let address = RangeAddress::new(topo.sheets[sheet as usize].name.clone(), r0, c0, r1, c1)
        .map_err(|e| e.to_string())?;
    wb.define_named_range(&name.name, &address, NamedRangeScope::Workbook)
        .map_err(|e| e.to_string())
}

/// Cells per batch, the knob peak memory scales with.
///
/// Setting up a workbook costs little next to evaluating one, so a small budget
/// lowers peak memory and costs no time. Below roughly 2048 cells the set-up
/// cost starts to show and memory stops improving.
pub const DEFAULT_BUDGET_CELLS: u64 = 8_192;

/// A data value in compact form. See `crate::values::Val`.
use crate::values::Val;

/// Data values keyed by (sheet index, row, col).
///
/// Values are read from the workbook XML rather than through the evaluation
/// backend. The backend decodes an entire sheet into its own map on first
/// access and holds it, which is the largest allocation that path makes.
/// Streaming instead keeps only what is stored here: formula cells are skipped
/// because they are recomputed, and blanks are omitted.
///
/// Each sheet is held as sorted rows of sorted columns rather than as a hash
/// map, for two reasons. A hashed entry costs its key, its value and a control
/// byte at a load factor below one, while a sorted row costs a column number
/// beside the value. Copying a range into a batch workbook then walks the cells
/// the range holds instead of testing every cell of its area.
pub struct DataStore {
    sheets: Vec<SheetData>,
}

/// One sheet, as rows of (column, value) sorted by column.
#[derive(Default)]
struct SheetData {
    /// Sorted distinct row numbers.
    rows: Vec<u32>,
    /// Where each row starts in `cols`/`vals`, with a final end. Length is
    /// `rows.len() + 1`.
    starts: Vec<u32>,
    cols: Vec<u32>,
    vals: Vec<Val>,
}

impl SheetData {
    fn build(mut cells: Vec<(u32, u32, Val)>) -> SheetData {
        cells.sort_unstable_by_key(|&(r, c, _)| (r, c));
        let mut out = SheetData {
            starts: Vec::with_capacity(1),
            ..SheetData::default()
        };
        out.starts.push(0);
        for (r, c, v) in cells {
            if out.rows.last() != Some(&r) {
                out.rows.push(r);
                out.starts.push(out.cols.len() as u32);
            }
            out.cols.push(c);
            out.vals.push(v);
            *out.starts.last_mut().unwrap() = out.cols.len() as u32;
        }
        out
    }

    /// The (column, value) span of one row index.
    fn span(&self, i: usize) -> (&[u32], &[Val]) {
        let (a, b) = (self.starts[i] as usize, self.starts[i + 1] as usize);
        (&self.cols[a..b], &self.vals[a..b])
    }

    fn get(&self, row: u32, col: u32) -> Option<&Val> {
        let i = self.rows.binary_search(&row).ok()?;
        let (cols, vals) = self.span(i);
        Some(&vals[cols.binary_search(&col).ok()?])
    }

    fn len(&self) -> usize {
        self.vals.len()
    }
}

/// Valueless declared cells of one sheet that a formula range covers.
///
/// Blank presence is observable to blank-counting calls (`COUNTBLANK`, a
/// `COUNTIF`-family call with a `""` criterion), so the store keeps these
/// as blank entries. The extra entries are bounded by referenced area: a
/// blank outside every component range changes no result and is dropped,
/// which is what keeps style-only tail areas out of the store.
fn sheet_blanks(topo: &Topology, si: u16) -> Vec<(u32, u32)> {
    let blanks = match topo.blanks.get(si as usize) {
        Some(b) if !b.is_empty() => b,
        _ => return Vec::new(),
    };
    let mut ranges: Vec<(u32, u32, u32, u32)> = Vec::new();
    for comp in &topo.comp_refs {
        for &(s, r0, c0, r1, c1) in comp {
            if s == si {
                ranges.push((r0, c0, r1, c1));
            }
        }
    }
    if ranges.is_empty() {
        return Vec::new();
    }
    blanks
        .iter()
        .copied()
        .filter(|&(r, c)| {
            ranges
                .iter()
                .any(|&(r0, c0, r1, c1)| r0 <= r && r <= r1 && c0 <= c && c <= c1)
        })
        .collect()
}

impl DataStore {
    pub fn get(&self, sheet: u16, row: u32, col: u32) -> Option<LiteralValue> {
        Some(self.sheets.get(sheet as usize)?.get(row, col)?.unpack())
    }

    /// Call `f` for every value the range holds, in row then column order.
    ///
    /// Cost follows the number of values present, not the area of the range, so
    /// a reference down a sparse column is cheap.
    pub fn for_range(
        &self,
        sheet: u16,
        r0: u32,
        c0: u32,
        r1: u32,
        c1: u32,
        mut f: impl FnMut(u32, u32, LiteralValue),
    ) {
        let Some(sd) = self.sheets.get(sheet as usize) else {
            return;
        };
        let first = sd.rows.partition_point(|&r| r < r0);
        for i in first..sd.rows.len() {
            let row = sd.rows[i];
            if row > r1 {
                break;
            }
            let (cols, vals) = sd.span(i);
            let start = cols.partition_point(|&c| c < c0);
            for k in start..cols.len() {
                if cols[k] > c1 {
                    break;
                }
                f(row, cols[k], vals[k].unpack());
            }
        }
    }

    pub fn len(&self) -> usize {
        self.sheets.iter().map(|s| s.len()).sum()
    }

    /// Take the values the read stage collected and index them.
    pub fn load(topo: &mut Topology) -> Self {
        DataStore::load_without(topo, None)
    }
    /// Take the values of every sheet except one, which stays empty.
    ///
    /// The streaming path reads the values of its formula sheet row by row, so
    /// holding them here as well would defeat the purpose. It still needs the
    /// other sheets: they carry the lookup tables it copies once, and the rows
    /// it returns for those sheets.
    pub fn load_without(topo: &mut Topology, skip: Option<u16>) -> Self {
        let raw = std::mem::take(&mut topo.values);
        let mut sheets = Vec::with_capacity(raw.len());
        for (si, cells) in raw.into_iter().enumerate() {
            if skip == Some(si as u16) {
                sheets.push(SheetData::default());
                continue;
            }
            let mut cells = Self::keep(topo, si as u16, cells);
            cells.extend(
                sheet_blanks(topo, si as u16)
                    .into_iter()
                    .map(|(r, c)| (r, c, Val::Empty)),
            );
            sheets.push(SheetData::build(cells));
        }
        DataStore { sheets }
    }

    /// Read the values back out of the file.
    ///
    /// The read stage hands its values to the first store that asks, so a
    /// caller that took them for a streamed run and then could not stream has
    /// nothing left to build a store from.
    pub fn reread(data: &[u8], topo: &Topology) -> Result<Self, String> {
        let mut zip =
            zip::ZipArchive::new(std::io::Cursor::new(data)).map_err(|e| e.to_string())?;
        let decoder = crate::values::Values::open(&mut zip);
        let parts = crate::graph::sheet_parts(&mut zip);
        let mut sheets = Vec::with_capacity(parts.len());
        for (si, (_name, path)) in parts.iter().enumerate() {
            let mut cells: Vec<(u32, u32, Val)> = Vec::new();
            decoder.read_sheet(&mut zip, path, |r, c, v| cells.push((r, c, Val::pack(v))));
            let mut cells = Self::keep(topo, si as u16, cells);
            cells.extend(
                sheet_blanks(topo, si as u16)
                    .into_iter()
                    .map(|(r, c)| (r, c, Val::Empty)),
            );
            sheets.push(SheetData::build(cells));
        }
        Ok(DataStore { sheets })
    }

    /// Drop the cells that hold a formula.
    ///
    /// Their value is recomputed, so a cached one would only compete with it
    /// for memory. Which cells those are is settled only once every sheet has
    /// been read, so the read stage cannot drop them itself.
    fn keep(topo: &Topology, si: u16, cells: Vec<(u32, u32, Val)>) -> Vec<(u32, u32, Val)> {
        cells
            .into_iter()
            .filter(|&(r, c, _)| !topo.index.contains_key(&(si, r, c)))
            .collect()
    }
}

fn shift_coord(v: u32, d: i64, is_abs: bool) -> u32 {
    if is_abs {
        return v;
    }
    // Excel never emits a shared-formula member whose relative reference falls
    // off the sheet, so clamping here is unreachable in practice.
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

/// Rebuild a shared formula's AST as seen from a member cell.
///
/// A shared formula is stored once at its master anchor; every member means the
/// same expression with relative references moved by the member's offset. The
/// pretty printer renders references from the parsed `reference`, so shifting
/// that alone is enough and no text round-trip is needed.
pub fn shift_ast(node: &ASTNode, dr: i64, dc: i64) -> ASTNode {
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

/// Pack components into batches under a cell budget.
///
/// Components keep their natural order so neighbours, which usually read
/// neighbouring data, land in the same batch. A component costing more than the
/// budget forms a batch on its own: it cannot be split without breaking the
/// dependency closure.
///
/// Cost is the cells a batch materialises, not bounding-box area. A component
/// of a few formulas reaching across a sheet spans a large box while costing
/// almost nothing to evaluate, and budgeting by area pays a workbook set-up per
/// such component.
///
/// A batch copies each distinct range once, so a shared lookup table is
/// charged to the first component that reads it and nothing after. The
/// running cost grows by a component's formulas plus the summed areas of the
/// distinct ranges the batch does not already hold (overlapping but unequal
/// ranges still double-count their intersection).
fn range_area(r: &RangeRef) -> u64 {
    (r.3 - r.1 + 1) as u64 * (r.4 - r.2 + 1) as u64
}

pub fn plan_batches(topo: &Topology, budget_cells: u64) -> Vec<Vec<u32>> {
    let mut batches: Vec<Vec<u32>> = Vec::new();
    let mut cur: Vec<u32> = Vec::new();
    let mut cur_cells: u64 = 0;
    // Ranges the current batch already holds.
    let mut cur_ranges: HashSet<RangeRef> = HashSet::new();
    for (c, cells) in topo.comp_cells.iter().enumerate() {
        let mut add = cells.len() as u64;
        for r in &topo.comp_refs[c] {
            if !cur_ranges.contains(r) {
                add += range_area(r);
            }
        }
        if !cur.is_empty() && cur_cells + add.max(1) > budget_cells {
            batches.push(std::mem::take(&mut cur));
            cur_cells = 0;
            cur_ranges.clear();
            add = cells.len() as u64;
            for r in &topo.comp_refs[c] {
                add += range_area(r);
            }
        }
        cur.push(c as u32);
        cur_cells += add.max(1);
        cur_ranges.extend(topo.comp_refs[c].iter().copied());
    }
    if !cur.is_empty() {
        batches.push(cur);
    }
    batches
}

#[derive(Debug)]
pub struct Evaluated {
    /// Computed value per formula cell, indexed as in `Topology::cells`.
    pub values: Vec<LiteralValue>,
    pub n_batches: usize,
    pub biggest_batch_cells: u64,
    /// Cells a spilled array wrote that no formula reads. The whole-file
    /// run spills the same cells, so row and grid output serves these in
    /// place of the store. Anchors are formula cells and come from
    /// `values`.
    pub spilled: Vec<(u16, u32, u32, LiteralValue)>,
}

/// Evaluate every component, one batch of workbooks at a time.
pub fn run(
    store: &DataStore,
    topo: &Topology,
    budget_cells: u64,
) -> Result<Evaluated, String> {
    let batches = plan_batches(topo, budget_cells);
    let mut values = vec![LiteralValue::Empty; topo.cells.len()];
    let mut biggest_batch_cells = 0u64;
    let mut spilled: Vec<(u16, u32, u32, LiteralValue)> = Vec::new();
    // Extents every resolved batch committed, for the overlap guard.
    let mut all_extents: Vec<(u16, RangeAddress)> = Vec::new();

    for batch in &batches {
        let mut cells: Vec<u32> = Vec::new();
        let mut ranges: Vec<RangeRef> = Vec::new();
        for &c in batch {
            cells.extend_from_slice(&topo.comp_cells[c as usize]);
            ranges.extend(topo.comp_refs[c as usize].iter().copied());
        }
        // Identical ranges collapse here, which matters when thousands of
        // formulas all read the same lookup table. Sorting before the dedup
        // also fixes the order every later step sees: sheets are added to the
        // batch workbook and inputs are copied in this order, so it must not
        // vary between runs. A `HashSet` deduplicates just as well but leaves
        // that order arbitrary, which is why this is a sorted `Vec`.
        ranges.sort_unstable();
        ranges.dedup();
        // The batch copies each distinct range once, so its cost is the
        // formulas plus the union area, matching `plan_batches`.
        let batch_cells =
            cells.len() as u64 + ranges.iter().map(range_area).sum::<u64>();
        biggest_batch_cells = biggest_batch_cells.max(batch_cells);

        // The screen is a superset of the spilling shapes, so a batch with no
        // flagged template cannot spill and pays nothing for the check. A
        // flagged template costs one engine inspection per anchor (about two
        // microseconds each).
        let candidates: Vec<u32> = cells
            .iter()
            .copied()
            .filter(|&i| {
                topo.array_capable
                    .get(topo.cells[i as usize].ast as usize)
                    .copied()
                    .unwrap_or(false)
            })
            .collect();

        let mut wb = build_batch_workbook(store, topo, batch, &cells, &ranges, &[])?;
        wb.evaluate_all().map_err(|e| e.to_string())?;
        let extents = spilled_anchors(&wb, topo, &candidates)?;
        if extents.is_empty() {
            read_values(&wb, topo, &cells, &mut values)?;
            continue;
        }

        // Whole-file equivalence guards, checked on pass-1 extents: the
        // extents the whole run would spill into. A footprint that covers
        // a formula cell of another batch makes the whole run fail its
        // evaluation outright (BlockedByFormula) where a seeded batch
        // would return a clean #SPILL!, and footprints across batches
        // resolve in source order, which a partitioned run cannot replay.
        let batch_set: HashSet<u32> = cells.iter().copied().collect();
        for (cell_i, extent) in &extents {
            let anchor = &topo.cells[*cell_i as usize];
            for r in extent.start_row..=extent.end_row {
                for c in extent.start_col..=extent.end_col {
                    if (r, c) != (anchor.row, anchor.col)
                        && topo.index.contains_key(&(anchor.sheet, r, c))
                        && !batch_set.contains(&topo.index[&(anchor.sheet, r, c)])
                    {
                        return Err(
                            "a spill footprint covers a formula cell, which makes the whole-file run fail"
                                .to_string(),
                        );
                    }
                }
            }
            if all_extents.iter().any(|(sheet, e)| {
                *sheet == anchor.sheet
                    && e.start_row <= extent.end_row
                    && extent.start_row <= e.end_row
                    && e.start_col <= extent.end_col
                    && extent.start_col <= e.end_col
            }) {
                return Err(
                    "spill footprints overlap, which the whole-file run resolves in source order"
                        .to_string(),
                );
            }
            all_extents.push((anchor.sheet, extent.clone()));
        }

        // A spill anchored. The mini-workbook copies only the dependency
        // closure, so a blocker that occupies a cell no formula reads was
        // absent and the spill succeeded where the whole-file run blocks.
        // Rebuild the batch with whole-file-equivalent occupancy inside the
        // spilled extents and let the engine make the same decision the
        // whole-file run would.
        let extents: Vec<RangeAddress> = extents.into_iter().map(|(_, e)| e).collect();
        let mut wb = build_batch_workbook(store, topo, batch, &cells, &ranges, &extents)?;
        wb.evaluate_all().map_err(|e| e.to_string())?;
        read_values(&wb, topo, &cells, &mut values)?;

        for (cell_i, extent) in spilled_anchors(&wb, topo, &candidates)? {
            let anchor = &topo.cells[cell_i as usize];
            let name = &topo.sheets[anchor.sheet as usize].name;
            for r in extent.start_row..=extent.end_row {
                for c in extent.start_col..=extent.end_col {
                    if r == anchor.row && c == anchor.col {
                        continue;
                    }
                    if let Some(v) = wb.get_value(name, r, c) {
                        spilled.push((anchor.sheet, r, c, v));
                    }
                }
            }
        }
    }

    Ok(Evaluated {
        values,
        n_batches: batches.len(),
        biggest_batch_cells,
        spilled,
    })
}

/// Build one batch workbook: the sheets and copied inputs the batch reads,
/// its workbook-scoped names, its formulas, and — when `seed` is set — the
/// occupancy the whole file has.
fn build_batch_workbook(
    store: &DataStore,
    topo: &Topology,
    batch: &[u32],
    cells: &[u32],
    ranges: &[RangeRef],
    seed_extents: &[RangeAddress],
) -> Result<Workbook, String> {
    let mut wb = Workbook::new_with_config(crate::clock::pin(WorkbookConfig::ephemeral()));
    let mut added: HashSet<u16> = HashSet::new();
    for &i in cells {
        let s = topo.cells[i as usize].sheet;
        if added.insert(s) {
            wb.add_sheet(&topo.sheets[s as usize].name)
                .map_err(|e| e.to_string())?;
        }
    }
    for &(s, ..) in ranges {
        if added.insert(s) {
            wb.add_sheet(&topo.sheets[s as usize].name)
                .map_err(|e| e.to_string())?;
        }
    }
    copy_inputs(store, topo, ranges, &mut wb)?;
    // Workbook-scoped names the formulas use. Their targets were copied
    // as inputs above, and formula cells inside a target joined the same
    // component through the dependency union, so they are placed below
    // rather than read stale. Sheet-scoped names keep the whole-file
    // fallback (see `partition_verdict`).
    {
        let mut defined: HashSet<&str> = HashSet::new();
        for name in topo.static_names.iter().filter(|n| n.scope == NameScope::Workbook) {
            if !defined.insert(name.name.as_ref()) {
                continue;
            }
            let (s, ..) = name.target;
            if added.insert(s) {
                wb.add_sheet(&topo.sheets[s as usize].name)
                    .map_err(|e| e.to_string())?;
            }
            define_static_name(&mut wb, topo, name)?;
        }
    }
    if !seed_extents.is_empty() {
        let batch_cells: HashSet<u32> = cells.iter().copied().collect();
        seed_occupancy(store, topo, &mut wb, seed_extents, &batch_cells)?;
    }
    place_formulas_batch(&mut wb, topo, batch)?;
    Ok(wb)
}

/// Formulas are parsed here rather than kept in the topology, so no run
/// holds more parsed trees than one batch needs. A shared formula is
/// parsed once for the batch and shifted per member. The units follow
/// component boundaries, which is what bulk ingest needs.
fn place_formulas_batch(
    wb: &mut Workbook,
    topo: &Topology,
    batch: &[u32],
) -> Result<(), String> {
    let mut parsed: HashMap<u32, ASTNode> = HashMap::new();
    let mut units: Vec<Vec<(u16, u32, u32, ASTNode)>> = Vec::new();
    let mut unit: Vec<(u16, u32, u32, ASTNode)> = Vec::new();
    for &c in batch {
        let comp = &topo.comp_cells[c as usize];
        let text_bytes: usize = comp
            .iter()
            .map(|&i| topo.texts[topo.cells[i as usize].ast as usize].len())
            .sum();
        if text_bytes > COMPONENT_TEXT_BUDGET {
            // Placed straight into the graph, so only one tree of this
            // component is alive at a time.
            for &i in comp {
                let fc = &topo.cells[i as usize];
                if !parsed.contains_key(&fc.ast) {
                    let ast = parse(&topo.texts[fc.ast as usize]).map_err(|e| e.to_string())?;
                    parsed.insert(fc.ast, ast);
                }
                let ast = shift_ast(&parsed[&fc.ast], fc.dr, fc.dc);
                wb.engine_mut()
                    .set_cell_formula(
                        &topo.sheets[fc.sheet as usize].name,
                        fc.row,
                        fc.col,
                        ast,
                    )
                    .map_err(|e| e.to_string())?;
            }
            continue;
        }
        for &i in comp {
            let fc = &topo.cells[i as usize];
            if !parsed.contains_key(&fc.ast) {
                let ast = parse(&topo.texts[fc.ast as usize]).map_err(|e| e.to_string())?;
                parsed.insert(fc.ast, ast);
            }
            let ast = shift_ast(&parsed[&fc.ast], fc.dr, fc.dc);
            unit.push((fc.sheet, fc.row, fc.col, ast));
        }
        if unit.len() >= INGEST_CHUNK {
            units.push(std::mem::take(&mut unit));
        }
    }
    if !unit.is_empty() {
        units.push(unit);
    }
    drop(parsed);
    place_formulas(wb, topo, units)
}

/// Read the computed value of every placed formula cell.
///
/// A placed formula with no value is a lost result, not a blank: report it
/// instead of silently writing `Empty`.
fn read_values(
    wb: &Workbook,
    topo: &Topology,
    cells: &[u32],
    values: &mut [LiteralValue],
) -> Result<(), String> {
    for &i in cells {
        let fc = &topo.cells[i as usize];
        match wb.get_value(&topo.sheets[fc.sheet as usize].name, fc.row, fc.col) {
            Some(v) => values[i as usize] = v,
            None => {
                return Err(format!(
                    "no value for formula cell {} r{}c{}",
                    topo.sheets[fc.sheet as usize].name, fc.row, fc.col
                ));
            }
        }
    }
    Ok(())
}

/// The anchors among the candidate cells that spilled at the last
/// evaluation, with the exact extent the engine committed.
///
/// The engine's inspection API reports the committed rectangle, which the
/// public `get_value` cannot: it normalises `Empty` to `None`, so a spilled
/// blank would read back as missing and a footprint scan would truncate.
fn spilled_anchors(
    wb: &Workbook,
    topo: &Topology,
    candidates: &[u32],
) -> Result<Vec<(u32, RangeAddress)>, String> {
    let options = SnapshotOptions { include_values: false };
    let mut out = Vec::new();
    for &i in candidates {
        let fc = &topo.cells[i as usize];
        let address = CellAddress {
            sheet: topo.sheets[fc.sheet as usize].name.clone(),
            row: fc.row,
            column: fc.col,
        };
        let report = wb
            .engine()
            .inspect_cell(&address, &options)
            .map_err(|e| format!("spill inspection failed: {e}"))?;
        if let Some(SpillRole::Anchor { extent }) = report.cell.spill {
            out.push((i, extent));
        }
    }
    Ok(out)
}

/// Give a rebuilt batch workbook the occupancy the whole file has inside
/// the spilled extents, so the engine's spill decisions match the
/// whole-file run's.
///
/// A spill is blocked by any formula cell and by any non-Empty value; a
/// present-but-Empty cell does not block, and a cell outside every extent
/// cannot affect the decision. The store already holds every data value,
/// and a formula cell outside this batch is unreadable by it — a
/// referenced formula cell is always in the same component — so a
/// non-Empty sentinel at those addresses reproduces the block without ever
/// contaminating a computed value. Seeding is bounded by the spilled
/// area, not the sheet.
fn seed_occupancy(
    store: &DataStore,
    topo: &Topology,
    wb: &mut Workbook,
    extents: &[RangeAddress],
    batch_cells: &HashSet<u32>,
) -> Result<(), String> {
    for extent in extents {
        let name = &extent.sheet;
        let Some(s) = topo
            .sheets
            .iter()
            .position(|i| i.name == *name)
            .map(|i| i as u16)
        else {
            continue;
        };
        for r in extent.start_row..=extent.end_row {
            for c in extent.start_col..=extent.end_col {
                if let Some(&i) = topo.index.get(&(s, r, c)) {
                    if !batch_cells.contains(&i) {
                        wb.set_value(name, r, c, LiteralValue::Number(0.0))
                            .map_err(|e| e.to_string())?;
                    }
                    // A batch formula is placed as a formula below.
                    continue;
                }
                if let Some(v) = store.get(s, r, c) {
                    if !matches!(v, LiteralValue::Empty) {
                        wb.set_value(name, r, c, v).map_err(|e| e.to_string())?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Whether the cells right of and below one anchor hold a value the batch
/// never wrote. Spills lay out right and down, so any anchored spill covers
/// at least one of these two neighbours.
///
/// Only the chunk layout uses this probe: the components layout asks the
/// engine's inspection API instead. The chunk layout refuses every
/// array-capable template, so a hit here means the screen has a hole; the
/// caller fails loudly rather than return wrong rows.
fn neighbor_spilled(
    wb: &Workbook,
    name: &str,
    sheet: u16,
    row: u32,
    col: u32,
    written: &HashSet<(u16, u32, u32)>,
) -> bool {
    for (r, c) in [
        (row, col.saturating_add(1)),
        (row.saturating_add(1), col),
    ] {
        if written.contains(&(sheet, r, c)) {
            continue;
        }
        if wb.get_value(name, r, c).is_some() {
            return true;
        }
    }
    false
}

/// Whether a formula reads an open-sided range on its own sheet.
///
/// `A:B` and `3:7` name no row or no column. Bulk ingest treats such a range as
/// covering the whole sheet when it builds dependencies, so a formula whose own
/// cell lies on that sheet appears to depend on itself. `=VLOOKUP(D2,A:B,2,0)`
/// in column E then reads as `#CIRC!` although nothing in `A:B` reads it back.
/// A range naming another sheet cannot close a cycle through this formula, so
/// it is left alone.
///
/// Filling the open sides in with the grid limits also avoids the error, but it
/// costs: `=LOOKUP(2,1/(J:J<>""),J:J)` then builds a 1048576-row array. Naming
/// these few formulas instead and placing them one at a time keeps both the
/// meaning and the speed.
fn reads_open_own_range(node: &ASTNode, own_sheet: &str) -> bool {
    match &node.node_type {
        ASTNodeType::Reference {
            reference:
                ReferenceType::Range {
                    sheet,
                    start_row,
                    start_col,
                    end_row,
                    end_col,
                    ..
                },
            ..
        } => {
            (start_row.is_none()
                || start_col.is_none()
                || end_row.is_none()
                || end_col.is_none())
                && !is_other_sheet(sheet.as_deref(), own_sheet)
        }
        ASTNodeType::UnaryOp { expr, .. } => reads_open_own_range(expr, own_sheet),
        ASTNodeType::BinaryOp { left, right, .. } => {
            reads_open_own_range(left, own_sheet) || reads_open_own_range(right, own_sheet)
        }
        ASTNodeType::Function { args, .. } => {
            args.iter().any(|a| reads_open_own_range(a, own_sheet))
        }
        ASTNodeType::Call { callee, args } => {
            reads_open_own_range(callee, own_sheet)
                || args.iter().any(|a| reads_open_own_range(a, own_sheet))
        }
        ASTNodeType::Array(rows) => rows
            .iter()
            .any(|row| row.iter().any(|a| reads_open_own_range(a, own_sheet))),
        _ => false,
    }
}

/// Formulas per bulk ingest call.
///
/// The builder holds every staged tree until it finishes, and clones each one
/// while it plans, so one call over a whole batch raises peak memory above the
/// whole-file path it exists to beat. Ingesting in several calls returns that
/// memory, because the staged trees are dropped as each call completes. It
/// costs no measurable time, because bulk ingest removes a per-formula cost,
/// not a per-call one.
///
/// A call may not split a component. Formulas in one component read each other,
/// and a reference to a formula ingested by an earlier call reads as blank, so
/// a split zeroes whole columns without reporting an error. Components are
/// closed under formula-to-formula dependency, so a whole number of them is
/// always safe.
const INGEST_CHUNK: usize = 2_048;

/// Formula text per component above which formulas are placed one at a time.
///
/// Bulk ingest must receive a whole component, and while it plans it holds
/// every staged tree twice: `finish` clones each owned tree into its batch
/// input while the staged copy is still alive. A parsed tree runs about a
/// hundred times the size of its source text, so the clone of a large component
/// costs tens of megabytes on its own.
///
/// The budget is set where the clone stops being affordable. Below it the
/// doubled peak stays under what a whole-file run of the same workbook costs,
/// and bulk ingest is much faster than placing formulas one at a time. Above it
/// the doubled peak overtakes the whole-file run, which is the one thing this
/// crate must not do.
const COMPONENT_TEXT_BUDGET: usize = 1_000_000;


/// Place formulas through the engine's bulk ingest path.
///
/// Setting one formula at a time is far more expensive than evaluating it.
/// Each call previews graph admission and mutates the dependency graph on its
/// own, and the cost per call grows with the graph already present. Bulk
/// ingest stages a whole batch and builds its edges once.
///
/// The engine configuration is left alone. The xlsx loader also switches the
/// sheet index to lazy while it ingests, but it feeds values through the same
/// pass. Here the data cells are already written, and a lazy index reads them
/// as blank, which turns range results into zero without reporting an error.
/// The loader's other two ingest settings change no measured time, so they are
/// not copied either. Every sheet named here must already exist in the
/// workbook.
///
/// Each unit is ingested on its own and must hold whole components.
fn place_formulas(
    wb: &mut Workbook,
    topo: &Topology,
    units: Vec<Vec<(u16, u32, u32, ASTNode)>>,
) -> Result<(), String> {
    for unit in units {
        // Sheet order must not vary between runs. Bulk ingest is fed one
        // sheet at a time, and a dependency chain split across two calls reads
        // stale values from the split point on without reporting an error, so
        // a random iteration order would move that split from run to run.
        //
        // Do not change this to `HashMap` for speed. A unit holds a handful of
        // sheets, so ordering them costs nothing measurable, and a randomly
        // ordered map produced results that differed between runs of the same
        // binary on the same file: mismatches against a whole-file run moved
        // between 102 and 145 across ten runs, and were a fixed 39 once this
        // map was ordered.
        let mut by_sheet: BTreeMap<u16, Vec<(u32, u32, ASTNode)>> = BTreeMap::new();
        for (sheet, row, col, ast) in unit {
            let name = &topo.sheets[sheet as usize].name;
            if reads_open_own_range(&ast, name) {
                wb.engine_mut()
                    .set_cell_formula(name, row, col, ast)
                    .map_err(|e| e.to_string())?;
            } else {
                by_sheet.entry(sheet).or_default().push((row, col, ast));
            }
        }

        let mut builder = wb.engine_mut().begin_bulk_ingest();
        for (sheet, mut list) in by_sheet {
            // Row-major order is what the loader feeds the builder.
            list.sort_unstable_by_key(|&(r, c, _)| (r, c));
            let id = builder.add_sheet(&topo.sheets[sheet as usize].name);
            builder.add_formulas(id, list);
        }
        builder.finish().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Copy the data cells a batch reads into its workbook.
///
/// Cells holding formulas are skipped: a referenced formula cell is always in
/// the same component, so it is written as a formula instead. The store already
/// excludes them, so a miss simply means the cell is blank.
fn copy_inputs(
    store: &DataStore,
    topo: &Topology,
    ranges: &[RangeRef],
    wb: &mut Workbook,
) -> Result<HashSet<(u16, u32, u32)>, String> {
    // Skips a cell an earlier range already copied.
    //
    // This must stay while ranges can overlap. The caller sorts and dedups the
    // range list, but that collapses only *identical* rectangles: two
    // overlapping but distinct rectangles, such as `A6:A64` and `A6:A53` from
    // repeated lookups down one column, still walk the same cells. Dropping
    // this set requires genuinely disjoint rectangles, which means containment
    // pruning and then a rectangle union, not a cheaper set type.
    //
    // That work was considered and not done. A partitioned run's time is in
    // formula placement and evaluation inside the engine, not in this copy
    // (measured: 7540 ms of run against 390 ms of topology build on a
    // 265,587-formula workbook). Before building rectangle merging, measure
    // `copy_inputs` itself on a batch with a large range union and show that
    // it is worth the complexity.
    let mut seen: HashSet<(u16, u32, u32)> = HashSet::new();
    let mut failed = None;
    for &(s, r0, c0, r1, c1) in ranges {
        let name = &topo.sheets[s as usize].name;
        store.for_range(s, r0, c0, r1, c1, |r, c, v| {
            if failed.is_some() || !seen.insert((s, r, c)) {
                return;
            }
            if let Err(e) = wb.set_value(name, r, c, v) {
                failed = Some(e.to_string());
            }
        });
    }
    match failed {
        Some(e) => Err(e),
        None => Ok(seen),
    }
}

// ---------------------------------------------------------------------------
// Scratch path
// ---------------------------------------------------------------------------

/// Source rows evaluated per scratch chunk.
///
/// The cost of placing a formula grows faster than the sheet height, so the
/// scratch sheet must stay short. The cost is in taking the formulas into the
/// dependency graph, not in calculating them.
pub const DEFAULT_CHUNK_ROWS: u32 = 500;

/// Rows above its own row that a formula may read.
///
/// Carry rows add values but no formulas to the scratch sheet. This limit keeps
/// the total sheet height at no more than twice the default chunk height.
pub const MAX_CARRY_ROWS: u32 = DEFAULT_CHUNK_ROWS;

/// Estimated lookup rows the scratch path accepts before it gives the file back.
///
/// The estimate charges each call the full height of its table. The engine can
/// index repeated exact lookups, so this is a conservative limit. Above the
/// limit, the caller tries the component path.
pub const DEFAULT_LOOKUP_BUDGET: u64 = 250_000_000;

/// One formula column of the scratch sheet.
#[derive(Clone)]
struct ColumnPlan {
    col: u32,
    /// The template this column repeats.
    ast: u32,
    /// The row the template was parsed at.
    anchor_row: u32,
    /// The column offset every cell of this column applies.
    dc: i64,
}

/// How to evaluate a workbook on one reused scratch sheet.
///
/// The plan is only produced for a workbook whose formulas form a solid block:
/// one sheet, one template per column, the same rows in every formula column,
/// and no reference to a later row. References to earlier rows use a bounded
/// carry area above each chunk. Under those rules the scratch sheet holds the
/// same formulas for every chunk, so the formulas are placed once and only the
/// input values change.
#[derive(Clone)]
pub struct ScratchPlan {
    /// The one sheet that holds formulas.
    pub sheet: u16,
    /// The first and last row of the formula block, inclusive.
    pub first_row: u32,
    pub last_row: u32,
    pub chunk_rows: u32,
    columns: Vec<ColumnPlan>,
    /// Rows of results the scratch sheet carries above each chunk, which is
    /// the deepest row a formula reads above its own.
    pub carry: u32,
    /// Data columns of the formula sheet that the formulas read.
    data_cols: Vec<u32>,
    /// Every column written above the chunk: the data columns plus the formula
    /// columns, whose carried values are the results of the chunk before.
    carry_cols: Vec<u32>,
    /// Ranges on other sheets. These hold data only and are copied once.
    static_ranges: Vec<RangeRef>,
    /// Workbook-scoped names for fixed ranges on the static sheets.
    static_names: Vec<StaticName>,
}

impl ScratchPlan {
    pub fn n_chunks(&self) -> usize {
        let rows = (self.last_row - self.first_row + 1) as u64;
        ((rows + self.chunk_rows as u64 - 1) / self.chunk_rows as u64) as usize
    }

    pub fn n_data_cols(&self) -> usize {
        self.data_cols.len()
    }

    /// Whether any template this plan places can produce a spilled array.
    ///
    /// A chunk workbook holds only the columns the plan copies, so a spill
    /// could occupy cells the chunk never wrote. The planner refuses these
    /// templates and routes the file to the components layout, which
    /// resolves spills against whole-file occupancy.
    pub fn array_capable(&self, topo: &Topology) -> bool {
        self.columns
            .iter()
            .any(|c| topo.array_capable.get(c.ast as usize).copied().unwrap_or(false))
    }
}

/// Decide whether a workbook can be evaluated on one reused scratch sheet.
///
/// Returns the reason it cannot, so a caller can report it. Every rule here
/// protects the same property: the formula placed at a given scratch row must
/// be the same for every chunk, and moving a formula to another row must not
/// change what it computes.
pub fn plan_scratch(
    topo: &Topology,
    chunk_rows: u32,
    lookup_budget: u64,
) -> Result<ScratchPlan, &'static str> {
    if topo.cells.is_empty() {
        return Err("no formulas");
    }
    // The lookup estimate does not model the engine's exact-match cache. Keep
    // the conservative budget until the planner classifies cacheable calls.
    if topo.lookup_work() > lookup_budget {
        return Err("lookups cost more than the budget allows");
    }
    // A formula that reads its own position gives a different answer after the
    // move. A nondeterministic call gives a different answer in every engine.
    if topo.row_sensitive_fns > 0 {
        return Err("formulas that read their own position");
    }
    if topo.nondeterministic_fns > 0 {
        return Err("unreproducible formulas");
    }

    let sheet = topo.cells[0].sheet;
    if topo.cells.iter().any(|c| c.sheet != sheet) {
        return Err("more than one sheet holds formulas");
    }
    if topo.unsupported_refs > 0 {
        return Err("unsupported references");
    }
    for name in &topo.static_names {
        if name.scope != NameScope::Workbook {
            return Err("sheet-scoped defined names");
        }
        if name.target.0 == sheet {
            return Err("a defined name targets the formula sheet");
        }
    }
    let sheet_name = topo.sheets[sheet as usize].name.as_str();

    // A reference on the formula sheet may read its own row or a row above it,
    // and nothing else. A row below sits in a chunk that is not evaluated yet,
    // and an absolute row would not move with the formula. The deepest row read
    // above becomes the number of carry rows the scratch sheet holds.
    let mut carry = 0u32;
    for (k, refs) in topo.ast_refs.iter().enumerate() {
        let anchor_row = topo.anchors[k].0;
        for r in refs {
            match ref_carry(r, anchor_row, sheet_name) {
                Some(depth) => carry = carry.max(depth),
                None => return Err("a formula reads outside its own row"),
            }
        }
    }
    if carry > MAX_CARRY_ROWS {
        return Err("a formula reads too far above its own row");
    }

    // Group the formula cells by column, and check that each column repeats one
    // template at a fixed offset.
    // Ordered, so that when more than one column is faulty the refusal reason
    // reported for the file does not depend on map iteration order.
    let mut by_col: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (i, fc) in topo.cells.iter().enumerate() {
        by_col.entry(fc.col).or_default().push(i as u32);
    }

    let mut columns: Vec<ColumnPlan> = Vec::with_capacity(by_col.len());
    let mut band: Option<(u32, u32)> = None;
    let mut rows_of_first: Vec<u32> = Vec::new();
    for (col, members) in &by_col {
        let first = &topo.cells[members[0] as usize];
        let ast = first.ast;
        let dc = first.dc;
        let anchor_row = (first.row as i64 - first.dr) as u32;

        let mut rows: Vec<u32> = Vec::with_capacity(members.len());
        for &i in members {
            let fc = &topo.cells[i as usize];
            if fc.ast != ast || fc.dc != dc {
                return Err("a formula column holds more than one template");
            }
            if (fc.row as i64 - fc.dr) as u32 != anchor_row {
                return Err("a formula column holds more than one template");
            }
            rows.push(fc.row);
        }
        rows.sort_unstable();

        // The block must be solid. A gap would leave a scratch row holding a
        // formula that the source does not have, and a formula in the same row
        // could then read that invented result.
        let (lo, hi) = (rows[0], rows[rows.len() - 1]);
        if (hi - lo + 1) as usize != rows.len() {
            return Err("a formula column has gaps");
        }
        match band {
            None => {
                band = Some((lo, hi));
                rows_of_first = rows;
            }
            Some(b) => {
                if b != (lo, hi) {
                    return Err("formula columns cover different rows");
                }
            }
        }

        columns.push(ColumnPlan { col: *col, ast, anchor_row, dc });
    }
    let (first_row, last_row) = band.ok_or("no formulas")?;
    debug_assert_eq!(rows_of_first.len(), (last_row - first_row + 1) as usize);
    // A stable order keeps the scratch sheet layout the same between runs.
    columns.sort_by_key(|c| c.col);

    // Split the ranges the formulas read: those on the formula sheet give the
    // data columns to load per chunk, and those on other sheets are static
    // inputs that are copied once.
    let mut data_cols: Vec<u32> = Vec::new();
    let mut static_ranges: Vec<RangeRef> = Vec::new();
    for refs in &topo.comp_refs {
        for &(s, r0, c0, r1, c1) in refs {
            if s == sheet {
                for c in c0..=c1 {
                    if !data_cols.contains(&c) {
                        data_cols.push(c);
                    }
                }
            } else {
                static_ranges.push((s, r0, c0, r1, c1));
            }
        }
    }
    // A formula column is written as a formula, not as a value.
    data_cols.retain(|c| !columns.iter().any(|p| p.col == *c));
    data_cols.sort_unstable();
    static_ranges.sort_unstable();
    static_ranges.dedup();

    // Above the chunk every column holds a value, the formula columns included,
    // because their carried value is the result the chunk before computed.
    let mut carry_cols = data_cols.clone();
    if carry > 0 {
        carry_cols.extend(columns.iter().map(|p| p.col));
        carry_cols.sort_unstable();
        carry_cols.dedup();
    } else {
        carry_cols.clear();
    }

    Ok(ScratchPlan {
        sheet,
        first_row,
        last_row,
        chunk_rows: chunk_rows.max(1),
        carry,
        columns,
        data_cols,
        carry_cols,
        static_ranges,
        static_names: topo.static_names.clone(),
    })
}

/// Does this reference still read the same cells after the formula moves to a
/// scratch row?
///
/// Returns how many rows above its own row the reference reads, or `None` when
/// the move would change what the formula computes.
///
/// On the formula's own sheet the reference must be relative, so that it follows
/// the formula, and it must name the anchor row or a row above it. A row above
/// is read from the carry rows, which hold the results of the chunk before. A
/// row below is refused: the chunk that holds it is not evaluated yet.
///
/// On another sheet the opposite is required. Those sheets hold data only and
/// are copied once at their own coordinates, so a reference into them must not
/// move at all. Every coordinate it names must be absolute. An open side, as in
/// `Lookup!A:B`, is never shifted and is therefore safe as it stands.
///
/// A relative reference to another sheet, such as `Sheet2!A2` repeated down a
/// column, is exactly what this rejects. It moves with the formula while the
/// sheet it reads does not, so it would read the wrong row.
fn ref_carry(r: &RawRef, anchor_row: u32, sheet_name: &str) -> Option<u32> {
    match r {
        RawRef::Cell { sheet, row, col: _, row_abs, col_abs } => {
            if is_other_sheet(sheet.as_deref(), sheet_name) {
                return (*row_abs && *col_abs).then_some(0);
            }
            if *row_abs || *row > anchor_row {
                return None;
            }
            Some(anchor_row - *row)
        }
        RawRef::Range {
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
            if is_other_sheet(sheet.as_deref(), sheet_name) {
                let still = pinned(*start_row, *start_row_abs)
                    && pinned(*end_row, *end_row_abs)
                    && pinned(*start_col, *start_col_abs)
                    && pinned(*end_col, *end_col_abs);
                return still.then_some(0);
            }
            if *start_row_abs || *end_row_abs {
                return None;
            }
            // An open side on the formula's own sheet spans rows no chunk holds.
            let (top, bottom) = ((*start_row)?, (*end_row)?);
            if top > anchor_row || bottom > anchor_row {
                return None;
            }
            Some(anchor_row - top.min(bottom))
        }
        RawRef::Named { .. } => Some(0),
        RawRef::Unsupported => None,
    }
}

/// Build the workbook that every chunk is evaluated on.
///
/// The sheets are added, the static inputs are copied once, and the formulas
/// are placed after the carry area. The formula for a column at scratch row
/// `carry + t` depends only on that row, because the column repeats one
/// template, so the same layout serves every chunk.
fn scratch_workbook(
    store: &DataStore,
    topo: &Topology,
    plan: &ScratchPlan,
) -> Result<Workbook, String> {
    let name = topo.sheets[plan.sheet as usize].name.clone();
    let mut wb = Workbook::new_with_config(crate::clock::pin(WorkbookConfig::ephemeral()));
    wb.add_sheet(&name).map_err(|e| e.to_string())?;

    // Static inputs live on other sheets and never change, so they are copied
    // once instead of once per chunk.
    let mut added: HashSet<u16> = HashSet::new();
    added.insert(plan.sheet);
    for &(s, ..) in &plan.static_ranges {
        if added.insert(s) {
            wb.add_sheet(&topo.sheets[s as usize].name)
                .map_err(|e| e.to_string())?;
        }
    }
    // Sorted and deduped rather than collected into a set, so the copy order
    // is the same on every run.
    let mut statics: Vec<RangeRef> = plan.static_ranges.clone();
    statics.sort_unstable();
    statics.dedup();
    copy_inputs(store, topo, &statics, &mut wb)?;
    for defined in &plan.static_names {
        define_static_name(&mut wb, topo, defined)?;
    }

    let mut parsed: HashMap<u32, ASTNode> = HashMap::new();
    let mut staged: Vec<(u16, u32, u32, ASTNode)> = Vec::new();
    for plan_col in &plan.columns {
        if !parsed.contains_key(&plan_col.ast) {
            let ast = parse(&topo.texts[plan_col.ast as usize]).map_err(|e| e.to_string())?;
            parsed.insert(plan_col.ast, ast);
        }
        let template = &parsed[&plan_col.ast];
        for t in 1..=plan.chunk_rows {
            let scratch_row = plan.carry + t;
            let dr = scratch_row as i64 - plan_col.anchor_row as i64;
            let ast = shift_ast(template, dr, plan_col.dc);
            staged.push((plan.sheet, scratch_row, plan_col.col, ast));
        }
    }
    drop(parsed);
    // One unit: a scratch row may read the row above it, so these formulas must
    // reach the graph together. The scratch sheet is short by construction.
    place_formulas(&mut wb, topo, vec![staged])?;
    Ok(wb)
}

/// A side that names no coordinate is open and is never shifted. A side that
/// names one must be absolute to stay where it is.
fn pinned(side: Option<u32>, is_abs: bool) -> bool {
    side.is_none() || is_abs
}

fn is_other_sheet(name: Option<&str>, own: &str) -> bool {
    match name {
        None => false,
        Some(n) => n != own,
    }
}

/// Evaluate every formula on one reused scratch workbook.
///
/// The workbook is built once: the sheets are added, the static inputs are
/// copied, and the formulas are placed after the carry area. Each chunk writes
/// the preceding results into the carry rows, writes its data values, and then
/// evaluates. The workbook is reused across chunks because building the
/// dependency graph is what costs.
pub fn run_scratch(
    store: &DataStore,
    topo: &Topology,
    plan: &ScratchPlan,
) -> Result<Evaluated, String> {
    let name = topo.sheets[plan.sheet as usize].name.clone();
    let mut wb = scratch_workbook(store, topo, plan)?;

    // Every cell a chunk writes, in scratch coordinates: carry rows, data
    // rows and formula anchors. Anything else holding a value after an
    // evaluation is a spilled array the whole-file run may block on.
    let mut written: HashSet<(u16, u32, u32)> = HashSet::new();
    for q in 1..=plan.carry {
        for &c in &plan.carry_cols {
            written.insert((plan.sheet, q, c));
        }
    }
    for t in 1..=plan.chunk_rows {
        let scratch_row = plan.carry + t;
        for &c in &plan.data_cols {
            written.insert((plan.sheet, scratch_row, c));
        }
        for plan_col in &plan.columns {
            written.insert((plan.sheet, scratch_row, plan_col.col));
        }
    }

    let mut values = vec![LiteralValue::Empty; topo.cells.len()];
    let mut start = plan.first_row;
    while start <= plan.last_row {
        let end = start
            .saturating_add(plan.chunk_rows - 1)
            .min(plan.last_row);

        // Carry rows contain plain values. Formula-column values come from the
        // chunk before; data-column values come from the source store.
        for q in 0..plan.carry {
            let source_row = start - plan.carry + q;
            for &c in &plan.carry_cols {
                let v = match topo.index.get(&(plan.sheet, source_row, c)) {
                    Some(&i) => values[i as usize].clone(),
                    None => store
                        .get(plan.sheet, source_row, c)
                        .unwrap_or(LiteralValue::Empty),
                };
                wb.set_value(&name, q + 1, c, v)
                    .map_err(|e| e.to_string())?;
            }
        }

        // Every needed cell is written on every chunk, including the blanks, so
        // no value can survive from the chunk before.
        for t in 1..=plan.chunk_rows {
            let source_row = start + t - 1;
            let scratch_row = plan.carry + t;
            for &c in &plan.data_cols {
                let v = if source_row <= end {
                    store
                        .get(plan.sheet, source_row, c)
                        .unwrap_or(LiteralValue::Empty)
                } else {
                    // The last chunk can be shorter than the scratch sheet.
                    LiteralValue::Empty
                };
                wb.set_value(&name, scratch_row, c, v)
                    .map_err(|e| e.to_string())?;
            }
        }

        wb.evaluate_all().map_err(|e| e.to_string())?;

        // See `anchored_spill`: a spill the whole-file run would block on
        // must fall back instead of returning the first element.
        let mut chunk_spill = false;
        for t in 1..=plan.chunk_rows {
            let scratch_row = plan.carry + t;
            for plan_col in &plan.columns {
                if neighbor_spilled(
                    &wb,
                    &name,
                    plan.sheet,
                    scratch_row,
                    plan_col.col,
                    &written,
                ) {
                    chunk_spill = true;
                    break;
                }
            }
            if chunk_spill {
                break;
            }
        }
        if chunk_spill {
            // The planner refuses every array-capable template on this
            // layout, so reaching this line means the screen has a hole.
            // Fail loudly rather than return rows the whole-file run
            // would disagree with.
            return Err(
                "a formula spilled into cells the chunk does not hold".to_string(),
            );
        }

        for t in 1..=(end - start + 1) {
            let source_row = start + t - 1;
            let scratch_row = plan.carry + t;
            for plan_col in &plan.columns {
                let Some(&i) = topo.index.get(&(plan.sheet, source_row, plan_col.col)) else {
                    continue;
                };
                if let Some(v) = wb.get_value(&name, scratch_row, plan_col.col) {
                    values[i as usize] = v;
                }
            }
        }

        start = end + 1;
    }

    Ok(Evaluated {
        values,
        n_batches: plan.n_chunks(),
        biggest_batch_cells: plan.chunk_rows as u64
            * (plan.columns.len() + plan.data_cols.len()) as u64
            + plan.carry as u64 * plan.carry_cols.len() as u64,
        spilled: Vec::new(),
    })
}

/// The cells of one source row, in column order.
pub type RowCells = Vec<(u32, LiteralValue)>;

/// Evaluates a scratch plan while the source rows stream past it.
///
/// `run_scratch` holds every data value of the workbook in a store, and returns
/// every result at once. This type holds one chunk instead. It reads the rows of
/// the formula sheet in order, evaluates the chunk that holds the row the caller
/// asks for, and gives that row back before it reads the next chunk. Peak memory
/// is one chunk plus the other sheets, which carry the lookup tables and hold no
/// formulas on this path.
pub struct ScratchRun {
    wb: Workbook,
    name: String,
    plan: ScratchPlan,
    stream: SheetStream,
    /// The data of every sheet except the streamed one.
    store: DataStore,
    /// The first source row of the window in `rows`.
    window_start: u32,
    /// The last source row of the window in `rows`.
    window_end: u32,
    /// One entry per row of the window. A row that holds no value stays empty.
    rows: Vec<RowCells>,
    /// The first row after the window. The stream reads it before the window
    /// ends, so it is kept here until the next window needs it.
    ahead: Option<(u32, RowCells)>,
    /// The last source rows already read and evaluated. They become plain
    /// input values above the next chunk.
    tail: Vec<(u32, RowCells)>,
    /// Whether a window was loaded.
    started: bool,
}

impl ScratchRun {
    /// Build the scratch workbook and open the row stream.
    ///
    /// Returns the reason it cannot stream the file, so that the caller can use
    /// the store path instead of failing.
    pub fn open(data: &[u8], topo: &mut Topology, plan: ScratchPlan) -> Result<ScratchRun, String> {
        let sheet = plan.sheet;
        // The stream returns rows in the order the part holds them, and this
        // run reads them in ascending order.
        if !topo.sheets[sheet as usize].rows_ascending {
            return Err("the sheet holds its rows out of order".to_string());
        }

        let mut zip =
            zip::ZipArchive::new(std::io::Cursor::new(data)).map_err(|e| e.to_string())?;
        let values = Rc::new(Values::open(&mut zip));
        let parts = crate::graph::sheet_parts(&mut zip);
        let part = match parts.get(sheet as usize) {
            Some((_name, path)) => path.clone(),
            None => return Err("the formula sheet has no part".to_string()),
        };
        let stream = match SheetStream::open(&mut zip, &part, values) {
            Some(s) => s,
            None => return Err("the sheet part cannot be read as a stream".to_string()),
        };
        drop(zip);

        let store = DataStore::load_without(topo, Some(sheet));
        let name = topo.sheets[sheet as usize].name.clone();
        let wb = scratch_workbook(&store, topo, &plan)?;

        Ok(ScratchRun {
            wb,
            name,
            plan,
            stream,
            store,
            window_start: 0,
            window_end: 0,
            rows: Vec::new(),
            ahead: None,
            tail: Vec::new(),
            started: false,
        })
    }

    /// The sheet whose rows this run streams.
    pub fn sheet(&self) -> u16 {
        self.plan.sheet
    }

    pub fn n_chunks(&self) -> usize {
        self.plan.n_chunks()
    }

    /// A value from a sheet this run does not stream.
    pub fn other_value(&self, sheet: u16, row: u32, col: u32) -> Option<LiteralValue> {
        self.store.get(sheet, row, col)
    }

    /// The cells of one source row of the formula sheet, in column order.
    ///
    /// The caller must ask for rows in ascending order. The chunk that holds the
    /// row is evaluated when the row first enters the window.
    pub fn row(&mut self, row: u32) -> Result<&[(u32, LiteralValue)], String> {
        if self.started && row < self.window_start {
            return Err("rows must be read in ascending order".to_string());
        }
        if !self.started || row > self.window_end {
            self.load_window(row)?;
        }
        Ok(&self.rows[(row - self.window_start) as usize])
    }

    /// Read the window that holds `row`, and evaluate it when it holds formulas.
    fn load_window(&mut self, row: u32) -> Result<(), String> {
        let in_block = row >= self.plan.first_row && row <= self.plan.last_row;
        let (start, end) = if in_block {
            let index = (row - self.plan.first_row) / self.plan.chunk_rows;
            let start = self.plan.first_row + index * self.plan.chunk_rows;
            let end = start
                .saturating_add(self.plan.chunk_rows - 1)
                .min(self.plan.last_row);
            (start, end)
        } else {
            // A row outside the formula block carries data only, so the window
            // is that one row and the scratch sheet stays as it is.
            (row, row)
        };

        self.window_start = start;
        self.window_end = end;
        self.started = true;
        self.rows = vec![Vec::new(); (end - start + 1) as usize];
        self.fill_rows(start, end);
        if in_block {
            self.evaluate_window(start, end)?;
        }
        self.remember_tail(start, end);
        Ok(())
    }

    /// Take the rows of the window off the stream.
    ///
    /// A row that holds no value at all is absent from the part, and its entry
    /// stays empty. The cached result of a formula cell is dropped, because the
    /// scratch sheet computes that value again.
    fn fill_rows(&mut self, start: u32, end: u32) {
        loop {
            let next = match self.ahead.take() {
                Some(row) => Some(row),
                None => self.stream.next_row(),
            };
            let Some((r, mut cells)) = next else { return };
            if r < start {
                continue;
            }
            if r > end {
                self.ahead = Some((r, cells));
                return;
            }
            if r >= self.plan.first_row && r <= self.plan.last_row {
                cells.retain(|(c, _)| !self.plan.columns.iter().any(|p| p.col == *c));
            }
            cells.sort_by_key(|(c, _)| *c);
            self.rows[(r - start) as usize] = cells;
        }
    }

    /// Write the data values of the window, evaluate it, and put the results
    /// back into the window rows.
    fn evaluate_window(&mut self, start: u32, end: u32) -> Result<(), String> {
        // A carried formula result is a plain value here. The engine then joins
        // it to the dependency chain of the formulas below it.
        for q in 0..self.plan.carry {
            let source_row = start - self.plan.carry + q;
            let cells = tail_row(&self.tail, source_row).ok_or_else(|| {
                format!("row {source_row} is not available for the next chunk")
            })?;
            for &c in &self.plan.carry_cols {
                let v = cell_of(cells, c);
                self.wb
                    .set_value(&self.name, q + 1, c, v)
                    .map_err(|e| e.to_string())?;
            }
        }

        // Every cell the formulas read is written on every chunk, blanks
        // included, so no value can survive from the chunk before.
        for t in 1..=self.plan.chunk_rows {
            let source_row = start + t - 1;
            let scratch_row = self.plan.carry + t;
            for &c in &self.plan.data_cols {
                let v = if source_row <= end {
                    cell_of(&self.rows[(source_row - start) as usize], c)
                } else {
                    // The last chunk can be shorter than the scratch sheet.
                    LiteralValue::Empty
                };
                self.wb
                    .set_value(&self.name, scratch_row, c, v)
                    .map_err(|e| e.to_string())?;
            }
        }

        self.wb.evaluate_all().map_err(|e| e.to_string())?;

        // A spill the whole-file run would block on cannot fall back here:
        // earlier windows already yielded their rows. It fails loudly
        // instead of returning the first element. See `anchored_spill`.
        for t in 1..=self.plan.chunk_rows {
            let scratch_row = self.plan.carry + t;
            for plan_col in &self.plan.columns {
                for (r, c) in [
                    (scratch_row, plan_col.col.saturating_add(1)),
                    (scratch_row.saturating_add(1), plan_col.col),
                ] {
                    // A neighbour in a carry, data or formula column was
                    // written by this chunk; anything else holding a value
                    // is a spilled array the whole-file run may block on.
                    let written = (r <= self.plan.carry && self.plan.carry_cols.contains(&c))
                        || (r > self.plan.carry && self.plan.data_cols.contains(&c))
                        || self.plan.columns.iter().any(|p| p.col == c);
                    if written {
                        continue;
                    }
                    if self.wb.get_value(&self.name, r, c).is_some() {
                        return Err(
                            "a formula spilled into cells the chunk does not hold".to_string(),
                        );
                    }
                }
            }
        }

        for t in 1..=(end - start + 1) {
            let scratch_row = self.plan.carry + t;
            for plan_col in &self.plan.columns {
                let v = self
                    .wb
                    .get_value(&self.name, scratch_row, plan_col.col)
                    .unwrap_or(LiteralValue::Empty);
                self.rows[(t - 1) as usize].push((plan_col.col, v));
            }
            self.rows[(t - 1) as usize].sort_by_key(|(c, _)| *c);
        }
        Ok(())
    }

    /// Keep only the rows that the next chunk can read.
    fn remember_tail(&mut self, start: u32, end: u32) {
        if self.plan.carry == 0 {
            return;
        }
        self.tail.extend(
            (start..=end).map(|r| (r, self.rows[(r - start) as usize].clone())),
        );
        let keep = self.plan.carry as usize;
        if self.tail.len() > keep {
            self.tail.drain(..self.tail.len() - keep);
        }
    }
}

/// Find one row in the small carry tail.
fn tail_row(tail: &[(u32, RowCells)], row: u32) -> Option<&[(u32, LiteralValue)]> {
    tail.iter().find(|(r, _)| *r == row).map(|(_, cells)| cells.as_slice())
}

/// The value at one column of a row, or blank when the row does not hold it.
fn cell_of(row: &[(u32, LiteralValue)], col: u32) -> LiteralValue {
    match row.binary_search_by_key(&col, |(c, _)| *c) {
        Ok(i) => row[i].1.clone(),
        Err(_) => LiteralValue::Empty,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use formualizer::parse::parser::parse;
    use formualizer::parse::pretty::canonical_formula;
    use formualizer::workbook::backends::CalamineAdapter;
    use formualizer::workbook::traits::SpreadsheetReader;
    use formualizer::workbook::LoadStrategy;
    use crate::testkit::{cell_f, cell_s, cell_v, xlsx, xlsx_with_defined_names, xlsx_with_shared_strings};

    fn shifted(formula: &str, dr: i64, dc: i64) -> String {
        canonical_formula(&shift_ast(&parse(formula).unwrap(), dr, dc))
    }

    #[test]
    fn shift_moves_relative_references_only() {
        assert_eq!(shifted("=A1+1", 5, 0), "=A6 + 1");
        assert_eq!(shifted("=A1", 0, 2), "=C1");
        assert_eq!(shifted("=$A$1", 5, 2), "=$A$1", "absolute stays pinned");
        assert_eq!(shifted("=$A1", 5, 2), "=$A6", "column pinned, row moves");
        assert_eq!(shifted("=A$1", 5, 2), "=C$1", "row pinned, column moves");
    }

    #[test]
    fn shift_reaches_nested_nodes() {
        assert_eq!(shifted("=IF(F2=\"\",\"\",H2-F2)", 3, 0), "=IF(F5 = \"\", \"\", H5 - F5)");
        assert_eq!(shifted("=SUM(A1:B2)", 2, 0), "=SUM(A3:B4)");
        assert_eq!(shifted("=SUM(A$1:B2)", 2, 0), "=SUM(A$1:B4)");
    }

    #[test]
    fn shift_of_zero_is_identity() {
        for f in ["=A1+B2", "=SUM(A1:A9)", "=IF(A1>0,B1,C1)"] {
            assert_eq!(shifted(f, 0, 0), canonical_formula(&parse(f).unwrap()));
        }
    }

    #[test]
    fn shift_keeps_sheet_qualifier() {
        assert_eq!(shifted("=Other!A1", 4, 0), "=Other!A5");
    }

    /// Batching decides peak memory, so every component must appear exactly
    /// once and no batch may exceed the budget unless a single component does.
    #[test]
    fn batches_cover_every_component_within_budget() {
        let extents = vec![10u64, 20, 5, 100, 1, 1];
        let topo = fake_topo(&extents);
        let batches = plan_batches(&topo, 30);

        let mut seen: Vec<u32> = batches.iter().flatten().copied().collect();
        seen.sort();
        assert_eq!(seen, vec![0, 1, 2, 3, 4, 5], "each component batched once");

        for b in &batches {
            let total: u64 = b.iter().map(|&c| extents[c as usize]).sum();
            assert!(
                total <= 30 || b.len() == 1,
                "batch {b:?} totals {total}, over budget with more than one component"
            );
        }
    }

    #[test]
    fn oversized_component_gets_its_own_batch() {
        let topo = fake_topo(&[5, 500, 5]);
        let batches = plan_batches(&topo, 100);
        let big = batches.iter().find(|b| b.contains(&1)).unwrap();
        assert_eq!(big, &vec![1], "a component larger than the budget cannot share");
    }

    #[test]
    fn shared_lookup_range_is_charged_once() {
        // Two components reading the same 10k-cell table cost one table.
        // Per-component budgeting would charge 10001 + 10001 and split them.
        let mut topo = fake_topo(&[1, 1]);
        let table: RangeRef = (0, 1, 1, 100, 100);
        topo.comp_refs = vec![vec![table], vec![table]];
        let batches = plan_batches(&topo, 10_500);
        assert_eq!(
            batches.len(),
            1,
            "1 + 10000 shared + 1 fits the budget in one batch"
        );
    }

    fn fake_topo(extents: &[u64]) -> Topology {
        Topology {
            sheets: Vec::new(),
            name_only_sheets: Vec::new(),
            cells: Vec::new(),
            texts: Vec::new(),
            anchors: Vec::new(),
            values: Vec::new(),
            ast_refs: Vec::new(),
            static_names: Vec::new(),
            named_refs: 0,
            lookup_rows: Vec::new(),
            comp_of: Vec::new(),
            comp_cells: extents.iter().map(|&e| vec![0u32; e as usize]).collect(),
            comp_refs: vec![Vec::new(); extents.len()],
            blanks: Vec::new(),
            text_criteria_ifs: 0,
            array_capable: Vec::new(),
            comp_extent: extents.to_vec(),
            index: HashMap::new(),
            full_extent_cells: 0,
            cross_sheet: false,
            cross_row: false,
            unsupported_refs: 0,
            dynamic_refs: 0,
            array_formulas: 0,
            self_refs: 0,
            parse_errors: 0,
            first_parse_error: None,
            xml_formula_cells: 0,
            nondeterministic_fns: 0,
            row_sensitive_fns: 0,
            t_read_ms: 0.0,
            t_graph_ms: 0.0,
        }
    }

    // -----------------------------------------------------------------------
    // Scratch path
    // -----------------------------------------------------------------------

    /// A solid block of row-local formulas, which is the shape the scratch
    /// path is built for.
    fn row_local_block(rows: u32) -> Vec<u8> {
        let mut sheet = String::new();
        for r in 1..=rows {
            sheet.push_str(&cell_v(&format!("A{r}"), &r.to_string()));
            sheet.push_str(&cell_v(&format!("B{r}"), &(r * 3).to_string()));
            sheet.push_str(&cell_f(&format!("C{r}"), &format!("A{r}+B{r}")));
            sheet.push_str(&cell_f(&format!("D{r}"), &format!("C{r}*2")));
            sheet.push_str(&cell_f(
                &format!("E{r}"),
                &format!("IF(A{r}&gt;3,SUM(A{r}:B{r}),0)"),
            ));
        }
        xlsx(&[("S", &sheet)])
    }

    fn topo_of(data: &[u8]) -> Topology {
        let mut src = crate::graph::read(data);
        crate::prelude::fold_and_rewrite(data, &mut src);
        crate::graph::build_from(src)
    }

    fn backward_block(rows: u32) -> Vec<u8> {
        let mut sheet = format!("{}{}", cell_v("A1", "10"), cell_v("B1", "3"));
        for r in 2..=rows {
            sheet.push_str(&cell_f(&format!("A{r}"), &format!("A{}+1", r - 1)));
            sheet.push_str(&cell_v(&format!("B{r}"), &(r * 3).to_string()));
            sheet.push_str(&cell_f(&format!("C{r}"), &format!("A{r}+B{r}")));
        }
        xlsx(&[("S", &sheet)])
    }

    #[test]
    fn a_solid_block_of_row_local_formulas_is_accepted() {
        let topo = topo_of(&row_local_block(40));
        let plan = plan_scratch(&topo, 500, DEFAULT_LOOKUP_BUDGET).expect("must be accepted");
        assert_eq!((plan.first_row, plan.last_row), (1, 40));
        assert_eq!(plan.columns.len(), 3, "columns C, D and E hold formulas");
        assert_eq!(plan.data_cols, vec![1, 2], "columns A and B hold data");
        assert_eq!(plan.n_chunks(), 1);
    }

    /// A chain longer than one ingest call must still evaluate row by row.
    ///
    /// Every row reads the row above, so all these formulas are one component.
    /// Splitting such a chain across bulk ingest calls returns stale values
    /// from the split point on, so a component is never split.
    #[test]
    fn a_chain_longer_than_one_ingest_call_is_evaluated_whole() {
        let rows = INGEST_CHUNK as u32 + 200;
        let mut xml = String::new();
        xml.push_str(&format!(r#"<row r="1">{}</row>"#, cell_v("A1", "1")));
        for r in 2..=rows {
            xml.push_str(&format!(
                r#"<row r="{r}">{}</row>"#,
                cell_f(&format!("A{r}"), &format!("A{}+1", r - 1))
            ));
        }
        let data = crate::testkit::xlsx(&[("Sheet1", &xml)]);
        let mut topo = topo_of(&data);
        assert_eq!(topo.comp_cells.len(), 1, "the chain must be one component");

        let store = DataStore::load(&mut topo);
        let got = run(&store, &topo, DEFAULT_BUDGET_CELLS).expect("components must run");
        let mut by_row: Vec<(u32, LiteralValue)> = topo
            .cells
            .iter()
            .enumerate()
            .map(|(i, fc)| (fc.row, got.values[i].clone()))
            .collect();
        by_row.sort_by_key(|&(r, _)| r);
        for (row, value) in by_row {
            assert_eq!(
                value,
                LiteralValue::Number(row as f64),
                "row {row} did not read the row above it"
            );
        }
    }

    /// A range with an open side must read as the whole column, not as a
    /// circular reference. Bulk ingest rejects `A:B` unless both sides are
    /// bounded, and the formula here reads no cell that reads it back.
    #[test]
    fn an_open_sided_range_is_read_as_the_whole_column() {
        let mut rows = String::new();
        for r in 1..=4u32 {
            rows.push_str(&format!(
                r#"<row r="{r}">{}{}{}{}</row>"#,
                cell_v(&format!("A{r}"), &r.to_string()),
                cell_v(&format!("B{r}"), &(r * 10).to_string()),
                cell_v(&format!("D{r}"), &r.to_string()),
                cell_f(&format!("E{r}"), &format!("VLOOKUP(D{r},A:B,2,0)")),
            ));
        }
        let data = crate::testkit::xlsx(&[("Sheet1", &rows)]);
        let mut topo = topo_of(&data);
        let store = DataStore::load(&mut topo);
        let got = run(&store, &topo, DEFAULT_BUDGET_CELLS).expect("components must run");

        let want: Vec<LiteralValue> = (1..=4u32)
            .map(|r| LiteralValue::Number((r * 10) as f64))
            .collect();
        let mut have: Vec<(u32, LiteralValue)> = topo
            .cells
            .iter()
            .enumerate()
            .map(|(i, fc)| (fc.row, got.values[i].clone()))
            .collect();
        have.sort_by_key(|&(r, _)| r);
        let have: Vec<LiteralValue> = have.into_iter().map(|(_, v)| v).collect();
        assert_eq!(have, want, "open-sided lookup must return the matched rows");
    }

    /// The chunk height must not change the answer, and a height below the row
    /// count must produce more than one chunk.
    #[test]
    fn the_scratch_result_matches_the_component_result() {
        let data = row_local_block(120);
        let mut topo = topo_of(&data);
        let store = DataStore::load(&mut topo);
        let want = run(&store, &topo, DEFAULT_BUDGET_CELLS).expect("components must run");

        for chunk_rows in [7, 50, 120, 500] {
            let plan = plan_scratch(&topo, chunk_rows, DEFAULT_LOOKUP_BUDGET).expect("must be accepted");
            let got = run_scratch(&store, &topo, &plan).expect("scratch must run");
            assert_eq!(
                got.values, want.values,
                "chunk height {chunk_rows} changed the result"
            );
        }

        let plan = plan_scratch(&topo, 50, DEFAULT_LOOKUP_BUDGET).expect("must be accepted");
        assert_eq!(plan.n_chunks(), 3, "120 rows need three chunks of 50");
    }

    /// The result from the row above must cross every chunk boundary as a plain
    /// carried value.
    #[test]
    fn backward_references_match_the_component_and_streamed_results() {
        let data = backward_block(40);
        let mut topo = topo_of(&data);
        let store = DataStore::reread(&data, &topo).expect("values must load");
        let want = run(&store, &topo, DEFAULT_BUDGET_CELLS).expect("components must run");

        for chunk_rows in [2, 3, 7, 39, 500] {
            let plan =
                plan_scratch(&topo, chunk_rows, DEFAULT_LOOKUP_BUDGET).expect("must be accepted");
            assert_eq!(plan.carry, 1, "the formula reads one row above");
            let got = run_scratch(&store, &topo, &plan).expect("scratch must run");
            assert_eq!(got.values, want.values, "stored chunk height {chunk_rows}");

            let plan =
                plan_scratch(&topo, chunk_rows, DEFAULT_LOOKUP_BUDGET).expect("must be accepted");
            let mut streamed = ScratchRun::open(&data, &mut topo, plan).expect("stream must open");
            for r in 1..=40 {
                let row = streamed.row(r).expect("row must evaluate");
                for c in [1, 3] {
                    if let Some(&i) = topo.index.get(&(0, r, c)) {
                        assert_eq!(
                            cell_of(row, c),
                            want.values[i as usize],
                            "streamed row {r}, column {c}, chunk height {chunk_rows}"
                        );
                    }
                }
            }
        }
    }

    /// A range may span earlier rows when all of them fit in the carry area.
    #[test]
    fn a_backward_range_crosses_a_chunk_boundary() {
        let mut sheet = format!("{}{}", cell_v("A1", "2"), cell_v("A2", "3"));
        for r in 3..=25 {
            sheet.push_str(&cell_v(&format!("A{r}"), &r.to_string()));
            sheet.push_str(&cell_f(
                &format!("B{r}"),
                &format!("SUM(A{}:A{r})", r - 2),
            ));
        }
        let data = xlsx(&[("S", &sheet)]);
        let mut topo = topo_of(&data);
        let store = DataStore::reread(&data, &topo).expect("values must load");
        let want = run(&store, &topo, DEFAULT_BUDGET_CELLS).expect("components must run");
        let plan = plan_scratch(&topo, 3, DEFAULT_LOOKUP_BUDGET).expect("must be accepted");
        assert_eq!(plan.carry, 2);
        let got = run_scratch(&store, &topo, &plan).expect("scratch must run");
        assert_eq!(got.values, want.values);

        let plan = plan_scratch(&topo, 3, DEFAULT_LOOKUP_BUDGET).expect("must be accepted");
        let mut streamed = ScratchRun::open(&data, &mut topo, plan).expect("stream must open");
        for r in 1..=25 {
            let row = streamed.row(r).expect("row must evaluate");
            if let Some(&i) = topo.index.get(&(0, r, 2)) {
                assert_eq!(cell_of(row, 2), want.values[i as usize], "row {r}");
            }
        }
    }

    /// The carry area may span more than one preceding chunk.
    #[test]
    fn carry_may_be_deeper_than_the_chunk() {
        let mut sheet = String::new();
        for r in 1..=5 {
            sheet.push_str(&cell_v(&format!("A{r}"), &r.to_string()));
        }
        for r in 6..=25 {
            sheet.push_str(&cell_f(&format!("A{r}"), &format!("A{}+5", r - 5)));
        }
        let data = xlsx(&[("S", &sheet)]);
        let mut topo = topo_of(&data);
        let store = DataStore::reread(&data, &topo).expect("values must load");
        let want = run(&store, &topo, DEFAULT_BUDGET_CELLS).expect("components must run");
        let plan = plan_scratch(&topo, 2, DEFAULT_LOOKUP_BUDGET).expect("must be accepted");
        assert_eq!(plan.carry, 5);
        let got = run_scratch(&store, &topo, &plan).expect("scratch must run");
        assert_eq!(got.values, want.values);

        let plan = plan_scratch(&topo, 2, DEFAULT_LOOKUP_BUDGET).expect("must be accepted");
        let mut streamed = ScratchRun::open(&data, &mut topo, plan).expect("stream must open");
        for r in 1..=25 {
            let row = streamed.row(r).expect("row must evaluate");
            if let Some(&i) = topo.index.get(&(0, r, 1)) {
                assert_eq!(cell_of(row, 1), want.values[i as usize], "row {r}");
            }
        }
    }

    /// A carried stream cannot skip the rows that supply its next chunk.
    #[test]
    fn a_carried_stream_refuses_a_jump() {
        let data = backward_block(20);
        let mut topo = topo_of(&data);
        let plan = plan_scratch(&topo, 5, DEFAULT_LOOKUP_BUDGET).expect("must be accepted");
        let mut run = ScratchRun::open(&data, &mut topo, plan).expect("stream must open");
        assert!(run.row(7).is_err(), "row 6 was not read for the carry area");
    }

    /// The last chunk is shorter than the scratch sheet. Its unused rows must be
    /// blanked, or a value from the chunk before would still be there.
    #[test]
    fn a_short_last_chunk_does_not_reuse_the_chunk_before() {
        let data = row_local_block(23);
        let mut topo = topo_of(&data);
        let store = DataStore::load(&mut topo);
        let want = run(&store, &topo, DEFAULT_BUDGET_CELLS).expect("components must run");
        let plan = plan_scratch(&topo, 10, DEFAULT_LOOKUP_BUDGET).expect("must be accepted");
        assert_eq!(plan.n_chunks(), 3, "23 rows need three chunks of 10");
        let got = run_scratch(&store, &topo, &plan).expect("scratch must run");
        assert_eq!(got.values, want.values);
    }

    /// Every rule that protects the move to another row.
    #[test]
    fn shapes_the_scratch_path_must_refuse() {
        // A forward reference names a chunk that is not evaluated yet.
        let mut sheet = String::new();
        for r in 1..=8 {
            sheet.push_str(&cell_f(&format!("A{r}"), &format!("B{}+1", r + 1)));
            sheet.push_str(&cell_v(&format!("B{r}"), "1"));
        }
        assert!(plan_scratch(
            &topo_of(&xlsx(&[("S", &sheet)])),
            500,
            DEFAULT_LOOKUP_BUDGET
        )
        .is_err());

        // A pinned row does not move with the formula.
        let mut sheet = cell_v("A1", "5");
        for r in 1..=8 {
            sheet.push_str(&cell_f(&format!("B{r}"), "$A$1*2"));
        }
        assert!(plan_scratch(&topo_of(&xlsx(&[("S", &sheet)])), 500, DEFAULT_LOOKUP_BUDGET).is_err());

        // A back-reference deeper than the carry limit is not worth a scratch
        // sheet that large.
        let sheet = format!("{}{}", cell_v("A1", "1"), cell_f("B502", "A1+1"));
        assert_eq!(
            plan_scratch(
                &topo_of(&xlsx(&[("S", &sheet)])),
                500,
                DEFAULT_LOOKUP_BUDGET
            )
            .err(),
            Some("a formula reads too far above its own row")
        );

        // A position-sensitive call gives another answer after the move.
        let mut sheet = String::new();
        for r in 1..=8 {
            sheet.push_str(&cell_v(&format!("A{r}"), "1"));
            sheet.push_str(&cell_f(&format!("B{r}"), &format!("A{r}+ROW()")));
        }
        assert!(plan_scratch(&topo_of(&xlsx(&[("S", &sheet)])), 500, DEFAULT_LOOKUP_BUDGET).is_err());

        // A nondeterministic call gives another answer in every engine. TODAY
        // is not one, because the pinned clock reproduces it.
        let mut sheet = String::new();
        for r in 1..=8 {
            sheet.push_str(&cell_v(&format!("A{r}"), "1"));
            sheet.push_str(&cell_f(&format!("B{r}"), &format!("A{r}+RAND()")));
        }
        assert!(plan_scratch(&topo_of(&xlsx(&[("S", &sheet)])), 500, DEFAULT_LOOKUP_BUDGET).is_err());

        // A gap in a column would put a formula where the source has none.
        let mut sheet = String::new();
        for r in 1..=8 {
            sheet.push_str(&cell_v(&format!("A{r}"), "1"));
            if r != 4 {
                sheet.push_str(&cell_f(&format!("B{r}"), &format!("A{r}*2")));
            }
        }
        assert!(plan_scratch(&topo_of(&xlsx(&[("S", &sheet)])), 500, DEFAULT_LOOKUP_BUDGET).is_err());

        // Two columns over different rows would do the same.
        let mut sheet = String::new();
        for r in 1..=8 {
            sheet.push_str(&cell_v(&format!("A{r}"), "1"));
            sheet.push_str(&cell_f(&format!("B{r}"), &format!("A{r}*2")));
            if r <= 4 {
                sheet.push_str(&cell_f(&format!("C{r}"), &format!("A{r}+1")));
            }
        }
        assert!(plan_scratch(&topo_of(&xlsx(&[("S", &sheet)])), 500, DEFAULT_LOOKUP_BUDGET).is_err());

        // Two sheets holding formulas is out of scope for this path.
        let one = format!("{}{}", cell_v("A1", "1"), cell_f("B1", "A1+1"));
        let two = format!("{}{}", cell_v("A1", "1"), cell_f("B1", "A1+1"));
        assert!(plan_scratch(&topo_of(&xlsx(&[("One", &one), ("Two", &two)])), 500, DEFAULT_LOOKUP_BUDGET).is_err());
    }

    /// A relative reference into another sheet must be refused.
    ///
    /// The other sheet is copied once at its own coordinates, but a relative
    /// reference moves with the formula. A column of
    /// `=IF(Sheet2!A2="","",Sheet2!A2)` would otherwise read one row too high
    /// on every row of the block.
    #[test]
    fn a_relative_reference_into_another_sheet_is_refused() {
        let mut main = String::new();
        for r in 2..=12 {
            main.push_str(&cell_f(
                &format!("A{r}"),
                &format!("IF(Two!A{r}=\"\",\"\",Two!A{r})"),
            ));
        }
        let mut two = String::new();
        for r in 1..=12 {
            two.push_str(&cell_v(&format!("A{r}"), &(r * 11).to_string()));
        }
        let topo = topo_of(&xlsx(&[("One", &main), ("Two", &two)]));
        assert!(
            plan_scratch(&topo, 500, DEFAULT_LOOKUP_BUDGET).is_err(),
            "a cross-sheet reference that moves with the row must be refused"
        );
    }

    /// The same shape pinned with `$` does not move, so it is accepted and gives
    /// the same values as the component path.
    #[test]
    fn a_pinned_reference_into_another_sheet_is_accepted() {
        let mut main = String::new();
        for r in 2..=12 {
            main.push_str(&cell_v(&format!("B{r}"), &r.to_string()));
            main.push_str(&cell_f(&format!("A{r}"), &format!("B{r}+Two!$A$3")));
        }
        let mut two = String::new();
        for r in 1..=12 {
            two.push_str(&cell_v(&format!("A{r}"), &(r * 11).to_string()));
        }
        let data = xlsx(&[("One", &main), ("Two", &two)]);
        let mut topo = topo_of(&data);
        let store = DataStore::load(&mut topo);
        let want = run(&store, &topo, DEFAULT_BUDGET_CELLS).expect("components must run");
        let plan = plan_scratch(&topo, 4, DEFAULT_LOOKUP_BUDGET).expect("must be accepted");
        let got = run_scratch(&store, &topo, &plan).expect("scratch must run");
        assert_eq!(got.values, want.values);
    }

    /// The streamed run must return exactly what the store-backed run holds:
    /// the computed value at every formula cell, and the source value at every
    /// data cell.
    #[test]
    fn the_streamed_rows_match_the_scratch_result() {
        let data = row_local_block(120);
        let mut topo = topo_of(&data);
        let store = DataStore::reread(&data, &topo).expect("values must load");

        for chunk_rows in [7, 50, 500] {
            let plan =
                plan_scratch(&topo, chunk_rows, DEFAULT_LOOKUP_BUDGET).expect("must be accepted");
            let want = run_scratch(&store, &topo, &plan).expect("scratch must run");
            let plan =
                plan_scratch(&topo, chunk_rows, DEFAULT_LOOKUP_BUDGET).expect("must be accepted");
            let mut got = ScratchRun::open(&data, &mut topo, plan).expect("the stream must open");

            for r in 1..=120u32 {
                let row = got.row(r).expect("the row must be read").to_vec();
                for c in 1..=5u32 {
                    let value = cell_of(&row, c);
                    match topo.index.get(&(0, r, c)) {
                        Some(&i) => assert_eq!(
                            value, want.values[i as usize],
                            "formula cell at row {r} column {c}"
                        ),
                        None => assert_eq!(
                            value,
                            store.get(0, r, c).unwrap_or(LiteralValue::Empty),
                            "data cell at row {r} column {c}"
                        ),
                    }
                }
            }
        }
    }

    /// Rows must be asked for in ascending order, because the stream reads the
    /// part once and never goes back.
    #[test]
    fn a_row_before_the_window_is_refused() {
        let data = row_local_block(40);
        let mut topo = topo_of(&data);
        let plan = plan_scratch(&topo, 10, DEFAULT_LOOKUP_BUDGET).expect("must be accepted");
        let mut run = ScratchRun::open(&data, &mut topo, plan).expect("the stream must open");
        run.row(21).expect("the row must be read");
        assert!(run.row(3).is_err(), "a row behind the window must be refused");
    }

    /// The stream reads the part once, in the order the part holds it. A sheet
    /// whose rows are written out of order is therefore given back, and the
    /// caller uses the store path.
    #[test]
    fn a_sheet_with_rows_out_of_order_is_not_streamed() {
        let mut sheet = String::new();
        for r in (1..=40u32).rev() {
            sheet.push_str(&cell_v(&format!("A{r}"), &r.to_string()));
            sheet.push_str(&cell_f(&format!("B{r}"), &format!("A{r}*2")));
        }
        let data = xlsx(&[("S", &sheet)]);
        let mut topo = topo_of(&data);
        assert!(!topo.sheets[0].rows_ascending, "the rows are written backwards");
        let plan = plan_scratch(&topo, 500, DEFAULT_LOOKUP_BUDGET).expect("must be accepted");
        assert!(
            ScratchRun::open(&data, &mut topo, plan).is_err(),
            "a sheet the stream cannot follow must be given back"
        );
    }

    /// A lookup costs the height of its table on every call, so a file whose
    /// lookups cost more than the budget is given back to the caller.
    #[test]
    fn lookups_over_the_budget_are_refused() {
        let mut main = String::new();
        for r in 1..=30 {
            main.push_str(&cell_v(&format!("A{r}"), &((r % 5) + 1).to_string()));
            main.push_str(&cell_f(
                &format!("B{r}"),
                &format!("VLOOKUP(A{r},Lookup!$A$1:$B$5,2,FALSE)"),
            ));
        }
        let mut lookup = String::new();
        for r in 1..=5 {
            lookup.push_str(&cell_v(&format!("A{r}"), &r.to_string()));
            lookup.push_str(&cell_v(&format!("B{r}"), &(r * 100).to_string()));
        }
        let topo = topo_of(&xlsx(&[("Main", &main), ("Lookup", &lookup)]));

        // 30 calls against a 5-row table.
        assert_eq!(topo.lookup_work(), 150);
        assert!(plan_scratch(&topo, 500, 149).is_err(), "over the budget");
        assert!(plan_scratch(&topo, 500, 150).is_ok(), "at the budget");
    }

    /// A defined name may point at a sheet the workbook never declares. The
    /// whole-file loader creates that sheet, empty, so the reader reports it
    /// too; otherwise the two paths return a different set of sheets for the
    /// same file.
    #[test]
    fn a_name_pointing_at_a_missing_sheet_is_still_reported() {
        let t = topo_of(&xlsx_with_defined_names(
            &[("S", &cell_f("B2", "A2+1"))],
            r#"<definedName name="Gone">'Old Data'!$A$1:$B$2</definedName>"#,
        ));
        assert_eq!(t.name_only_sheets, vec!["Old Data".to_string()]);

        // A name on a sheet that exists, an external workbook, and a name that
        // is not a reference at all invent nothing.
        let t = topo_of(&xlsx_with_defined_names(
            &[("S", &cell_f("B2", "A2+1"))],
            concat!(
                r#"<definedName name="Here">S!$A$1</definedName>"#,
                r#"<definedName name="Far">'[1]Book'!$A$1</definedName>"#,
                r#"<definedName name="Broken">#REF!</definedName>"#,
            ),
        ));
        assert!(t.name_only_sheets.is_empty(), "{:?}", t.name_only_sheets);
    }

    /// A declared cell with no value counts in `COUNTBLANK`, so the store
    /// keeps the blanks a formula range covers and the mini-workbook declares
    /// them too. The oracle loads through the engine like a whole-file run
    /// does. (`COUNTIF` with an `""` criterion is gated to the whole-file
    /// path instead: `""` matching depends on how the workbook was built.
    /// See `text-criteria aggregates` and the `lib` verdict test.)
    #[test]
    fn blank_cells_count_in_a_partitioned_countblank() {
        let mut sheet = String::new();
        for r in 2..=35u32 {
            sheet.push_str(&cell_v(&format!("G{r}"), &r.to_string()));
        }
        sheet.push_str(r#"<c r="G36" s="26" t="n"/>"#);
        sheet.push_str(r#"<c r="G37" s="26" t="n"/>"#);
        // `COUNTBLANK` is not folded, so it goes through the store and the
        // batch workbook.
        sheet.push_str(&cell_f("G44", "COUNTBLANK(G$2:G$37)"));
        let data = xlsx(&[("S", &sheet)]);
        let mut topo = topo_of(&data);
        assert_eq!(topo.cells.len(), 1);
        let store = DataStore::load(&mut topo);
        // The range the formula reads keeps its blanks.
        assert_eq!(store.get(0, 36, 7), Some(LiteralValue::Empty));
        assert_eq!(store.get(0, 37, 7), Some(LiteralValue::Empty));
        let result = run(&store, &topo, DEFAULT_BUDGET_CELLS).unwrap();
        let adapter = <CalamineAdapter as SpreadsheetReader>::open_bytes(data).unwrap();
        let mut whole = Workbook::from_reader(
            adapter,
            LoadStrategy::EagerAll,
            crate::clock::pin(WorkbookConfig::interactive()),
        )
        .unwrap();
        whole.evaluate_all().unwrap();
        assert_eq!(result.values[0], LiteralValue::Number(2.0));
        assert_eq!(Some(result.values[0].clone()), whole.get_value("S", 44, 7));
    }

    /// `_xHHHH_` escapes Excel writes for characters XML cannot hold decode
    /// like the whole-file backend, on both the shared-string and inline
    /// string paths.
    #[test]
    fn x_escaped_strings_read_like_the_whole_file_run() {
        let sheet = format!(
            "{}{}",
            r#"<c r="B4" t="inlineStr"><is><t>1 , 2_x000D_</t></is></c>"#,
            cell_s("B5", 0),
        );
        let data = xlsx_with_shared_strings(&[("S", &sheet)], &["shared_x000D_"]);
        let mut topo = topo_of(&data);
        let store = DataStore::load(&mut topo);
        assert_eq!(store.get(0, 4, 2), Some(LiteralValue::Text("1 , 2\r".into())));
        assert_eq!(store.get(0, 5, 2), Some(LiteralValue::Text("shared\r".into())));
        assert_eq!(whole_value(&data, "S", 4, 2), Some(LiteralValue::Text("1 , 2\r".into())));
        assert_eq!(whole_value(&data, "S", 5, 2), Some(LiteralValue::Text("shared\r".into())));
    }

    /// A spilled array writes cells no formula read, so a batch that copies
    /// only the dependency closure can miss a blocker. When the batch
    /// anchors a spill, the run rebuilds it with whole-file occupancy: the
    /// engine then blocks exactly where the whole-file run blocks, and the
    /// spilled values of a free spill are served from the overlay.
    #[test]
    fn a_batch_spill_reproduces_the_whole_file_decision() {
        // Blocked: B3 holds a value the whole-file run sees, so the anchor
        // reports the spill error and nothing is overlaid.
        let blocked = spill_fixture(true);
        let mut topo = topo_of(&blocked);
        let store = DataStore::load(&mut topo);
        let result = run(&store, &topo, DEFAULT_BUDGET_CELLS).unwrap();
        assert_eq!(result.values[0], spill_error());
        assert!(result.spilled.is_empty(), "{:?}", result.spilled);
        assert_eq!(whole_value(&blocked, "S", 2, 2), Some(spill_error()));

        // Free: both runs spill identically, so the anchor holds the first
        // element and the overlay serves the footprint the store lacks.
        let free = spill_fixture(false);
        let mut topo = topo_of(&free);
        let store = DataStore::load(&mut topo);
        let result = run(&store, &topo, DEFAULT_BUDGET_CELLS).unwrap();
        assert_eq!(result.values[0], LiteralValue::Number(10.0));
        assert_eq!(
            result.spilled,
            vec![
                (0u16, 3u32, 2u32, LiteralValue::Number(20.0)),
                (0u16, 4u32, 2u32, LiteralValue::Number(30.0)),
            ]
        );
        assert_eq!(whole_value(&free, "S", 2, 2), Some(LiteralValue::Number(10.0)));
    }

    /// Engine-behavior guard for the text-criteria ungate: a text criterion
    /// over a numeric range must coerce, and a wildcard must match text
    /// only, on the whole-file path itself. A stale engine revision (one
    /// whose base text lane holds text only) fails these loudly instead of
    /// silently answering differently from a batch.
    #[test]
    fn text_criteria_coerce_on_the_whole_file_path() {
        let mut sheet = String::new();
        sheet.push_str(&cell_v("A1", "1"));
        sheet.push_str(&cell_v("A2", "2"));
        sheet.push_str(&cell_v("A3", "100"));
        sheet.push_str(&cell_v("A4", "1x"));
        sheet.push_str(&cell_f("C1", r#"COUNTIF(A1:A4,"1")"#));
        sheet.push_str(&cell_f("C2", r#"COUNTIF(A1:A4,"1*")"#));
        let data = xlsx(&[("S", &sheet)]);
        // The numeric 1 matches the text criterion (Excel coercion).
        assert_eq!(whole_value(&data, "S", 1, 3), Some(LiteralValue::Number(1.0)));
        // The wildcard matches the text "1x" only, not the numbers.
        assert_eq!(whole_value(&data, "S", 2, 3), Some(LiteralValue::Number(1.0)));
    }

    /// Engine-behavior guard for the reducer screen. If a future engine
    /// adds elementwise lifting, `ABS(A1:A3)` would spill and
    /// `REDUCING_FNS` would hide it; these assertions must fail loudly
    /// then.
    #[test]
    fn reducers_consume_arrays_without_lifting() {
        let sheet = format!(
            "{}{}{}{}{}",
            r#"<c r="A1" t="inlineStr"><is><t>abc</t></is></c>"#,
            cell_v("A2", "20"),
            cell_v("A3", "30"),
            cell_f("B1", "ABS(A1:A3)"),
            cell_f("B2", "LEN(A1:A3)"),
        );
        let data = xlsx(&[("S", &sheet)]);
        // ABS over a range is a value error, not a lifted array.
        assert_eq!(
            whole_value(&data, "S", 1, 2),
            Some(LiteralValue::Error(formualizer::common::error::ExcelError::new(
                formualizer::common::error::ExcelErrorKind::Value
            )))
        );
        // LEN takes the implicit intersection.
        assert_eq!(whole_value(&data, "S", 2, 2), Some(LiteralValue::Number(3.0)));
    }

    /// Two footprints that overlap with neither anchor inside the other
    /// resolve by placement order, which inside one batch is the source
    /// order the whole run uses; across batches the run refuses instead of
    /// guessing.
    #[test]
    fn overlapping_spill_footprints_follow_source_order_or_refuse() {
        let sheet = format!(
            "{}{}",
            cell_f("E1", "SEQUENCE(5,2)"),
            cell_f("B3", "SEQUENCE(2,5)"),
        );
        let data = xlsx(&[("S", &sheet)]);

        // One batch: the engine resolves the overlap itself, and the
        // row-major placement order matches the whole run's source order.
        let mut topo = topo_of(&data);
        let store = DataStore::load(&mut topo);
        let result = run(&store, &topo, DEFAULT_BUDGET_CELLS).unwrap();
        assert_eq!(result.values[0], LiteralValue::Number(1.0));
        assert_eq!(result.values[1], spill_error());
        assert_eq!(whole_value(&data, "S", 1, 5), Some(LiteralValue::Number(1.0)));
        assert_eq!(whole_value(&data, "S", 3, 2), Some(spill_error()));

        // Separate batches: neither batch sees the other's footprint, so
        // the run refuses rather than return two winners.
        let mut topo = topo_of(&data);
        let store = DataStore::load(&mut topo);
        let err = run(&store, &topo, 1).unwrap_err();
        assert!(err.contains("overlap"), "{err}");
    }

    /// A footprint that covers another formula's cell makes the whole run
    /// fail its evaluation outright; the run refuses instead of returning
    /// values the whole run would not have.
    #[test]
    fn a_footprint_over_a_foreign_formula_refuses_to_answer() {
        // B2 spills B2:B4; B4 is a formula in its own component. A budget
        // of 4 splits the components, so pass 2 would seed B4 with a
        // sentinel value and return a clean #SPILL!; the guard refuses.
        let sheet = format!(
            "{}{}",
            cell_f("B2", "INDEX($A$1:$A$3,0)"),
            cell_f("B4", "1+1"),
        );
        let data = xlsx(&[("S", &sheet)]);
        let mut topo = topo_of(&data);
        let store = DataStore::load(&mut topo);
        let err = run(&store, &topo, 4).unwrap_err();
        assert!(err.contains("formula cell"), "{err}");

        // In one batch the engine itself refuses the footprint over a
        // formula, exactly as the whole run does.
        let mut topo = topo_of(&data);
        let store = DataStore::load(&mut topo);
        let err = run(&store, &topo, DEFAULT_BUDGET_CELLS).unwrap_err();
        assert!(err.contains("BlockedByFormula"), "{err}");
    }

    /// The engine lifts scalar arithmetic over a range to an array, so no
    /// function-name list can enumerate the spilling shapes. The screen
    /// flags the range instead, and the batch resolves the spill against
    /// whole-file occupancy.
    #[test]
    fn lifted_scalar_arithmetic_resolves_like_an_array_call() {
        let mut sheet = String::new();
        for (r, v) in [(1u32, "10"), (2, "20"), (3, "30")] {
            sheet.push_str(&cell_v(&format!("A{r}"), v));
        }
        sheet.push_str(&cell_f("B2", "A1:A3+0"));
        sheet.push_str(&cell_v("B3", "0"));
        let data = xlsx(&[("S", &sheet)]);
        let mut topo = topo_of(&data);
        let store = DataStore::load(&mut topo);
        let result = run(&store, &topo, DEFAULT_BUDGET_CELLS).unwrap();
        assert_eq!(result.values[0], spill_error());
        assert_eq!(whole_value(&data, "S", 2, 2), Some(spill_error()));
    }

    /// `INDEX(A1:A3,0)` spills `B2:B4`. With `blocked` a value sits at `B3`.
    fn spill_fixture(blocked: bool) -> Vec<u8> {
        let mut sheet = String::new();
        for (r, v) in [(1u32, "10"), (2, "20"), (3, "30")] {
            sheet.push_str(&cell_v(&format!("A{r}"), v));
        }
        sheet.push_str(&cell_f("B2", "INDEX($A$1:$A$3,0)"));
        if blocked {
            sheet.push_str(&cell_v("B3", "0"));
        }
        xlsx(&[("S", &sheet)])
    }

    /// The whole-file value of one cell, loaded through the engine.
    fn whole_value(data: &[u8], sheet: &str, row: u32, col: u32) -> Option<LiteralValue> {
        let adapter = <CalamineAdapter as SpreadsheetReader>::open_bytes(data.to_vec()).unwrap();
        let mut whole = Workbook::from_reader(
            adapter,
            LoadStrategy::EagerAll,
            crate::clock::pin(WorkbookConfig::interactive()),
        )
        .unwrap();
        whole.evaluate_all().unwrap();
        whole.get_value(sheet, row, col)
    }

    fn spill_error() -> LiteralValue {
        LiteralValue::Error(formualizer::common::error::ExcelError::new(
            formualizer::common::error::ExcelErrorKind::Spill,
        ))
    }

    /// A `#REF!` literal carries no dependency edge, so a formula holding one
    /// partitions and must land on the same cell with the same value as a
    /// whole-file run.
    #[test]
    fn ref_error_literals_match_a_whole_file_run() {
        let formulas = [
            ("B1", "IFERROR(#REF!,7)"),
            ("B2", "'Missing'!#REF!"),
            ("B3", "VLOOKUP(A1,#REF!,2,0)"),
            ("B4", "'Data Set'!A1+1"),
        ];
        let main: String = formulas.iter().map(|(cell, text)| cell_f(cell, text)).collect();
        let data = xlsx(&[("S", &main), ("Data Set", &format!("{}{}", cell_v("A1", "11"), cell_v("A2", "17")))]);
        let topo = topo_of(&data);
        assert!(topo.is_partitionable());
        let store = DataStore::reread(&data, &topo).unwrap();
        let result = run(&store, &topo, 2).unwrap();
        let mut whole = Workbook::new();
        whole.add_sheet("S").unwrap();
        whole.add_sheet("Data Set").unwrap();
        whole.set_value("Data Set", 1, 1, LiteralValue::Number(11.0)).unwrap();
        whole.set_value("Data Set", 2, 1, LiteralValue::Number(17.0)).unwrap();
        for (i, (_, formula)) in formulas.iter().enumerate() {
            whole.set_formula("S", i as u32 + 1, 2, &format!("={formula}")).unwrap();
        }
        whole.evaluate_all().unwrap();
        for (i, cell) in topo.cells.iter().enumerate() {
            assert_eq!(Some(result.values[i].clone()), whole.get_value("S", cell.row, cell.col));
        }
    }

    /// `NOW` and `TODAY` are Excel-volatile, but the run clock pins one instant
    /// for every workbook a call builds, so a batch and a whole-file run report
    /// the same value. Without that pin the two differ by the microseconds
    /// between the two engines.
    #[test]
    fn clock_calls_match_a_whole_file_run() {
        let formulas = [
            ("B1", "TODAY()"),
            ("B2", "NOW()"),
            ("B3", "A1+TODAY()"),
            ("B4", "YEAR(TODAY())"),
        ];
        let main: String = std::iter::once(cell_v("A1", "2"))
            .chain(formulas.iter().map(|(cell, text)| cell_f(cell, text)))
            .collect();
        let data = xlsx(&[("S", &main)]);
        let topo = topo_of(&data);
        assert!(topo.is_partitionable(), "a pinned clock is reproducible");
        let store = DataStore::reread(&data, &topo).unwrap();
        let result = run(&store, &topo, 2).unwrap();
        let mut whole = Workbook::new_with_config(crate::clock::pin(WorkbookConfig::ephemeral()));
        whole.add_sheet("S").unwrap();
        whole.set_value("S", 1, 1, LiteralValue::Number(2.0)).unwrap();
        for (i, (_, formula)) in formulas.iter().enumerate() {
            whole.set_formula("S", i as u32 + 1, 2, &format!("={formula}")).unwrap();
        }
        whole.evaluate_all().unwrap();
        for (i, cell) in topo.cells.iter().enumerate() {
            assert_eq!(Some(result.values[i].clone()), whole.get_value("S", cell.row, cell.col));
        }
    }

    #[test]
    fn a_fixed_workbook_name_is_available_to_stored_and_streamed_scratch() {
        let mut main = String::new();
        for r in 1..=12 {
            let key = (r % 5) + 1;
            main.push_str(&cell_v(&format!("A{r}"), &key.to_string()));
            main.push_str(&cell_f(
                &format!("B{r}"),
                &format!("VLOOKUP(A{r},rates,2,FALSE)"),
            ));
        }
        let mut lookup = String::new();
        for r in 1..=5 {
            lookup.push_str(&cell_v(&format!("A{r}"), &r.to_string()));
            lookup.push_str(&cell_v(&format!("B{r}"), &(r * 100).to_string()));
        }
        let data = xlsx_with_defined_names(
            &[("Main", &main), ("Lookup", &lookup)],
            r#"<definedName name="Rates">Lookup!$A$1:$B$5</definedName>"#,
        );
        let mut topo = topo_of(&data);
        assert_eq!(topo.named_refs, 12);
        assert_eq!(topo.lookup_work(), 60);
        let store = DataStore::reread(&data, &topo).expect("values must load");

        for chunk_rows in [3, 8, 500] {
            let plan = plan_scratch(&topo, chunk_rows, DEFAULT_LOOKUP_BUDGET)
                .expect("the fixed name must be accepted");
            assert_eq!(plan.static_names.len(), 1);
            let got = run_scratch(&store, &topo, &plan).expect("scratch must run");
            for (i, cell) in topo.cells.iter().enumerate() {
                let key = (cell.row % 5) + 1;
                assert_eq!(got.values[i], LiteralValue::Number((key * 100) as f64));
            }
        }

        let plan = plan_scratch(&topo, 4, DEFAULT_LOOKUP_BUDGET)
            .expect("the fixed name must be accepted");
        let mut streamed = ScratchRun::open(&data, &mut topo, plan).expect("stream must open");
        for r in 1..=12 {
            let key = (r % 5) + 1;
            assert_eq!(
                cell_of(streamed.row(r).expect("row must evaluate"), 2),
                LiteralValue::Number((key * 100) as f64)
            );
        }
    }

    #[test]
    fn scratch_refuses_names_that_cannot_keep_their_scope_or_address() {
        let main = format!("{}{}", cell_v("A1", "1"), cell_f("B1", "Fixed+A1"));
        let same_sheet = xlsx_with_defined_names(
            &[("Main", &main)],
            r#"<definedName name="Fixed">Main!$A$1</definedName>"#,
        );
        assert_eq!(
            plan_scratch(&topo_of(&same_sheet), 500, DEFAULT_LOOKUP_BUDGET).err(),
            Some("a defined name targets the formula sheet")
        );

        let local_name = xlsx_with_defined_names(
            &[("Main", &main), ("Lookup", &cell_v("A1", "10"))],
            r#"<definedName name="Fixed" localSheetId="0">Lookup!$A$1</definedName>"#,
        );
        assert_eq!(
            plan_scratch(&topo_of(&local_name), 500, DEFAULT_LOOKUP_BUDGET).err(),
            Some("sheet-scoped defined names")
        );
    }

    /// A lookup table on another sheet holds data only. It is copied once, and
    /// the formulas that read it still move row by row.
    #[test]
    fn a_static_sheet_is_copied_once_and_read_correctly() {
        let mut main = String::new();
        for r in 1..=30 {
            main.push_str(&cell_v(&format!("A{r}"), &((r % 5) + 1).to_string()));
            main.push_str(&cell_f(
                &format!("B{r}"),
                &format!("VLOOKUP(A{r},Lookup!$A$1:$B$5,2,FALSE)"),
            ));
        }
        let mut lookup = String::new();
        for r in 1..=5 {
            lookup.push_str(&cell_v(&format!("A{r}"), &r.to_string()));
            lookup.push_str(&cell_v(&format!("B{r}"), &(r * 100).to_string()));
        }
        let data = xlsx(&[("Main", &main), ("Lookup", &lookup)]);

        let mut topo = topo_of(&data);
        let store = DataStore::load(&mut topo);
        let want = run(&store, &topo, DEFAULT_BUDGET_CELLS).expect("components must run");

        let plan = plan_scratch(&topo, 8, DEFAULT_LOOKUP_BUDGET).expect("must be accepted");
        assert!(!plan.static_ranges.is_empty(), "the lookup table is static");
        let got = run_scratch(&store, &topo, &plan).expect("scratch must run");
        assert_eq!(got.values, want.values);
    }
}

