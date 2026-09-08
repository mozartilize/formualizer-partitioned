# Benchmarks

Native Rust timings and RSS versus Formualizer whole-file evaluation.
Not Python API numbers. Run every command from the package root.

Headline figures live in the crate [`README.md`](../README.md). This file is
how to reproduce them.

## Reproduce

```sh
bash bench/data/fetch_corpora.sh
cargo test --no-default-features
cargo build --release --locked --manifest-path bench/Cargo.toml

# Comparable totals: one worker. `--results` must not already exist.
bench/target/release/corpus_bench bench/data/ssb --check --jobs 1 \
    --results /tmp/ssb.jsonl
# Headline Sheetpedia numbers used the 1000-file sample, not the full dump.
bench/target/release/corpus_bench bench/data/sheetpedia/sample_1000 --check --jobs 1 \
    --results /tmp/sheetpedia.jsonl
```

`corpus_bench` walks `DIR` for `*.xlsx`. Files with `_answer` in the name are
skipped. A fresh `fetch_corpora.sh` unpacks SSB under `bench/data/ssb` and the
full Sheetpedia tree under `bench/data/sheetpedia`; walking the latter is a
different (much larger) run than `sample_1000`. Default `min_formulas=0` splits every eligible file (worst case).
Pass `--min-formulas 2000` to match the library default.

| flag | default | meaning |
|---|---|---|
| `top_n` (positional) | 25 | largest files for separate-process memory |
| `--timeout` | 120 s | per-file worker deadline |
| `--memory-mb` | 4096 | address-space cap (`0` disables) |
| `--jobs` | CPU count | concurrent file workers |
| `--check` | off | compare values to Formualizer whole-file |

`corpus_bench` is its own crate. It compiles `src/` modules directly, without
libpython. `bench/corpus_bench.py` is only a `cargo run --release` launcher.

Exit `0` only if every measured file timed and (with `--check`) matched.
Crashes and timeouts are recorded, not allowed to abort the sweep.

```sh
jq -c 'select(.phase=="correctness" and .status=="mismatch")' results.jsonl
jq -c 'select(.stage) | {path, phase, stage, reason}' results.jsonl
jq -s 'map(select(.phase=="speed" and .status=="ok"))
       | sort_by(-.time_ratio)[:10]
       | map({path, time_ratio, whole, partitioned})' results.jsonl
```

## Gotchas

- **`--jobs` > 1 contends CPU and RAM.** Ratios stay usable; wall-clock totals
  and p90 do not. Use `--jobs 1` before publishing numbers.
- **`--results FILE` refuses to overwrite.** Delete or pick a new path.
- **`--locked` is required for a comparable pin.** The Formualizer git rev is
  in `bench/Cargo.toml`. A lockfile drift is a different engine.
- **Do not compare to old Python benches.** Native JSON encode, no FFI, no
  Python objects.
- **RSS is a process high-water mark.** It never falls. Memory uses a fresh
  process per mode on the largest `top_n` files. Speed-worker
  `whole_peak_mb` is the isolated whole peak (whole runs first);
  `partitioned_peak_mb` is the combined ceiling after that, not an exact
  partitioned peak.
- **Speed timings exclude zip I/O and eligibility**, not evaluator setup,
  eval, JSON encode, or disposal. Both sides encode every row.
- **`min_formulas=0` is not production.** The library default is 2000. Small
  files look worse on the sweep than they do in `eval_rows`.
- **Excel formula caches are evidence only.** They never change the exit code.
  Stale or foreign `calcId` / `fullCalcOnLoad` caches are normal.
- **`NOW` / `TODAY` share one instant** across whole and partitioned, so a
  clock formula cannot mismatch just because two calls straddled a second.
- **A whole-file SIGABRT** (address-space cap, usually a bogus full-width
  `dimension`) kills that worker. The sweep may still report a partitioned
  timing; the lost whole baseline still counts as a failed measurement.
- **Expected corpus noise, not regressions:** four SSB whole-file `#SPILL!`
  (`BlockedByFormula`) files (10452/3, 55049/1–3); one Sheetpedia sheet-set
  mismatch (0944). A folded whole-column aggregate can differ in the last
  floating-point digit from a cell-wise `SUM`.
- **`cargo test` needs `--no-default-features`.** Default `extension-module`
  unlinks libpython. Maturin still builds the wheel with it.

## Other

```sh
bash bench/data/fetch_fed.sh                 # optional Fed fixtures
python bench/golden_diff.py                  # generated + downloaded files
python bench/corpus_diff.py DIR              # Python-path corpus check
python bench/plan_reasons.py DIR --out x.jsonl
python bench/plan_reasons.py --diff before.jsonl after.jsonl
python bench/bench_scratch.py ROWS           # forced-layout ablation
```
