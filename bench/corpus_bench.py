"""Compare partitioned evaluation against a whole-file run on a corpus.

Both speed paths JSON-encode every row. Each timing pair runs in a fresh process
so an allocation failure cannot abort the corpus. RSS uses a separate process
per mode on the largest files, net of an import-only baseline.

Optional correctness checks compare both partitioned entry points with eval_grid,
not with Excel's cached answers. `--min-formulas` sets the size gate: 0 (the
default) exercises the partitioned path on every splittable file, and 2000 (the
production default) measures what production actually splits.

`--jobs` runs files concurrently. Every timing is measured inside its own
worker process and summed per file, so jobs affects how long the sweep takes,
not what it reports.

Run: python bench/corpus_bench.py DIR [top_n] --jobs N --check --results results.jsonl
"""

import argparse
import collections
import glob
import json
import os
import resource
import statistics
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from contextlib import nullcontext
from io import BytesIO
from zipfile import ZipFile

import formualizer_partitioned


def consume_whole(data):
    grid = formualizer_partitioned.eval_grid(data)
    return sum(len(json.dumps(row, default=str)) for rows in grid.values() for row in rows)


def consume_partitioned(data, min_formulas):
    return sum(len(json.dumps(row, default=str))
               for _, _, row in formualizer_partitioned.eval_rows(data, None, 0.9, min_formulas))


def worker(mode, path, min_formulas=0):
    with open(path, "rb") as fh:
        data = fh.read()
    # The planner can report an unreadable archive as having no formulas.
    # Reject invalid containers rather than count them as ordinary fallbacks.
    with ZipFile(BytesIO(data)) as archive:
        archive.getinfo("xl/workbook.xml")
    plan = formualizer_partitioned.partition_plan(data, None, 0.9, min_formulas)
    result = {
        "status": "ok", "extent": plan["full_extent_cells"],
        "formulas": plan["n_formula_cells"], "batches": plan["n_batches"],
        "chunk_reason": plan.get("chunk_reason"),
    }
    if not plan["partitioned"]:
        return dict(result, status="skipped", reason=plan["fallback_reason"])
    if mode == "speed":
        for name, consume in [("whole", consume_whole),
                              ("partitioned", lambda d: consume_partitioned(d, min_formulas))]:
            started = time.perf_counter()
            consume(data)
            result[name] = time.perf_counter() - started
    elif mode == "check":
        from corpus_diff import compare, display_first
        diffs, first, strategy, timings = compare(data)
        result.update(status="mismatch" if diffs else "ok", differences=diffs,
                      first=display_first(first), strategy=strategy, timings=dict(timings))
    else:
        started = time.perf_counter()
        {"whole": consume_whole,
         "partitioned": lambda d: consume_partitioned(d, min_formulas)}[mode](data)
        result.update(secs=time.perf_counter() - started,
                      peak=resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024)
    return result


def baseline_mb():
    out = subprocess.run(
        [sys.executable, "-c", "import formualizer_partitioned, resource;"
         "print(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss/1024)"],
        capture_output=True, text=True, check=True, timeout=30,
    )
    return float(out.stdout.strip())


def measure(path, mode, timeout=120, memory_mb=4096, min_formulas=0):
    try:
        out = subprocess.run(
            [sys.executable, os.path.abspath(__file__), "--worker", mode, path,
             str(memory_mb), str(min_formulas)],
            capture_output=True, text=True, timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return {"status": "timeout", "reason": f"exceeded {timeout:g} seconds"}
    if out.returncode != 0:
        return {"status": "error", "returncode": out.returncode,
                "reason": out.stderr.strip()[-2000:] or f"worker exit {out.returncode}"}
    try:
        result = json.loads(out.stdout)
        if not isinstance(result, dict) or "status" not in result:
            raise ValueError("missing status")
        return result
    except ValueError as exc:
        return {"status": "error", "reason": f"invalid worker output: {exc}",
                "output": out.stdout[-2000:]}


def pct(values, p):
    values = sorted(values)
    return values[min(len(values) - 1, int(len(values) * p))] if values else 0.0


def foreach(fn, items, jobs):
    if jobs <= 1 or len(items) <= 1:
        for item in items:
            yield item, fn(item)
        return
    with ThreadPoolExecutor(max_workers=min(jobs, len(items))) as pool:
        for item, value in zip(items, pool.map(fn, items)):
            yield item, value


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "--worker":
        memory_mb = int(sys.argv[4])
        resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
        if memory_mb:
            cap = memory_mb * 1024 * 1024
            resource.setrlimit(resource.RLIMIT_AS, (cap, cap))
        print(json.dumps(worker(sys.argv[2], sys.argv[3], int(sys.argv[5])), default=str))
        return 0

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root")
    parser.add_argument("top_n", nargs="?", type=int, default=25)
    parser.add_argument("--check", action="store_true", help="compare with whole-file evaluation")
    parser.add_argument("--results", help="new JSONL file; flushed after every result")
    parser.add_argument("--timeout", type=float, default=120, help="seconds per worker")
    parser.add_argument("--memory-mb", type=int, default=4096, help="worker address-space cap; 0 disables")
    # 0 exercises the partitioned path on every splittable file. Pass
    # DEFAULT_MIN_FORMULAS (2000) to measure what production actually splits.
    parser.add_argument("--min-formulas", type=int, default=0,
                        help="formula count below which a file takes the whole-file path")
    parser.add_argument("--jobs", type=int, default=os.cpu_count() or 1,
                        help="concurrent file workers (default: CPU count); lower if RAM is tight")
    args = parser.parse_args()
    if args.top_n < 0 or args.timeout <= 0 or args.memory_mb < 0 or args.jobs < 1:
        parser.error("top_n and memory-mb must be nonnegative; timeout and jobs must be positive")

    files = sorted(glob.glob(os.path.join(args.root, "**", "*.xlsx"), recursive=True))
    ignored = [f for f in files if "_answer" in os.path.basename(f)]
    files = [f for f in files if "_answer" not in os.path.basename(f)]
    rows = []
    counts = collections.Counter()
    checks = collections.Counter()
    memory_counts = collections.Counter()
    failures = 0
    with open(args.results, "x") if args.results else nullcontext() as report:
        def record(phase, path, result):
            if report is not None:
                report.write(json.dumps({"phase": phase, "path": os.path.relpath(path, args.root),
                                         **result}, default=str) + "\n")
                report.flush()

        for path in ignored:
            record("selection", path, {"status": "skipped", "reason": "_answer filename exclusion"})
        print(f"{len(files)} workbooks under {args.root}; {len(ignored)} excluded by filename", flush=True)
        print(f"Worker limits: {args.timeout:g}s, {args.memory_mb} MiB address space (0 = unlimited), "
              f"{args.jobs} jobs")
        base = baseline_mb()
        print(f"\nMEMORY  peak RSS, interpreter baseline {base:.1f} MiB subtracted")
        print("   extent formulas whole_MiB part_MiB memory_gain whole_s part_s time_ratio file", flush=True)
        ratios = []

        def memory_pair(path):
            measured = {}
            for mode in ("whole", "partitioned"):
                result = measure(path, mode, args.timeout, args.memory_mb, args.min_formulas)
                if result["status"] == "ok":
                    result.update(baseline_mb=base, net_peak_mb=result["peak"] - base)
                measured[mode] = result
            return measured

        memory_files = sorted(files, key=os.path.getsize, reverse=True)[:args.top_n]
        for path, measured in foreach(memory_pair, memory_files, args.jobs):
            for mode, result in measured.items():
                record("memory_" + mode, path, result)
                memory_counts[result["status"]] += 1
                failures += result["status"] in {"error", "timeout"}
            mw, ms = measured["whole"], measured["partitioned"]
            if mw["status"] != "ok" or ms["status"] != "ok":
                continue
            w, s = mw["peak"] - base, ms["peak"] - base
            # Near-baseline peaks do not support a meaningful net-memory ratio.
            gain = w / s if w > 1 and s > 1 else None
            if gain is not None:
                ratios.append(gain)
            gain_text = f"{gain:.2f}x" if gain is not None else "n/a"
            time_ratio = ms["secs"] / mw["secs"]
            print(f"   {mw['extent']:,} {mw['formulas']:,} {w:.1f} {s:.1f} "
                  f"{gain_text} {mw['secs']:.4f} {ms['secs']:.4f} {time_ratio:.2f}x "
                  f"{os.path.relpath(path, args.root)}", flush=True)
        if ratios:
            print(f"   median memory gain {statistics.median(ratios):.2f}x over {len(ratios)} workbooks")
        print(f"   memory worker outcomes: {dict(memory_counts)}", flush=True)

        print("\nSPEED  partitioned / whole (>1 means slower); JSON-encode every row", flush=True)

        def speed_one(path):
            result = measure(path, "speed", args.timeout, args.memory_mb, args.min_formulas)
            if not args.check:
                return result, None
            if result["status"] == "ok":
                check = measure(path, "check", args.timeout, args.memory_mb, args.min_formulas)
            else:
                check = {"status": "skipped", "reason": "speed result: " + result["status"],
                         "detail": result.get("reason")}
            return result, check

        for index, (path, (result, check)) in enumerate(foreach(speed_one, files, args.jobs), 1):
            record("speed", path, result)
            counts[result["status"]] += 1
            failures += result["status"] in {"error", "timeout"}
            if result["status"] == "ok":
                rows.append(result)
            if check is not None:
                record("correctness", path, check)
                checks[check["status"]] += 1
                failures += check["status"] in {"error", "timeout", "mismatch"}
            if index % 100 == 0 or index == len(files):
                line = (f"   {index}/{len(files)} speed={dict(counts)} correctness={dict(checks)}")
                if args.jobs == 1:
                    line += (f" whole={sum(r['whole'] for r in rows):.2f}s "
                             f"partitioned={sum(r['partitioned'] for r in rows):.2f}s")
                print(line, flush=True)

    print(f"\nSPEED SUMMARY  {len(rows)} measured workbooks; outcomes={dict(counts)}")
    if rows:
        speed = [r["partitioned"] / r["whole"] for r in rows if r["whole"] > 0]
        if speed:
            print(f"   median {statistics.median(speed):.2f}x   p10 {pct(speed, .10):.2f}x"
                  f"   p90 {pct(speed, .90):.2f}x   worst {max(speed):.2f}x")
        if args.jobs == 1:
            print(f"   total: whole {sum(r['whole'] for r in rows):.2f}s"
                  f"   partitioned {sum(r['partitioned'] for r in rows):.2f}s")
        else:
            print(f"   total: quote from --jobs 1 (this run used --jobs {args.jobs})")
        big = sorted(rows, key=lambda r: -r["extent"])[:args.top_n]
        big_speed = [r["partitioned"] / r["whole"] for r in big if r["whole"] > 0]
        if big_speed:
            print(f"   {len(big_speed)} largest by extent: median {statistics.median(big_speed):.2f}x")
        print(f"   extent cells: median {pct([r['extent'] for r in rows], .5):,}"
              f"   p90 {pct([r['extent'] for r in rows], .9):,}"
              f"   max {max(r['extent'] for r in rows):,}")
    if args.check:
        print(f"CORRECTNESS  exact comparison with eval_grid (not Excel): {dict(checks)}")
    print(f"Complete. {failures} failed measurements/checks. Results: {args.results or '(not saved)'}", flush=True)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
