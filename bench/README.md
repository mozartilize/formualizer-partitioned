# Measurements

The scripts in this directory produce the numbers below. Run them from the
package root. The "Testing" section lists the commands.

Measure peak memory in one process per run. RSS is a high-water mark that
never falls inside a process, so one process can report only one measurement.

## Testing

Run these commands from the package root:

```sh
cargo test --no-default-features                # unit tests
bash bench/data/fetch_fed.sh                    # download optional Excel files
python bench/golden_diff.py                     # generated + downloaded files
python bench/corpus_diff.py DIR                 # a corpus of real files
python bench/plan_reasons.py DIR --out before.jsonl   # per-file planner reasons
python bench/plan_reasons.py --diff before.jsonl after.jsonl
python bench/corpus_bench.py DIR --check --results results.jsonl  # memory, speed, correctness
python bench/corpus_bench.py DIR --min-formulas 2000 --results prod.jsonl  # production gate
python bench/corpus_bench.py DIR --jobs 8 --results results.jsonl  # cap concurrency
python bench/bench_scratch.py ROWS              # forced-mode ablation measurements
bash bench/data/fetch_corpora.sh                # download SSB and Sheetpedia
```

The two sweeps pass `min_formulas=0` so that they exercise the partitioned path
rather than the size heuristic that governs production use. Pass
`--min-formulas 2000` to `corpus_bench.py` or `plan_reasons.py` to measure what
production actually splits instead.

`corpus_bench.py` reports the memory and speed trade. Peak memory uses a fresh
process for each mode on the largest 25 files (override with positional `top_n`).
Each timing pair also uses a fresh process. Both timed paths JSON-encode every
row; setup and correctness checks are outside those timings. RSS is a high-water
mark, so the parent never parses workbooks before launching measurements.

Workers default to a 120-second timeout and a 4096 MiB address-space cap. Override
with `--timeout` and `--memory-mb` (0 disables the memory cap). File workers run
concurrently (`--jobs`, default CPU count); lower that if large workbooks compete
for RAM. Every timing is measured inside its own worker process and summed per
file, so `--jobs` changes how long a sweep takes, not what it reports.
Crashes and timeouts are recorded, not allowed to abort the sweep. The `--results` JSONL file must not
already exist and is flushed after every result. Each record includes the full
corpus-relative path, phase, status, measurements or skip/error reason. Memory
records include raw peak RSS, the interpreter baseline, net peak RSS, and seconds.
The log prints cumulative timing and outcome counts every 100 files.

`--check` compares `eval_partitioned` and `eval_rows` exactly with `eval_grid` in
another worker. All three calls share one instant for `NOW` and `TODAY`, so a
file that reads the clock cannot disagree merely because two calls straddled a
second. It records difference counts and the first differing location
and values. This checks partitioning consistency, not correctness against Excel;
last-bit floating-point differences count as mismatches. Non-partitionable files
are skipped with the planner's fallback reason. Correctness checks are also
skipped, with an explicit reason, when timing fails. Files containing `_answer`
in their names are excluded and recorded separately. A completed sweep exits 1
if any measurement fails or any correctness check disagrees; otherwise it exits 0.

`cargo test` needs `--no-default-features` because the default
`extension-module` feature unlinks libpython, which the test harness needs.
maturin enables it through `pyproject.toml`. This does not affect wheel builds.

The tests compare every entry point with `eval_grid`, a whole-file run, cell by
cell. `golden_diff.py` covers generated workbooks and optional fixture files.
The generated workbooks include row-local formulas, cross-sheet references,
chains, `INDIRECT` and mixed value types. `corpus_diff.py` tests a directory of
real workbooks. It reports disagreements and the reasons it did not partition a
file.

## Prelude folding

A test with 100000 source rows and four aggregates compares two prelude
strategies:

| prelude strategy | time | peak |
|---|---:|---:|
| one 100k-row workbook | 12.72 s | 106 MiB |
| fold over a reused 500-row sheet | **0.57 s** | **11 MiB** |

Both strategies give the same cell values. One workbook in the 2729-file corpus
has a last-digit floating-point difference. A folded `SUM` adds chunk totals,
while a whole-column `SUM` adds cells.

## Chunk sheet size

Formula-placement cost grows quickly with chunk-sheet height:

| chunk sheet height | time to place the formulas | peak |
|---:|---:|---:|
| 500 | 0.15 s | 53.8 MiB |
| 2,000 | 0.85 s | 83.3 MiB |
| 5,000 | 3.68 s | 190.7 MiB |
| 20,000 | 41.40 s | 720.1 MiB |

Evaluation time stays near 3.3 seconds for all four heights. This result sets the
chunk height to 500 rows.

On a generated 20000-row running chain, carry-row streaming needs 0.04 seconds
and 23.2 MB. The whole-file path needs 0.16 seconds and 73.7 MB.

## Why a coverage number needs a per-file map

The planner reports the **first** matching fallback reason, so a category total
moves when an earlier gate changes and says nothing about which files became
partitionable. Claim a coverage change only from a per-file before/after map:

```sh
python bench/plan_reasons.py DIR --out before.jsonl   # old build
python bench/plan_reasons.py DIR --out after.jsonl    # new build
python bench/plan_reasons.py --diff before.jsonl after.jsonl
```

The diff names every file that gained or lost the partitioned path. A file that
loses it needs a correctness reason; check that file against a whole-file run
before calling the change neutral.

## Formula-bearing files that do not partition

Measured on the 6,000-file AutoFormula sample with `min_formulas=0`, over the
2,961 files whose planner reason was `ok`, `unparsed formulas`, `names, tables
or 3D references`, or `INDIRECT/OFFSET references`.

**"No formulas" is a real denominator.** Of the 2,664 files reported as having
no formulas, an XML `<f>` count finds formulas in 3. The other 2,661 hold none.

**Every unparsed file is a parser gap, and 54 of 60 are external workbooks.**
`partition_plan` reports `first_parse_error` with sheet, cell, stage, formula
text and parser message. Across the 60:

| count | formula shape | example |
|---:|---|---|
| 36 | external workbook-level name | `=[4]!'NGC2 LAST'` |
| 15 | external reference inside a call | `="As of "&TEXT(New '[3]Summary'!$P$4,"mmmm d, yyyy")` |
| 6 | name-to-name range | `=VLOOKUP(...,C_s1:C_f1,...)` |
| 3 | apostrophe in an external sheet name | `='[1]Stacey's Reconciliation'!F3` |

Worksheet `CDATA` explains none of them; the reader concatenates `CDATA` into
the formula text regardless. Only the 6 name-to-name ranges could partition
without external-reference support, and that needs an upstream parser change.

**A dropped formula cell was answering from its cached value.** `<f/>` elements
the reader cannot resolve (a `t="dataTable"` formula, a shared member whose
master is missing) used to leave the cell out of the graph, so the partitioned
run published the cached `<v>` while the whole-file run recomputed it. One file
in the sample did this and disagreed on 14 cells. Comparing the XML `<f>` count
with the resolved cell count turns that into the `unresolved formula cells`
fallback: 7 files in the sample, 1 of which used to partition and mismatch.

**A constant defined name must stay whole-file work.** The loader keeps only
range definitions, so a whole-file run of `=A1+_Order1` with `_Order1 = 0`
answers `#NAME?`. A mini-workbook that defined the constant would answer `1`.
The reader therefore refuses these files (9 in the sample). `golden_diff.py`
fails when the loader starts resolving constants, which is the point to add
support on both paths.

**A reference the graph cannot resolve is not a partitionable file.** The
`unsupported_refs` count refuses 48 Sheetpedia files that an earlier build
split, because it counted only `named_refs`. Evaluating those 48 confirms the
gate: 47 fail outright with `#REF!: Sheet not found` (44 on the whole-file path
too, so most is an engine limitation, not a partitioning one). Splitting them
was never a working fast path.

**`NOW` and `TODAY` are reproducible, so they do not force a whole-file run.**
The engine clock is pinned once per call (`src/clock.rs`), and every workbook a
call builds reads that one instant, so a batch and a whole-file run report the
same value. `NOW` truncates to whole seconds, so without the pin the two paths
agree only while they stay inside the same second: a race, not a guarantee.
Refusing every Excel-volatile call cost 122 files across SpreadsheetBench and
Sheetpedia while preventing 9 disagreements, and 6 of those 9 were `RAND`. Only
calls that two engines cannot be made to agree on are refused now: `RAND`,
`RANDARRAY`, `RANDBETWEEN` and `INFO`, which reports workbook-level facts such
as the number of open sheets. `INDIRECT` and `OFFSET` keep their own
dynamic-reference gate. Recovered: SpreadsheetBench 673 to 731 files,
Sheetpedia 574 to 625, with 13 files still refused for `RAND`.

Two calls are two runs, so `NOW` may still move by a second between them. A
caller comparing one evaluation with another passes the same instant to both:
`eval_grid(data, now)`, `eval_rows(..., now=now)`. `corpus_diff.py` does this,
which is what makes its comparison deterministic.

**A name pointing at a missing sheet still creates that sheet.** A defined name
such as `'ENRON INV'!$A$1:$H$37` whose sheet the workbook never declares makes
the whole-file loader create it, empty. A partitioned run that reported only
the declared sheets returned a different set of sheets for the same file: 9
files in these two corpora, and the baseline sweep holds 10 more. The reader
now reports those sheets too (`Sources::name_only_sheets`), which fixed all 9.
An external reference (`[1]Sheet1`) invents nothing and is not counted.

**A declared cell with no value is absent from a mini-workbook.** `<c r="G36"
s="26" t="n"/>` holds no value but does exist, and the whole-file loader keeps
it. The value store drops it, so `COUNTIF(G$2:G$37,"")` counts 2 in a
whole-file run and 0 in a batch. One file in these corpora disagrees for this
reason. Storing every valueless declared cell would undo the trim work that
keeps style-only rows out of memory, and refusing every file that pairs a
blank-sensitive call with such a cell would cost 23 files to fix 2, so neither
is implemented. See the plan for the bounded version of the fix.

**Literal `INDIRECT` does not occur.** `INDIRECT("Sheet1!A1")` is statically
resolvable, and rewriting it to `$A$1` would remove the dynamic-reference and
volatile gates. Across 13,458 workbooks (AutoFormula sample, SSB, Sheetpedia),
78 files call `INDIRECT` with a leading quote, and **none** of them passes a
plain literal: every call concatenates (`INDIRECT("A"&ROW())`), which stays
dynamic. The rewrite recovers no file and is not implemented.

## Layout coverage

On the 2729 SpreadsheetBench workbooks with `min_formulas=0`, 27% of the files
split. Another 58% have no formulas. Safety and cost gates send the remaining
15% to the whole-file path.

Of those files, 283 (10.4%) meet the chunk contract, and their input rows
stream. Another 448 (16.4%) use the component layout. Of the chunked files, 54
carry one or more earlier rows. The chunk layout rejects these conditions
(counted over every file, including ones that a later gate refuses anyway):

| count | reason |
|---:|---|
| 381 | a formula reads outside its own row |
| 107 | more than one sheet holds formulas |
| 97 | formulas that read their own position |
| 86 | a formula column holds more than one template |
| 86 | unsupported references |
| 38 | formula columns cover different rows |
| 37 | a formula column has gaps |
| 9 | unreproducible formulas |
| 6 | a formula reads too far above its own row |

A file that misses this contract still partitions if the component layout
accepts it, which is the 16.4% above.

Refresh both with `python bench/plan_reasons.py bench/data/ssb --out x.jsonl`.

## Trim validation

In the tested corpus, style-only cells extend one sheet by 87131 rows and 3973
columns. `trim=True` prevents a database consumer from writing those empty tail
rows.

A 400-workbook corpus test compares streamed rows with backend `range_page`
bounds plus a whole grid. Results match on 376 files and differ on 7 files. In
six of those files, `range_page` loses data. It omits seven rows and four columns
from one sheet, one column from another, and all 209 rows from a third. The
remaining difference is not related to trimming.

Both paths write blank rows inside the reported extent. In that sample, 11.24%
of the rows from the backend path are blank.

## An aggregate-heavy sheet, which is what the chunk layout is for

A generated fixture of 100000 rows holds 20 data columns, 14 row-local formula
columns and 4 whole-column aggregates. That gives 1.8 million formula cells,
which compress to 18 sources. `bench_scratch.py` builds the fixture and measures
it.

The production API always picks the `partitioned` layout for this fixture and
streams its inputs; the row below it ("stored inputs") and the component row
are not choices a caller makes. They are forced through the private
`_benchmark_rows` entry point to isolate what each design decision is worth:
whether an evaluator reuses one workbook or opens one per component batch, and
whether that workbook's inputs stream from the file or come from a preloaded
store.

| | time | peak |
|---|---:|---:|
| partitioned, streamed inputs | **10.80 s** | 869.1 MB |
| partitioned, stored inputs | 12.47 s | 895.0 MB |
| components, with the fold | 26.23 s | 895.4 MB |
| whole file | did not finish | over 4 GB |

The whole-file run cannot allocate inside a 4 GB cap on this fixture. At 20000
rows it does finish, and the four runs are then 2.19 s / 198.7 MB, 2.54 s /
244.8 MB, 5.76 s / 246.7 MB and 66.25 s / 1273.7 MB.

The fold is what makes a partitioned run possible here. Without it the largest
bounding box covers 100% of the sheet, and the file takes the whole-file path.

Streaming the chunk inputs removes the full-sheet store. That is worth 46 MB at
20000 rows and 26 MB at 100000 rows. The saving is smaller than the store
itself. At 100000 rows the Python row objects the consumer builds dominate the
peak, not the store. A consumer that writes each row and drops it still pays for
one row at a time. This fixture measures the evaluation side.

## Ordinary workbooks

The numbers below are the peak RSS of a Python process that JSON-encodes every
row, net of the interpreter's own footprint. Each measurement uses its own
process. Reproduce them by selecting the files a build actually splits, then
measuring only those:

```sh
python bench/plan_reasons.py bench/data/sheetpedia --out prod.jsonl --min-formulas 2000
# symlink the files whose "partitioned" is true into one directory, then
python bench/corpus_bench.py THAT_DIR 23 --min-formulas 2000 --results ord.jsonl
```

The population moves whenever a gate changes, so re-select it before comparing
with an older run. These figures come from the 23 of 2,000 Sheetpedia workbooks
and the 9 of 2,729 SpreadsheetBench workbooks that reach a fast path under the
production defaults. SpreadsheetBench ships a solved `_answer` copy beside
every workbook; both tools exclude it.

Sheetpedia, 23 files:

| | peak | total time |
|---|---:|---:|
| whole file, full grid | 1092.8 MB | 7.08 s |
| partitioned, streamed rows | 956.4 MB | 9.25 s |

That is 0.88x the memory and 1.31x the time. Per file the median is 0.87x
memory and 1.41x time. The largest file holds 46609 formulas in two batches and
runs at 75.5 MB against 191.1 MB whole.

SpreadsheetBench, 9 files:

| | peak | total time |
|---|---:|---:|
| whole file, full grid | 468.3 MB | 3.14 s |
| partitioned, streamed rows | 443.0 MB | 1.81 s |

That is 0.95x the memory and 0.58x the time, and per file the median is 0.57x
memory and 0.43x time. The best file holds 14991 formulas in five batches and
runs at 28.4 MB against 98.6 MB whole.

Three of those nine are copies of one workbook that the chunk layout accepts as
a single 239-row chunk: 2151 formulas over a 6050-cell extent, 27.5 MB whole
against 101.9 MB partitioned. **A one-chunk plan cannot lower the peak**,
because the chunk covers the whole sheet while the run still pays for the value
store and the chunk workbook. No gate rejects it today.

Across the 731 SpreadsheetBench workbooks that partition with `min_formulas=0`,
727 measured, the median time is 1.15x and p90 is 6.40x; 4 workers hit the 4 GB
cap. Tiny files dominate that set: its median extent is 300 cells, and on such
a file the fixed cost of building a topology cannot be repaid. `min_formulas`
exists for exactly that reason; the 76 files of at least 10000 cells run at
1.36x. On the 25 largest, memory is 701.9 MB whole against 345.6 MB
partitioned, a median gain of 1.34x and a best of 3.59x.

## The read stage

The formulas and the data values come out of one pass over the worksheet parts.
A second pass costs almost exactly what the whole value store costs. Reading
every part again with a callback that discards its values measures 14.5 ms
against a 16.1 ms store on one file, and 67.8 ms against 75.6 ms on another.
Inflating and tokenizing is the work. Decoding, packing and sorting the values
is the small remainder. One merged reader keeps the store phase under 6 ms on
every file measured.

The reader collects values before the fallback gates run, so a whole-file
workbook has paid for them. That is cheap for the same reason. The extra cost
over the formula pass alone is the decode and the packing. The evaluator drops
the store before the whole-file run starts.

## The value store

The store holds each sheet as sorted rows of sorted columns, not as a hash map. A hashed
entry costs its key, its value and a control byte at a load factor below one. A
sorted row costs a column number beside the value. On a workbook of 116207 kept
values, the hash layout costs 8.7 MB and the sorted layout costs 3.0 MB.

The layout also sets what a batch pays to copy its inputs in. A map forces the
copy to walk every cell of every range a component reads and to ask the map for
each one, so the copy costs the area of the range. The sorted layout walks only
the values the range holds.

## Placing the formulas

Setting one formula at a time is the largest cost in the component layout, above
evaluation itself. Each call previews graph admission and mutates the dependency
graph on its own, and the cost per call grows with the graph already there. On a
21772-formula workbook, placement takes 7264 ms against 91 ms of evaluation.

The engine's bulk ingest builder takes a whole group at once. Placement on that
file falls to 402 ms. Three rules come out of the measurements:

- **A call must hold whole components.** A split of one dependency chain across
  two calls returns stale values from the split point on, and reports no error.
  A chain of 5000 rows ingested in calls of 2048 gives `A2048 = 1` instead of
  2048. Each component contains all its formula dependencies, so a call can
  safely hold a whole number of components. `probe/splitprobe.rs` reproduces
  this in 30 lines.
- **Place an open-sided range on the formula's own sheet one cell at a time.**
  Bulk ingest treats `A:B` as covering the whole sheet when it builds
  dependencies. `=VLOOKUP(D2,A:B,2,0)` in column E then reports `#CIRC!`
  although nothing in `A:B` reads it back. `probe/circprobe.rs` shows that any
  bounded range is safe. Filling the open sides in with the grid limits also
  works, but it costs. `=LOOKUP(2,1/(J:J<>""),J:J)` then builds a 1048576-row
  array, and one workbook goes from 0.006 s to 4.44 s.
- **Place a component with more than 1 MB of formula text one cell at a time.**
  See `COMPONENT_TEXT_BUDGET` below.

Keep the engine configuration unchanged. The xlsx loader switches its sheet index
to lazy while it ingests, but it feeds values through the same pass. Here the
data cells are already written, and a lazy index reads them as blank. That turns
range results into zero on three SpreadsheetBench files, and reports no error.

## Reference closure soundness

The component layout is only sound when every cell a formula can read is
copied into its batch workbook. Anything unresolvable falls back to the
whole file instead of reading blank. An AutoFormula sweep found each of these
as a live mismatch before the corresponding guard existed:

- **Chained colons merge.** `SUM(I8:K8:M8)` means `SUM(I8:M8)`, but it parses
  as `BinaryOp(":", Range, Cell)`. Collecting the endpoints separately
  dropped `L8` from the closure without a fallback. Chained colons now merge
  to one range; an endpoint that cannot be bounded (a function call, a
  cross-sheet pair, a glued `A1:OFFSET(` name) refuses the file instead.
- **Sheet names match case-insensitively**, as Excel does. A sheet spelling
  the workbook does not hold (phantom `&`-variants, case mismatches) counts
  as unresolvable rather than an empty read.
- **A specified side that shifts off the sheet is `#REF!`**, not row 1. A
  shared-formula member shifted off-grid refuses the file.
- **Volatile formulas stay whole.** Every batch workbook evaluates at a
  different instant, so `NOW()`/`RAND()` disagree even within one run.
- **Workbook-scoped defined names are recreated in each batch workbook**
  (their targets already join the dependency closure). Sheet-scoped names
  keep the whole-file fallback.
- **Batches budget the range union.** A batch copies each distinct range
  once, so a shared lookup table is charged to its first reader only. A
  3358-formula file that packed 668 batches (6.4 s against 0.1 s whole) now
  packs 2 (0.26 s).

## What bulk ingest costs

`finish` clones every staged tree into its batch input while the staged copy is
still alive, so memory holds a component's trees twice. Measurements of the largest
component of three workbooks:

| formulas | formula text | the clone | bulk peak | whole file |
|---:|---:|---:|---:|---:|
| 21764 | 742 KB | 43.1 MB | 164.2 MB | 133.9 MB |
| 35569 | 392 KB | 35.6 MB | 150.8 MB | 128.5 MB |
| 46608 | 1228 KB | 98.9 MB | 293 MB | 189.7 MB |

The first two exceed the whole-file run only by about what the clone costs. The
third exceeds it by more, so it takes the per-cell path and peaks at 75 MB. That
is what `COMPONENT_TEXT_BUDGET` selects between, and 1 MB is the line that
separates the two groups.

The clone is avoidable in principle. The builder's interned path passes an arena
id instead of a tree and never clones, but nothing public turns an `ASTNode`
into an `AstNodeId`, and `add_formula_ids` has no caller in the engine.
Interning would not help this crate anyway, given the sharing it already
exploits. Every cell of a compressed column holds a distinct tree once its
the algorithm shifts their references.

Tests reject two other ways to shrink the staged trees. Dropping
the `source_token` of a shifted node saves 5 MB and changes results, so the
engine reads it. Interning identical subtrees saves nothing, for the same reason
an arena id would not.

## Where the memory goes

Two rounds of measurement each contradicted what looked obvious.

**The evaluation backend's sheet cache, about 56 MB.** The backend decodes a
whole sheet on first access and keeps it. Chunking the reads does not help,
because every read populates the cache. Reading values from the XML directly
removes that cache. It also removes the repeated sheet scans that made the
partitioned path twice as slow, which turns a memory-for-time trade into a
saving on both.

**The parsed formulas, about 4.4 KB each.** The topology looks costly because
two of its fields are vectors of vectors, with 60650 small allocations on the
test file. Measurement says otherwise. Those fields cost 1.4 MB of headers,
while 14991 parsed formulas hold 65 MB, and every other field of the topology
together comes to 2.5 MB. Keeping each formula's references and its source text
instead, and re-parsing per batch, takes that file's topology from 72.5 MB to
9.9 MB, and its whole run from 93.9 MB to 62.3 MB. The first file barely moves,
because shared formulas already collapse it to 1626 parses.

Two things look like savings and are not. Dropping the `source_token` each
parsed node carries frees 7 MB of 72.5 MB. Streaming the worksheet XML instead
of reading it into a string changes the peak not at all, because the peak falls
in the graph-building phase after that string is already freed.

## Choosing the knobs

`budget_cells` is what peak memory tracks. Measurements set its default:

| budget | Over Haul | 14991-formula file |
|---|---|---|
| 131072 | 110.3 MB / 1.38 s | -- |
| 32768 | 75.7 MB / 0.88 s | 127.7 MB / 0.59 s |
| **8192** (default) | **69.7 MB / 0.77 s** | **109.4 MB / 0.57 s** |
| 2048 | 67.7 MB / 0.85 s | 101.0 MB / 0.59 s |
| 512 | 67.1 MB / 1.27 s | -- |

Below 8192 cells, memory keeps improving only slightly, while workbook set-up
starts to show.

`min_formulas` exists because a saving is not automatically worth having. A file
of 132 formulas in 53864 cells does partition and does save memory, but it runs
eleven times slower to do so, on a file that uses only 21 MB.

The table below measures `max_ratio` with template compression and row streaming. A
limit of 0.5 rejects 189 otherwise safe SSB files. The production threshold
makes only six of them relevant. Three files at ratio 0.881 hold 4664 formulas
each, and forcing the component layout makes them 42% faster and reduces net peak
RSS by 54-59%. The other three have a ratio above 5 and gain nothing. A default
of 0.9 admits the useful group and still refuses the second. No
production-relevant SSB file has a useful ratio between 0.9 and 1.0.

`INGEST_CHUNK` (2048 formulas) groups small components into one bulk ingest
call. `COMPONENT_TEXT_BUDGET` (1 MB of formula text) sends a component that is
too large for that path to per-cell placement. Neither is worth tuning per file.
With components as the unit, chunk sizes from 256 to unlimited change time by at
most 2% and memory by at most 5 MB, because a component that exceeds the chunk
cannot be split anyway.

The text budget is the one real dial, and it is a memory-for-time trade on a
small number of files. Over the 9 workbooks whose largest component holds more
than half their formulas, 1 MB measures 1.34x time and 0.88x memory, while
400 KB measures 2.79x time and 0.64x memory. Almost all of that time is one
workbook whose 742 KB component takes 0.64 s in bulk and 7.76 s one at a time.
The default stays at 1 MB. The two workbooks that overshoot at that setting
overshoot by about what the ingest clone costs, so removal of the clone upstream
would put them under the whole-file run without a loss of speed.

Tests reject a gate on the largest component's share of the formulas. The gate
looks right on three files and fails on the corpus. The workbooks
whose largest component holds over 90% of their formulas net 0.85x memory as a
group. Their losses are small in absolute terms, at 30 MB and 22 MB. Their wins
are not, at 117 MB on one file.

## Names and array formulas

The measurements in this section test component-layout support only.

In the 2729-workbook corpus, array formulas cause 6.1% of the fallbacks. Defined
names cause another 4.3%. The array group has a median of 22 formulas, and the
names group has a median of 50. Only 9 files in each group hold 2000 formulas or
more. Those files are three workbooks in three variants each.

A component-size check rejects all the large candidates. Their largest component
covers 77%, 90%, 99% or 99% of the sheet. Component-layout support alone would
enable no files.

A test with `max_ratio=1.0` gives these results:

| file | whole | forced split |
|---|---|---|
| 13165 formulas, 66640 cells | 68.5 MB | 70.0 MB |
| 8166 formulas, 68816 cells | 70.2 MB | 72.9 MB |
| 38050 formulas, 40352 cells | 541.9 MB | 634.5 MB |

Splitting a workbook with one connected formula mass costs memory. Every batch
must copy the same shared inputs. Two workbooks that do partition provide a
contrast. Their memory use falls from 109.7 MB to 59.9 MB and from 35.2 MB to
28.7 MB.

Each Federal Reserve workbook declares more than 300 names but holds only 30
formulas. `min_formulas` sends each file to the whole-file path.

### Fixed-address defined names on the chunk layout

The chunk layout accepts a workbook-scoped name that targets a fixed, fully
absolute range on another sheet.

In the same 2729-workbook corpus, 12 files use such a name. None of them becomes
partitionable, because each one holds another blocker: unsupported references,
formulas that read their own position, or volatile formulas. The combined
`names, tables or 3D references` group falls from 116 files to 113.

The support gives no gain on this corpus. A customer file that uses only fixed
names and meets the rest of the chunk contract does gain. Check the
`partition_plan` fields `named_refs`, `fallback_reason` and `chunk_reason` on
customer files.
