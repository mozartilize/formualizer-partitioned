"""Per-file planner reasons for a corpus, without evaluating anything.

A coverage claim needs a before/after map per file, not a headline percentage:
the planner reports the *first* matching fallback reason, so category totals
move when an earlier gate changes and say nothing about what became
partitionable. Run this with the old build and the new one, then diff.

Run from the package root:

    python bench/plan_reasons.py DIR --out before.jsonl
    python bench/plan_reasons.py DIR --out after.jsonl
    python bench/plan_reasons.py --diff before.jsonl after.jsonl
"""

import argparse
import collections
import glob
import json
import os
import sys


def scan(root, out_path, min_formulas):
    import formualizer_partitioned

    files = sorted(glob.glob(os.path.join(root, "**", "*.xlsx"), recursive=True))
    # SpreadsheetBench ships a solved copy beside every workbook. Exclude it, as
    # corpus_bench.py does, so both tools count the same population.
    files = [f for f in files if "_answer" not in os.path.basename(f)]
    with open(out_path, "x") as out:
        for path in files:
            row = {"path": os.path.relpath(path, root)}
            try:
                with open(path, "rb") as fh:
                    # min_formulas=0 by default: report what is safe, not what
                    # is worth it. Pass 2000 to see what production splits.
                    plan = formualizer_partitioned.partition_plan(fh.read(), None, 0.9, min_formulas)
                row.update(partitioned=plan["partitioned"], reason=plan["fallback_reason"],
                           strategy=plan["strategy"], formulas=plan["n_formula_cells"],
                           chunk_reason=plan.get("chunk_reason"),
                           carry_rows=plan.get("carry_rows"),
                           # Tolerate an older build, so a before/after pair can
                           # span the change that added these.
                           xml_formula_cells=plan.get("xml_formula_cells"),
                           parse_errors=plan.get("parse_errors"),
                           first_parse_error=plan.get("first_parse_error"))
            except Exception as exc:
                row["error"] = f"{type(exc).__name__}: {exc}"
            out.write(json.dumps(row, default=str) + "\n")
            out.flush()
    print(f"{len(files)} workbooks -> {out_path}")
    return 0


def diff(before_path, after_path):
    load = lambda p: {r["path"]: r for r in map(json.loads, open(p))}
    before, after = load(before_path), load(after_path)
    shared = [p for p in after if p in before]
    moves = collections.Counter(
        (before[p].get("reason", "error"), after[p].get("reason", "error"))
        for p in shared
        if before[p].get("reason") != after[p].get("reason")
    )
    gained = [p for p in shared if after[p].get("partitioned") and not before[p].get("partitioned")]
    lost = [p for p in shared if before[p].get("partitioned") and not after[p].get("partitioned")]
    print(f"{len(shared)} workbooks in both runs")
    print(f"  partitioned: {sum(bool(before[p].get('partitioned')) for p in shared)}"
          f" -> {sum(bool(after[p].get('partitioned')) for p in shared)}")
    print(f"  newly partitioned {len(gained)}, newly refused {len(lost)}")
    for path in (gained + lost)[:40]:
        print(f"    {'+' if path in gained else '-'} {path}"
              f"  {before[path].get('reason')!r} -> {after[path].get('reason')!r}")
    if len(gained) + len(lost) > 40:
        print(f"    ... {len(gained) + len(lost) - 40} more")
    for (was, now), count in moves.most_common():
        print(f"  {count:>5}  {was!r} -> {now!r}")
    # A newly refused file is only acceptable with a correctness reason; check
    # each one against a whole-file run before claiming the change is neutral.
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("root", nargs="?", help="corpus directory to scan")
    parser.add_argument("--out", help="new JSONL file; flushed after every file")
    parser.add_argument("--diff", nargs=2, metavar=("BEFORE", "AFTER"))
    parser.add_argument("--min-formulas", type=int, default=0,
                        help="formula count below which a file takes the whole-file path")
    args = parser.parse_args()
    if args.diff:
        return diff(*args.diff)
    if not args.root or not args.out:
        parser.error("give a corpus directory and --out, or --diff BEFORE AFTER")
    return scan(args.root, args.out, args.min_formulas)


if __name__ == "__main__":
    sys.exit(main())
