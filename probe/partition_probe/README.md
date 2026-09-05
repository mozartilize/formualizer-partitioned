# partition_probe — mini-workbook cost probe

Measures what the `formualizer-partitioned` evaluation strategy costs before
any of it is implemented. It complements `probe/probe.py`, which
decides whether a *file* can be partitioned; this probe decides how a partitioned
file should be *evaluated*.

The probe generates synthetic data in the shape of the xlstream benchmark
(20 data columns, 28 row-local formula columns, optionally 2 lookup columns),
with whole-column aggregates already replaced by the literals a prelude pass
would substitute. Nothing is read from disk, so the numbers isolate engine cost
from XML reading and dependency-graph construction.

## Running

```bash
cd probe/partition_probe
cargo build --release

BIN=target/release/xlsx-partition-probe

# one ephemeral workbook per batch (today's partition::evaluate shape)
/usr/bin/time -v $BIN batched 20000 500 1 1

# one reused scratch workbook, rows rebased to 1..N (proposed shape)
/usr/bin/time -v $BIN scratch 20000 500 1

# aggregate prelude, whole-height versus chunked fold
/usr/bin/time -v $BIN prelude 100000
/usr/bin/time -v $BIN prelude_chunked 100000 500

# lookup cost versus lookup-table height
LOOKUP_ROWS=20000 /usr/bin/time -v $BIN scratch 5000 500 1
```

Subcommands:

| Command | Meaning |
|---|---|
| `batched <rows> <batch_rows> <rebase 0\|1> <lookup 0\|1\|2>` | new mini-workbook per batch |
| `scratch <rows> <batch_rows> <lookup 0\|1\|2>` | one reused workbook, chunk values overwritten |
| `prelude <rows>` | aggregates over one full-height workbook |
| `prelude_chunked <rows> <chunk_rows>` | additive fold over a reused compact sheet |

`lookup`: `0` none, `1` whole-column lookup ranges, `2` bounded lookup ranges.
`LOOKUP_ROWS` (default `1000`) sets lookup-table height.

`batched` and `scratch` print a checksum over all numeric results. Any change to
batching, rebasing or reuse must keep that checksum identical for the same
parameters; a differing checksum means the strategy changed semantics.

## Baseline measurements

Recorded 2026-09-04 on i7-13650HX, WSL2, 9.7 GiB RAM, release build,
formualizer 0.8.4, single-threaded. Peak RSS from `/usr/bin/time -v`.

### Strategy comparison, 100,000 rows / 3,000,000 formula cells

| Command | Time | Peak RSS |
|---|---:|---:|
| `scratch 100000 500 0` | 4.39s | 28 MiB |
| `scratch 100000 500 1` | 20.10s | 109 MiB |
| `batched 100000 500 1 1` | 54.77s | 68 MiB |

For reference, `cilladev/xlstream` evaluates the equivalent medium fixture in
15.90s (1 worker) / 13.05s (4 workers) at ~615 MiB.

### Rebasing and reuse, 20,000 rows

| Command | Time | Peak RSS |
|---|---:|---:|
| `batched 20000 500 0 0` (original row numbers) | 9.28s | 54.7 MiB |
| `batched 20000 500 1 0` (rebased) | 6.01s | 55.7 MiB |
| `scratch 20000 500 0` | 0.99s | 26.4 MiB |

### `set_cell_formula` is superlinear in sheet extent

`scratch 20000 <batch_rows> 2`, formula-build phase only:

| Scratch rows | Build | Peak RSS |
|---:|---:|---:|
| 500 | 0.15s | 53.8 MiB |
| 2,000 | 0.85s | 83.3 MiB |
| 5,000 | 3.68s | 190.7 MiB |
| 20,000 | 41.40s | 720.1 MiB |

Evaluation stayed ~3.3s in all four runs. The cost is graph ingestion, not
calculation, so a mini-workbook must stay short regardless of source height.

### Prelude, 100,000 rows

| Command | Time | Peak RSS |
|---|---:|---:|
| `prelude 100000` | 12.72s | 106 MiB |
| `prelude_chunked 100000 500` | 0.57s | 11 MiB |
| `prelude_chunked 100000 2000` | 0.59s | 13.9 MiB |

Both produce identical aggregates (`SUM` 498,822,000, `AVERAGE` 4,988.34,
`SUMIF` 124,720,000, `COUNTIF` 100,000).

### Lookup cost is linear in table height

`scratch 5000 500 1`, i.e. 10,000 lookup calls:

| `LOOKUP_ROWS` | Eval | Peak RSS |
|---:|---:|---:|
| 1,000 | 0.87s | 39.6 MiB |
| 5,000 | 3.58s | 82.7 MiB |
| 20,000 | 15.48s | 248.0 MiB |

Per-call cost did not change with batch size (evaluation stayed ~3.3s whether
the batch was 500 or 20,000 rows) and did not change between whole-column and
bounded ranges. There is no reusable lookup index on this path, so a fast path
must budget `lookup_calls * lookup_table_rows` and reject files above the budget.

## How these numbers are used

Re-run the probe after upgrading formualizer: if `set_cell_formula` stops
being superlinear, or lookups gain an index, revisit the chunk-height and
lookup-budget decisions.
