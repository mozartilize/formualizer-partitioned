# Mini-workbook evaluation

Contracts for how a partitioned piece is built and evaluated. Layout admission
(`plan_scratch`, component batches, names, spills) lives in
[`src/partition.rs`](../src/partition.rs). This page is the copy/eval rules
that keep a piece equal to Formualizer whole-file.

## Mini-workbooks

Batch and chunk workbooks start with `WorkbookConfig::ephemeral()` and the
run's pinned clock. Before construction, apply the source workbook's `<calcPr>`
settings with Formualizer's `apply_calc_settings_to_cycle`. Iteration requires
both runtime cycle detection and the source iteration count/tolerance; default
cycle handling returns `Circ` instead of iterating.

Do not copy the xlsx loader's ingest-only settings (`SheetIndexMode::Lazy`,
`range_expansion_limit = 0`, `defer_graph_building`). Those are restored
before whole-file `evaluate_all`. Leaving them on a mini-workbook keeps
range formulas (`SUM(B2:C2)`) as Arrow ranges over empty base cells, so they
miss results that other formulas in the same piece just computed.

## Copying inputs (`copy_inputs`)

Only cells the piece reads. Formula cells are skipped: a referenced formula
is always in the same component and is placed as a formula.

Per sheet, after gathering unique stored cells:

| sheet shape | path |
|---|---|
| packed A1-origin rectangle (`n == max_row * max_col`) | `begin_bulk_ingest_arrow` + `append_row` |
| anything looser | empty Arrow sheet + `begin_bulk_update_arrow` on present cells |
| Date / DateTime / Time / Duration | `set_value` (overlay does not stamp those formats) |

Do not fill holes with `Empty`. Missing and Empty both count as blank for
`COUNTBLANK`, but allocating Empty cells inflates RSS.

`C2+1` on a date cell must stay a `Date`. Overlay-only writes turn it into
a serial `Number`, and `TEXT(...,"DD")` then prints the serial.

## Chunk height

`chunk_rows = min(requested, last_row - first_row + 1)` (at least 1).

A scratch sheet taller than the formula block places empty-tail formulas that
still evaluate (COUNTIF over dummy rows, and so on).

Iterative workbooks cannot reuse a scratch workbook across chunks: previous
formula results seed the next cycle and change convergence. If more than one
chunk is needed, use components instead. A single scratch chunk is allowed.

## Writes

Per-cell `set_value` runs dirty propagation. Wrap a chunk's carry/data writes
and occupancy seeding in one `begin_deferred_dirty` / `end_deferred_dirty`
so that cost is O(cells), not O(cells²).

Evaluate with `evaluate_all` on the mini-workbook. Targeted `evaluate_cells`
is not faster on this path.

## Names

Chunk layout: workbook-scoped names targeting a fixed absolute range on
another sheet.

Component layout: those, plus sheet-scoped names. Recreate scope and target
sheets even when empty; keep shadowed pairs; a bare `$A$1` inherits its
scope sheet.
