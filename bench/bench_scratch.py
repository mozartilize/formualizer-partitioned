"""Measure the chunk layout against the component layout on a generated fixture.

The production API (`eval_rows`/`eval_partitioned`) picks between them
automatically and does not expose the choice. This script forces each one
through the private `_benchmark_rows` entry point (not part of the public
contract) to reproduce the ablation measurements that justified the design:
chunk height, the streaming input saving, and the component-batch cost.

The fixture has the shape the chunk layout is built for and that the design was
measured against: a block of row-local formulas over a block of data columns,
plus a few whole-column aggregates that the prelude must fold away first.

Peak memory is measured in one process per run. Resident memory is a high-water
mark that never falls inside a process, so two runs in one process would report
the first run's peak for both.

Run from the package root: python bench/bench_scratch.py [rows]
"""

import json
import os
import subprocess
import sys
import time

PACKAGE_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FIXTURE_DIR = os.path.join(PACKAGE_ROOT, "bench", "data", "bench")

# 20 data columns, then row-local formulas, then four folded aggregates. The
# formulas repeat one shape per column, which is what template compression and
# the scratch layout both need.
ROW_LOCAL = [
    "=A{r}+B{r}",
    "=C{r}-D{r}",
    "=E{r}*F{r}",
    "=IFERROR(G{r}/H{r},0)",
    "=I{r}^2",
    "=IF(A{r}>5000,B{r},C{r})",
    '=IFS(A{r}>7500,"P",A{r}>5000,"G",TRUE,"B")',
    "=AND(A{r}>0,B{r}>0)",
    "=LEFT(L{r},3)",
    "=UPPER(M{r})",
    "=ROUND(E{r},2)",
    "=MOD(F{r},G{r})",
    "=ISNUMBER(A{r})",
    "=TEXT(E{r},\"0.00\")",
]
AGGREGATES = [
    "=B{r}/SUM(B:B)*100",
    "=C{r}-AVERAGE(C:C)",
    '=A{r}+SUMIF(R:R,"EMEA",A:A)',
    '=A{r}/COUNTIF(A:A,">500")',
]


def build(rows):
    """Write the fixture once and reuse it, because writing it is the slow part."""
    os.makedirs(FIXTURE_DIR, exist_ok=True)
    path = os.path.join(FIXTURE_DIR, f"scratch_{rows}.xlsx")
    if os.path.exists(path):
        return path

    import openpyxl

    wb = openpyxl.Workbook(write_only=True)
    ws = wb.create_sheet("Main")
    regions = ["EMEA", "APAC", "AMER", "LATAM"]
    for r in range(1, rows + 1):
        row = []
        for c in range(1, 21):
            if c == 12:
                row.append(f"code{r % 997}")
            elif c == 13:
                row.append(f"name{r % 331}")
            elif c == 18:
                row.append(regions[r % 4])
            else:
                row.append(1000.0 + ((r * 7 + c * 13) % 8000))
        for tpl in ROW_LOCAL + AGGREGATES:
            row.append(tpl.format(r=r))
        ws.append(row)
    wb.save(path)
    return path


CHILD = r"""
import json, os, resource, sys, time
import formualizer_partitioned

path, mode = sys.argv[1], sys.argv[2]
data = open(path, "rb").read()

t0 = time.time()
if mode == "plan":
    out = formualizer_partitioned.partition_plan(data, None, 0.5, 0)
    rows = 0
else:
    rows = 0
    if mode == "whole":
        rows_iter, out = formualizer_partitioned._benchmark_rows(
            data, "whole", None, 0.5, 0, False, True
        )
    else:
        # mode is "streamed", "scratch", "components" or "auto"; the prelude
        # runs for all four, so the only difference measured is how the rows
        # are evaluated and where their input values are held.
        rows_iter, out = formualizer_partitioned._benchmark_rows(
            data, mode, None, 0.5, 0, False, True
        )
    for _sheet, _rn, _row in rows_iter:
        rows += 1
elapsed = time.time() - t0
peak_mb = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024
print(json.dumps({"sec": elapsed, "peak_mb": peak_mb, "rows": rows, "plan": out}))
"""


def run(path, mode):
    proc = subprocess.run(
        [sys.executable, "-c", CHILD, path, mode],
        capture_output=True,
        text=True,
        cwd=PACKAGE_ROOT,
    )
    if proc.returncode != 0:
        return {"error": proc.stderr.strip().splitlines()[-1][:120]}
    return json.loads(proc.stdout)


def main():
    rows = int(sys.argv[1]) if len(sys.argv) > 1 else 20_000
    t0 = time.time()
    path = build(rows)
    size_mb = os.path.getsize(path) / 1024 / 1024
    print(f"fixture {os.path.basename(path)}  {size_mb:.1f} MB  built in {time.time() - t0:.1f}s")

    plan = run(path, "plan").get("plan", {})
    print(
        f"  formula cells {plan.get('n_formula_cells')}, "
        f"sources {plan.get('n_formula_sources')}, "
        f"folded {plan.get('prelude_folded')}, "
        f"strategy {plan.get('strategy')} ({plan.get('chunk_reason')})"
    )

    print(f"\n  {'run':<24} {'selected':>12} {'time':>9} {'peak':>10} {'rows':>9}")
    for mode, label in [
        ("auto", "auto (production)"),
        ("streamed", "chunks, streamed inputs"),
        ("scratch", "chunks, stored inputs"),
        ("components", "components + prelude"),
        ("whole", "whole file"),
    ]:
        got = run(path, mode)
        if "error" in got:
            print(f"  {label:<24} {'FAILED':>12}  {got['error']}")
            continue
        selected = got["plan"].get("selected_mode", "?")
        print(
            f"  {label:<24} {selected:>12} {got['sec']:>8.2f}s {got['peak_mb']:>9.1f}M {got['rows']:>9}"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
