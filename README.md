# formualizer-partitioned

An unofficial extension to [formualizer](https://github.com/PSU3D0/formualizer).
It evaluates xlsx formulas with a smaller memory peak by splitting a workbook
into bounded pieces — a reused workbook evaluated a chunk of rows at a time,
or one workbook per batch of independent dependency components — and streaming
the resulting rows to Python. The Python module is `formualizer_partitioned`.

Formula semantics come entirely from [formualizer]. This crate only decides
*what to evaluate together* and *when to release it*. When it cannot prove that
splitting is safe, it evaluates the whole file. Splitting does not change a
result, with one measured caveat: a folded whole-column aggregate can differ
in the last floating-point digit (see "Prelude folding" in
[`bench/README.md`](bench/README.md)).

[formualizer]: https://github.com/PSU3D0/formualizer

## Why

The current pipeline loads a workbook, evaluates it whole, and returns every
cell to Python. Peak memory scales with the sheet extent, which is what pushes
the Lambda memory setting up. A 170 KB workbook whose formulas read a full
column peaks at 117 MB evaluated whole, and 8.8 MB when split.

Most spreadsheets are not one large calculation. They are thousands of small,
independent calculations. Each row computes from its own inputs. The evaluator
can use separate workbooks and release each one after it reads the results.

## Usage

```python
import formualizer_partitioned

# Stream rows: only one row is live at a time.
for sheet, row_number, values in formualizer_partitioned.eval_rows(data):
    store(json.dumps(values, default=str))

# Return a whole grid.
grid = formualizer_partitioned.eval_grid(data)          # always evaluates the whole file
grid = formualizer_partitioned.eval_partitioned(data)   # partitions when safe

# Inspect the decision without evaluating.
formualizer_partitioned.partition_plan(data)
# {'partitioned': True, 'fallback_reason': None, 'strategy': 'partitioned',
#  'chunk_reason': None, 'n_formula_cells': 45318, 'n_formula_sources': 716,
#  'n_components': 30325, 'prelude_folded': 4, 'prelude_chunks': 200, ...}
```

In the plan, `partitioned` is true for either split layout and `strategy`
says which one. `n_batches` is the component-batch estimate; the chunk layout
does not batch that way.

`eval_rows` and `eval_partitioned` accept these parameters:

- `budget_cells` sets the number of cells per batch.
- `max_ratio` sets the largest permitted component ratio. The default is 0.9.
- `min_formulas` sets the minimum formula count. A value of 0 forces safe
  partitioning.
- `lookup_budget` sets the number of lookup-table rows that the fast path can
  scan. See [`src/partition.rs`](src/partition.rs).
- `prelude` folds whole-column aggregates into numbers before the graph build.
  It is on by default; turning it off can reduce speed, but cannot change a
  result.

`eval_rows` returns an iterator that reports what it did:

```python
rows = formualizer_partitioned.eval_rows(data)
rows.strategy         # 'partitioned', 'components' or 'whole'
rows.fallback_reason  # why a faster layout was refused, or None
```

## How it works

A workbook goes through three stages and is then evaluated by one of two
layouts, chosen automatically:

```text
read sheet XML   -> formula cells, one template per formula column
fold aggregates  -> replace SUM(A:A) and friends with the number they compute
build the graph  -> dependency components

layout:  partitioned -> one reused chunk workbook. eval_rows reads that
                        sheet's input rows from the file as the caller asks
                        for them when it can; otherwise the same chunks are
                        evaluated from a preloaded value store. Either way
                        the rows are identical.
         components  -> one workbook per batch of components
```

Neither layout changes a result, and a caller never chooses between them. The
fallback order is:

1. Try the `partitioned` layout (a solid block of row-local formulas).
2. Try the `components` layout (any shape the dependency closure allows).
3. Use the whole-file path.

Workbook-scoped names that resolve to fixed ranges are copied into both
layouts. The component layout also recreates sheet-scoped names, including
shadowed pairs. Array-formula XML no longer forces fallback: the loader
ignores declared array extents, and partitioning follows that.

Only `eval_rows`, and only on the `partitioned` layout, can stream its input
rows. The `components` layout and chunk files whose sheet cannot stream read
values through a preloaded store; the whole-file path reads the evaluated
workbook.

1. **Read topology from the sheet XML.** Stream every `<c>` element to find
   formula cells. Going through the engine's per-cell API instead costs minutes
   on a 45k-formula file, because each call re-renders an AST to text.

2. **Keep one formula source per column, not per cell.** Read a shared formula
   (`<f t="shared" si="0" ref="A2:A40">`) once at its master anchor. For each
   row formula, move the column template to the new row and compare the two
   formulas. If they agree, the cell reuses the template and discards its own
   text. The reader still parses every formula once. On one test file, this
   reduces 45318 formula cells to 716 sources. On another file, it reduces 1256
   cells to 1 source.

3. **Fold whole-column aggregates into numbers.** A formula such as
   `=B2/SUM(B:B)*100` reads a whole column, so its bounding box covers the
   sheet and prevents splitting. The prelude folds `SUM`, `COUNT`, `COUNTA`,
   `MIN`, `MAX`, `SUMIF` and `COUNTIF` over 500-row chunks. It calculates
   `AVERAGE` from `SUM` and `COUNT`. The formula then keeps only its own row.
   The prelude does not change an aggregate such as `MEDIAN`, which needs every
   value at once. See [`src/prelude.rs`](src/prelude.rs).

4. **Group into components.** Union-find over formula-to-formula edges only. A
   data precedent widens a component's bounding box but never merges two
   formulas, so a shared input cell (a header, a lookup table) cannot glue
   unrelated rows together.

5. **Read cell values in the same pass.** Decode values directly from the
   worksheet parts. The evaluation backend caches a whole decoded sheet on first
   access, which makes that path use much more memory. The formula pass also
   collects the values. Inflating and tokenizing a worksheet part causes most of
   the value-store cost. The reader keeps only the shared-string table and one
   flag per cell format.

6. **Evaluate.** Use one reused chunk workbook for row chunks, or use one new
   workbook for each component batch. Both layouts are safe. Every formula
   that a component reads is inside that component. Every other input is a
   copied data cell. A chunk workbook is only as tall as the formula block
   (capped at 500 rows). Packed tables copy through Arrow ingest; holes stay
   unallocated.

7. **Place the formulas in bulk.** Send each component group to the engine in
   one bulk ingest call, not one call per cell.

The Rust module docs contain the implementation contracts:

- [`src/lib.rs`](src/lib.rs) documents whole-file fallback and row trimming.
- [`src/partition.rs`](src/partition.rs) documents the chunk layout's contract,
  streamed inputs and lookup limits.
- [`src/prelude.rs`](src/prelude.rs) documents aggregate folding.
- [`docs/partition-eval.md`](docs/partition-eval.md) documents mini-workbook
  copy and eval rules (Arrow ingest, dates, chunk height).

A private `_benchmark_rows(data, mode="streamed"|"components"|"whole"|"auto")`
entry point exists so `bench/bench_scratch.py` can force one layout at a time.
It is not part of the public contract: a forced mode fails when the file does
not qualify, rather than silently running a different layout. `streamed` is
the chunk layout with inputs read from the file; if that sheet cannot stream,
`auto` evaluates the same chunks from a preloaded store. `rows.strategy`
merges both into `partitioned`; the report's `selected_mode` tells them apart.

## Measurements

Native Rust versus Formualizer whole-file, `min_formulas=0` (every eligible
file is split). Method: [`bench/README.md`](bench/README.md).

| | SSB (826 files) | Sheetpedia (197 files) |
|---|---|---|
| time median | 0.71× | 0.84× |
| time p90 | 1.50× | 1.70× |
| total time | 51 s → 30 s | 2.7 s → 3.4 s |
| whole-file RSS median | 12.4 MiB | 12.7 MiB |

A full-column Sheetpedia file (0913, 149 formulas, 170 KB on disk): **117 MiB → 8.8 MiB** (13×) and 91 ms → 56 ms. An SSB file with a 4.7 M-cell extent (34493): **2.1 s → 0.24 s**.

Values match Formualizer except one pre-existing Sheetpedia sheet-set mismatch (0944).

## Building for AWS Lambda

The `python3.11` runtime is Amazon Linux 2 (glibc 2.26) on x86_64, and the
deployment has no `architecture:` key, so the wheel must be manylinux2014
(glibc 2.17) rather than whatever the build host happens to be:

```
pip install ziglang
maturin build --release --zig --compatibility manylinux2014 \
    --target x86_64-unknown-linux-gnu -i python -o dist
```

That command produces a 5.1 MB wheel and a 14 MB unpacked package. The package
needs only libc, libm, libpthread and libdl at glibc 2.17 or older. It also runs
on Amazon Linux 2023. Check it with
`objdump -T <so> | grep -oE 'GLIBC_[0-9.]+' | sort -uV`.

Amazon Linux 2 rejects a wheel when its bundled ELF library contains misaligned
`PT_LOAD` segments (`ELF load command address/offset not properly aligned`).
This crate's `.so` has four `PT_LOAD` segments. All satisfy
`(vaddr - offset) % align == 0` at `align=0x1000`.


