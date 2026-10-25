#!/usr/bin/env python3
"""Independent bounded legality checker for RustROAD's documented DEF 5.8 subset.

Read-only: exact design metadata, row lattice, cell coverage, geometry and overlaps.
This is not a DRC/LVS checker and does not validate routing or connectivity.
"""
import argparse
import bisect
import json
import re
import sys
from pathlib import Path

MAX_BYTES = 16 * 1024 * 1024
MAX_CELLS = 250_000
MAX_ROWS = 100_000
MAX_STATEMENTS = 350_010
NAME = r"[A-Za-z0-9_-]{1,128}"
INT = r"-?[0-9]+"


def require(ok, message):
    if not ok:
        raise ValueError(message)


def integer(x):
    require(type(x) is int and -(1 << 63) <= x < (1 << 63), "coordinate must be an i64 integer")
    return x


def read_bounded(path):
    with path.open("rb") as f:
        data = f.read(MAX_BYTES + 1)
    require(len(data) <= MAX_BYTES, f"input exceeds {MAX_BYTES} bytes")
    return data.decode("utf-8")


def box(value):
    coords = tuple(integer(value[k]) for k in ("lx", "ly", "ux", "uy"))
    require(coords[0] < coords[2] and coords[1] < coords[3], "empty or inverted bounding box")
    return coords


def contains(outer, inner):
    return outer[0] <= inner[0] and outer[1] <= inner[1] and inner[2] <= outer[2] and inner[3] <= outer[3]


class Fenwick:
    def __init__(self, n):
        self.tree = [0] * (n + 1)

    def add(self, i, value):
        i += 1
        while i < len(self.tree):
            self.tree[i] += value
            i += i & -i

    def prefix(self, n):
        result = 0
        while n:
            result += self.tree[n]
            n -= n & -n
        return result


def validate_rows(rows, height):
    # Independent x sweep: active row starts within +/- height imply y overlap.
    ys = sorted({r[2] for r in rows})
    events = []
    for i, (x0, x1, y) in enumerate(rows):
        events.extend(((x0, 1, i), (x1, 0, i)))
    counts = Fenwick(len(ys))
    for _, entering, i in sorted(events):
        y = rows[i][2]
        slot = bisect.bisect_left(ys, y)
        if entering:
            lo, hi = bisect.bisect_right(ys, y - height), bisect.bisect_left(ys, y + height)
            require(counts.prefix(hi) == counts.prefix(lo), "physical rows overlap")
            counts.add(slot, 1)
        else:
            counts.add(slot, -1)


def check(input_path, def_path):
    design = json.loads(read_bounded(input_path))
    require(type(design) is dict, "design must be an object")
    require(type(design.get("name")) is str and re.fullmatch(NAME, design["name"]), "invalid design name")
    units = integer(design["units_per_micron"])
    require(units > 0, "units_per_micron must be positive")
    die, core = box(design["die_area"]), box(design["core_area"])
    require(contains(die, core), "core lies outside die")
    site_name = design["site"].get("name", "rustroad_site")
    require(type(site_name) is str and re.fullmatch(NAME, site_name), "invalid site name")
    width, height = integer(design["site"]["width"]), integer(design["site"]["height"])
    require(width > 0 and height > 0, "site dimensions must be positive")
    require(type(design["rows"]) is list and 0 < len(design["rows"]) <= MAX_ROWS, "row budget exceeded or rows absent")
    require(type(design["cells"]) is list and len(design["cells"]) <= MAX_CELLS, "cell budget exceeded")
    rows = []
    for r in design["rows"]:
        x0, x1, y = (integer(r[k]) for k in ("x_start", "x_end", "y"))
        require(x0 < x1 and (x1 - x0) % width == 0, "row is not a positive site lattice")
        require(contains(core, (x0, y, x1, y + height)), "row outside core")
        rows.append((x0, x1, y))
    validate_rows(rows, height)
    expected = {}
    for c in design["cells"]:
        require(all(type(c.get(k)) is str and re.fullmatch(NAME, c[k]) for k in ("name", "master")), "invalid cell name or master")
        require(c["name"] not in expected, "duplicate input instance")
        cw, ch = integer(c["width"]), integer(c["height"])
        require(cw > 0 and ch == height and cw % width == 0, "unsupported cell dimensions")
        expected[c["name"]] = (c["master"], cw, ch)
    by_y = {}
    for idx, row in enumerate(rows):
        by_y.setdefault(row[2], []).append((row[0], row[1], idx))
    for segments in by_y.values():
        segments.sort()
    placements, exported_rows = {}, []
    headers, declared_count, in_components, ended_components, ended_design = set(), None, False, False, False
    text = read_bounded(def_path)
    require(len(text.splitlines()) <= MAX_STATEMENTS, "DEF statement budget exceeded")
    for line_number, line in enumerate(text.splitlines(), 1):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        require(len(line) <= 4096, f"statement too long at line {line_number}")
        require(not ended_design, "content after END DESIGN")
        if line == "END COMPONENTS":
            require(in_components, "unexpected END COMPONENTS")
            in_components, ended_components = False, True
            continue
        if line == "END DESIGN":
            require(ended_components, "END DESIGN before END COMPONENTS")
            ended_design = True
            continue
        if in_components:
            match = re.fullmatch(rf"- ({NAME}) ({NAME}) \+ PLACED \( ({INT}) ({INT}) \) N ;", line)
            require(match is not None, f"unsupported component syntax at line {line_number}")
            name, master, x, y = match.groups()
            require(name not in placements, "duplicate DEF instance")
            require(name in expected and expected[name][0] == master, "unknown instance or master mismatch")
            x, y = integer(int(x)), integer(int(y))
            _, cw, ch = expected[name]
            require(-(1 << 63) <= x + cw < (1 << 63) and -(1 << 63) <= y + ch < (1 << 63), "placement coordinate overflow")
            segments = by_y.get(y, [])
            loc = bisect.bisect_right(segments, (x, 1 << 64, 1 << 64)) - 1
            require(loc >= 0, f"{name}: cell not on a row")
            x0, x1, row_id = segments[loc]
            require(x0 <= x and x + cw <= x1 and (x - x0) % width == 0, f"{name}: off lattice or beyond row")
            require(contains(core, (x, y, x + cw, y + ch)), f"{name}: outside core")
            placements[name] = (row_id, x, x + cw)
            continue
        patterns = [
            ("version", r"VERSION 5\.8 ;", None),
            ("divider", r'DIVIDERCHAR "/" ;', None),
            ("bus", r'BUSBITCHARS "\[\]" ;', None),
            ("design", rf"DESIGN ({NAME}) ;", (design["name"],)),
            ("units", rf"UNITS DISTANCE MICRONS ({INT}) ;", (str(units),)),
            ("die", rf"DIEAREA \( ({INT}) ({INT}) \) \( ({INT}) ({INT}) \) ;", tuple(str(n) for n in die)),
        ]
        found = False
        for key, pattern, want in patterns:
            match = re.fullmatch(pattern, line)
            if match:
                require(key not in headers and not ended_components, "duplicate or misplaced DEF header")
                require(want is None or match.groups() == want, f"DEF {key} mismatch")
                headers.add(key)
                found = True
                break
        if found:
            continue
        match = re.fullmatch(rf"ROW ({NAME}) {re.escape(site_name)} ({INT}) ({INT}) N DO ([0-9]+) BY 1 STEP ({INT}) 0 ;", line)
        if match:
            require(not ended_components and declared_count is None, "row after components")
            name, x, y, count, step = match.groups()
            require(all(name != prior[0] for prior in exported_rows[-1:]), "duplicate consecutive row name")
            exported_rows.append((name, int(x), int(y), int(count), int(step)))
            require(len(exported_rows) <= MAX_ROWS, "DEF row budget exceeded")
            continue
        match = re.fullmatch(r"COMPONENTS ([0-9]+) ;", line)
        require(match is not None and declared_count is None and not ended_components, f"unsupported DEF syntax at line {line_number}")
        declared_count = int(match.group(1))
        require(declared_count == len(expected), "DEF component count mismatch")
        in_components = True
    require(ended_design and headers == {"version", "divider", "bus", "design", "units", "die"}, "incomplete DEF document")
    require(len(placements) == len(expected), "missing placed instances")
    require(len(exported_rows) == len(rows) and len({r[0] for r in exported_rows}) == len(rows), "missing or duplicate DEF rows")
    actual_rows = sorted((x, x + count * step, y) for _, x, y, count, step in exported_rows if step == width and count > 0)
    require(actual_rows == sorted(rows), "DEF rows differ from physical input")
    previous = {}
    for name, (row_id, x, end) in sorted(placements.items(), key=lambda item: item[1]):
        require(previous.get(row_id, -(1 << 63)) <= x, f"{name}: overlaps another cell")
        previous[row_id] = end
    return {"schema": "rustroad.def_legality.v1", "status": "passed", "instances": len(placements), "rows": len(rows),
            "checks": ["identity", "coverage", "row_geometry", "site_alignment", "containment", "overlap"],
            "scope": "single-height N-oriented cells; exported DEF subset only", "drc_status": "unchecked", "lvs_status": "unchecked", "routing_status": "unchecked"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--def", dest="def_path", type=Path, required=True)
    args = parser.parse_args()
    try:
        report = check(args.input, args.def_path)
    except (ValueError, KeyError, TypeError, OSError, UnicodeError) as error:
        print(json.dumps({"schema": "rustroad.def_legality.v1", "status": "failed", "diagnostic": str(error)}))
        return 1
    print(json.dumps(report, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
