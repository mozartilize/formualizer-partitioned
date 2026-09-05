#!/usr/bin/env python3
"""Component-distribution probe for xlsx dependency graphs.

Decides whether the "partition into per-component mini-workbooks" approach
(path A) can lower peak memory versus evaluating the whole sheet at once.

It does NOT evaluate formulas. It streams the sheet XML, extracts each
formula's precedents with a probe-grade regex, groups cells with union-find,
and reports the component size distribution — in particular the largest
component's bounding box relative to the full used extent.

Decision rule:
  biggest-component extent  ~=  full extent  -> a cross-sheet/cross-row
    aggregate spans the sheet; partitioning cannot beat whole-file eval on
    this file (fallback path dominates).
  biggest-component extent  <<  full extent  -> most work is in small
    components; partitioning lowers peak proportionally.

Peak-memory model comes from the formualizer eval facts: peak ~= extent-cells
(rows x cols) x ~433 B, paid once per evaluate_all call.

Usage:
  python probe/probe.py FILE.xlsx [FILE2.xlsx ...]
  python probe/probe.py --gen sample.xlsx      # write a mixed-shape sample, then probe it

Precedent extraction here is deliberately approximate and OVER-merges
(a range collapses to its corner cells + a bbox expansion), which can only
overestimate the largest component. That is the safe direction for a
go / no-go decision: it never makes partitioning look better than it is.
"""

import re
import sys
import zipfile

from lxml import etree

BYTES_PER_EXTENT_CELL = 433  # formualizer eval peak ~= rows*cols * this

_NS = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
_R = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"


def col_to_num(letters):
    n = 0
    for ch in letters:
        n = n * 26 + (ord(ch) - 64)
    return n


_CELL_RE = re.compile(r"^([A-Z]{1,3})([0-9]+)$")
_FLAG_RE = re.compile(r"^(\$?)([A-Z]{1,3})(\$?)([0-9]+)$")


def parse_ref(ref):
    """'A1' / '$A$1' -> (row, col). Returns None if not a plain cell ref."""
    m = _CELL_RE.match(ref.replace("$", ""))
    if not m:
        return None
    return int(m.group(2)), col_to_num(m.group(1))


def parse_ref_flags(tok):
    """'$A$1' -> (row, col, col_abs, row_abs). None if not a cell ref."""
    m = _FLAG_RE.match(tok)
    if not m:
        return None
    return int(m.group(4)), col_to_num(m.group(2)), bool(m.group(1)), bool(m.group(3))


# A ref token, optionally sheet-qualified and/or a range. Probe-grade.
_TOKEN_RE = re.compile(
    r"""
    (?:(?P<sheet>'[^']+'|[A-Za-z_][A-Za-z0-9_.]*)\!)?   # optional Sheet!
    (?P<a>\$?[A-Z]{1,3}\$?[0-9]+)                        # first cell
    (?::(?P<b>\$?[A-Z]{1,3}\$?[0-9]+))?                  # optional :second
    """,
    re.VERBOSE,
)
_STRING_RE = re.compile(r'"(?:[^"]|"")*"')
_FUNC_HIST = {}


def extract_refs(formula, cur_sheet):
    """Parse a formula into structured refs that carry $-absolute flags, so a
    shared-formula master can be re-resolved per member cell by offset.

    Returns (refs, funcs). Each ref is:
      ('cell',  sheet, row, col, col_abs, row_abs)
      ('range', sheet, r0, c0, ra0, ca0, r1, c1, ra1, ca1)
    """
    funcs = set(
        m.group(1).upper()
        for m in re.finditer(r"([A-Za-z_][A-Za-z0-9_.]*)\s*\(", formula)
    )
    for f in funcs:
        _FUNC_HIST[f] = _FUNC_HIST.get(f, 0) + 1
    body = _STRING_RE.sub("", formula)  # drop string literals
    refs = []
    for m in _TOKEN_RE.finditer(body):
        end = m.end()
        if end < len(body) and body[end] == "(":
            continue  # function name, not a ref
        sheet = m.group("sheet")
        sheet = sheet.strip("'") if sheet else cur_sheet
        a = parse_ref_flags(m.group("a"))
        if a is None:
            continue
        b_raw = m.group("b")
        if b_raw is None:
            refs.append(("cell", sheet, a[0], a[1], a[2], a[3]))
        else:
            b = parse_ref_flags(b_raw)
            if b is None:
                refs.append(("cell", sheet, a[0], a[1], a[2], a[3]))
                continue
            refs.append(("range", sheet, a[0], a[1], a[2], a[3],
                         b[0], b[1], b[2], b[3]))
    return refs, funcs


def resolve_refs(refs, dr, dc):
    """Apply a member offset (dr, dc) to structured refs (relative parts only).

    Returns (singletons, ranges):
      singletons: [(sheet, row, col)]
      ranges:     [(sheet, r0, c0, r1, c1)] (normalized corners)
    """
    singletons = []
    ranges = []
    for ref in refs:
        if ref[0] == "cell":
            _, sheet, row, col, ca, ra = ref
            r = row if ra else row + dr
            c = col if ca else col + dc
            singletons.append((sheet, r, c))
        else:
            _, sheet, r0, c0, ca0, ra0, r1, c1, ca1, ra1 = ref
            rr0 = r0 if ra0 else r0 + dr
            cc0 = c0 if ca0 else c0 + dc
            rr1 = r1 if ra1 else r1 + dr
            cc1 = c1 if ca1 else c1 + dc
            lo_r, hi_r = sorted((rr0, rr1))
            lo_c, hi_c = sorted((cc0, cc1))
            ranges.append((sheet, lo_r, lo_c, hi_r, hi_c))
    return singletons, ranges


class UnionFind:
    def __init__(self):
        self.parent = {}

    def find(self, x):
        p = self.parent.setdefault(x, x)
        while p != x:
            self.parent[x] = self.parent.setdefault(p, p)
            x, p = p, self.parent[p]
        return x

    def union(self, a, b):
        ra, rb = self.find(a), self.find(b)
        if ra != rb:
            self.parent[ra] = rb


def workbook_sheets(zf):
    """Return [(sheet_name, worksheet_xml_path)] in document order."""
    wb = etree.fromstring(zf.read("xl/workbook.xml"))
    rels = etree.fromstring(zf.read("xl/_rels/workbook.xml.rels"))
    rid_to_target = {
        r.get("Id"): r.get("Target")
        for r in rels.iter(f"{{{'http://schemas.openxmlformats.org/package/2006/relationships'}}}Relationship")
    }
    out = []
    for s in wb.iter(f"{{{_NS}}}sheet"):
        name = s.get("name")
        rid = s.get(f"{{{_R}}}id")
        target = rid_to_target.get(rid, "")
        if not target.startswith("/"):
            target = "xl/" + target.lstrip("./")
        else:
            target = target.lstrip("/")
        out.append((name, target))
    return out


def probe_file(path):
    zf = zipfile.ZipFile(path)
    sheets = workbook_sheets(zf)

    uf = UnionFind()
    # per-component bbox accumulators keyed by union-find root, filled after
    comp_cells = {}          # node -> set membership captured via nodes list
    nodes = set()            # all (sheet,row,col) that are formula cells or precedents
    formula_nodes = set()
    # bbox expansions requested by ranges: root-agnostic, applied post-union
    range_expands = []       # (formula_node, sheet, r0, c0, r1, c1)
    full_extent = {}         # sheet -> (max_row, max_col)
    cross_sheet = False
    cross_row = False

    shared_masters = {}      # si -> (master_row, master_col, refs)

    def wire(fnode, refs, dr, dc):
        nonlocal cross_sheet, cross_row
        nodes.add(fnode)
        formula_nodes.add(fnode)
        singles, rngs = resolve_refs(refs, dr, dc)
        frow = fnode[1]
        for s, r, col in singles:
            if s != fnode[0]:
                cross_sheet = True
            if r != frow:
                cross_row = True  # ref to another row (chain / running total)
            pnode = (s, r, col)
            nodes.add(pnode)
            uf.union(fnode, pnode)
        for s, r0, c0, r1, c1 in rngs:
            if s != fnode[0]:
                cross_sheet = True
            if r1 > r0 or r0 != frow:
                cross_row = True
            for corner in ((s, r0, c0), (s, r1, c1)):
                nodes.add(corner)
                uf.union(fnode, corner)
            range_expands.append((fnode, s, r0, c0, r1, c1))

    for name, target in sheets:
        try:
            data = zf.read(target)
        except KeyError:
            continue
        max_row = max_col = 0
        members = []         # (row, col, si) deferred until masters known
        shared_masters.clear()
        for _, c in etree.iterparse(_bio(data), tag=f"{{{_NS}}}c"):
            ref = c.get("r")
            rc = parse_ref(ref) if ref else None
            if rc:
                if rc[0] > max_row:
                    max_row = rc[0]
                if rc[1] > max_col:
                    max_col = rc[1]
            f_el = c.find(f"{{{_NS}}}f")
            if f_el is not None and rc:
                si = f_el.get("si")
                shared = f_el.get("t") == "shared"
                if f_el.text:
                    refs, _ = extract_refs(f_el.text, name)
                    if shared and si is not None:
                        shared_masters[si] = (rc[0], rc[1], refs)
                    wire((name, rc[0], rc[1]), refs, 0, 0)
                elif shared and si is not None:
                    members.append((rc[0], rc[1], si))
            c.clear()
        # resolve shared-formula members by relative offset from their master
        for row, col, si in members:
            master = shared_masters.get(si)
            if master is None:
                continue
            mrow, mcol, refs = master
            wire((name, row, col), refs, row - mrow, col - mcol)
        full_extent[name] = (max_row, max_col)

    # component bbox: min/max row/col per root, per sheet touched
    # bbox is measured in a single (dominant) sheet's coordinates per component;
    # for cross-sheet components we sum extent across sheets touched.
    from collections import defaultdict

    comp_bounds = defaultdict(lambda: {})  # root -> sheet -> [rmin,cmin,rmax,cmax]

    def touch(root, sheet, r, col):
        d = comp_bounds[root]
        b = d.get(sheet)
        if b is None:
            d[sheet] = [r, col, r, col]
        else:
            if r < b[0]:
                b[0] = r
            if col < b[1]:
                b[1] = col
            if r > b[2]:
                b[2] = r
            if col > b[3]:
                b[3] = col

    for node in nodes:
        root = uf.find(node)
        touch(root, node[0], node[1], node[2])
    for fnode, s, r0, c0, r1, c1 in range_expands:
        root = uf.find(fnode)
        touch(root, s, r0, c0)
        touch(root, s, r1, c1)

    # component extent-cells = sum over sheets of bbox rows*cols
    comps = []
    for root, per_sheet in comp_bounds.items():
        extent = 0
        span_rows = 0
        span_cols = 0
        for b in per_sheet.values():
            rows = b[2] - b[0] + 1
            cols = b[3] - b[1] + 1
            extent += rows * cols
            span_rows = max(span_rows, rows)
            span_cols = max(span_cols, cols)
        comps.append((extent, span_rows, span_cols, len(per_sheet)))

    comps.sort(reverse=True)
    full_cells = sum(r * c for r, c in full_extent.values())

    _report(path, sheets, full_extent, full_cells, formula_nodes, comps,
             cross_sheet, cross_row)


def _bio(data):
    import io
    return io.BytesIO(data)


def _mb(cells):
    return cells * BYTES_PER_EXTENT_CELL / 1e6


def _report(path, sheets, full_extent, full_cells, formula_nodes, comps,
            cross_sheet, cross_row):
    print("=" * 70)
    print(f"FILE: {path}")
    print(f"sheets: {len(sheets)}   formula cells: {len(formula_nodes)}")
    for name, (r, c) in full_extent.items():
        print(f"  [{name}] extent {r} x {c} = {r*c:,} cells")
    print(f"full used extent (all sheets): {full_cells:,} cells "
          f"(~{_mb(full_cells):.0f} MB whole-file eval)")
    print(f"components: {len(comps)}   cross-sheet: {cross_sheet}   "
          f"cross-row dep: {cross_row}")
    if not comps:
        print("no formula components. eval self-gates to ~0; probe moot.")
        return
    biggest = comps[0]
    biggest_cells = biggest[0]
    pct = 100.0 * biggest_cells / full_cells if full_cells else 0.0
    print("\ntop components by bbox extent-cells:")
    print(f"  {'rank':>4} {'extent-cells':>14} {'rows':>7} {'cols':>6} "
          f"{'sheets':>7} {'~MB':>7} {'%full':>7}")
    for i, (ext, rows, cols, ns) in enumerate(comps[:8], 1):
        print(f"  {i:>4} {ext:>14,} {rows:>7} {cols:>6} {ns:>7} "
              f"{_mb(ext):>7.1f} {100.0*ext/full_cells:>6.1f}%")

    # size histogram
    buckets = [(1, "1"), (10, "2-10"), (100, "11-100"),
               (1000, "101-1k"), (10000, "1k-10k"), (float("inf"), ">10k")]
    hist = {label: 0 for _, label in buckets}
    for ext, *_ in comps:
        for hi, label in buckets:
            if ext <= hi:
                hist[label] += 1
                break
    print("\ncomponent extent-cell histogram:")
    for _, label in buckets:
        print(f"  {label:>8}: {hist[label]}")

    print("\ntop functions used:")
    for f, n in sorted(_FUNC_HIST.items(), key=lambda kv: -kv[1])[:15]:
        print(f"  {f:<14} {n}")

    print("\nDECISION")
    print(f"  biggest component = {pct:.1f}% of full extent "
          f"(~{_mb(biggest_cells):.0f} MB vs ~{_mb(full_cells):.0f} MB whole-file)")
    if pct >= 60:
        print("  -> giant component spans the sheet. Partitioning canNOT beat "
              "whole-file eval here; fallback dominates. Path A adds cost with "
              "little RAM win on this shape.")
    elif pct >= 25:
        print("  -> partial win. Partitioning lowers peak but a sizable "
              "component remains. Worth it only if these files are frequent.")
    else:
        print("  -> partitioning wins big: peak drops to a fraction of "
              "whole-file. Path A pays off on this shape.")


def gen_sample(path):
    """Write a mixed-shape sample: many row-local formulas + one column
    aggregate (cross-row) + a couple cross-cell chains."""
    from openpyxl import Workbook

    wb = Workbook()
    ws = wb.active
    ws.title = "Data"
    ws.append(["a", "b", "b_plus_10", "running", "grand_total"])
    n = 2000
    for i in range(2, n + 2):
        ws.cell(i, 1, i)                       # a: data
        ws.cell(i, 2, i * 2)                   # b: data
        ws.cell(i, 3, f"=B{i}+10")             # row-local
        if i == 2:
            ws.cell(i, 4, f"=C{i}")            # running total start
        else:
            ws.cell(i, 4, f"=D{i-1}+C{i}")     # running total: chains all rows
        ws.cell(i, 5, f"=SUM(B2:B{n+1})")      # aggregate: whole column
    wb.save(path)
    print(f"wrote {path}")


def main(argv):
    if len(argv) >= 3 and argv[1] == "--gen":
        gen_sample(argv[2])
        probe_file(argv[2])
        return
    if len(argv) < 2:
        print(__doc__)
        return
    for p in argv[1:]:
        probe_file(p)


if __name__ == "__main__":
    main(sys.argv)
