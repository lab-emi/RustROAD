#!/usr/bin/env python3
"""Reference-extraction work of a DEF's regular wiring (NETS), counted from the DEF alone.

Black-box measurements of OpenRCX (no source read; docs/extraction.md, "Oracles") on the Ibex
SG13G2 layouts: its extraction time follows the number of wires it reports extracting, and that
number is the regular wiring's *pieces* (plus the special wiring): every maximal collinear
same-layer run of a net, split at each junction strictly inside it (a via landing or a branch).
OpenRCX splits there whether or not the DEF does, and a DEF that leaves a via inside a run changes
its RC network, so export-def ends a path at every junction. Pieces therefore depend on the route topology,
not on how the DEF is written.

Per DEF this prints (JSON):
  segments        consecutive point pairs of all routing statements
  runs            maximal collinear same-layer runs per net (union of centre-line intervals)
  pieces          runs + distinct junctions strictly inside a run: via landings on the run's
                  layer and ends of the net's other runs on that layer (branches)
  statements      '+ ROUTED' / 'NEW' statements other than RECT patches
  trail_bound     lower bound on statements for the same segments: per connected component of a
                  net's segment/via graph, max(1, odd-degree vertices / 2)
  connections     sum over routed nets of (connected pins - 1)
  vias, patches, runs_by_layer {layer: {preferred, wrong_way}}
  pieces_by_cause first match per run: metal1 (the lowest routing layer), via_pair_jog (a
                  preferred-direction run of at most two pitches with a via landing at both ends,
                  above the lowest layer), staircase (in a chain of >= 3 same-layer runs joined at
                  plain corners - two runs, no via, no third run - that is monotone in x and y),
                  zigzag (such a chain that is not monotone), upper (fourth routing layer and up),
                  other

usage: def_pieces.py --tech-lef TECH.lef DEF [DEF ...] [--json OUT]
Supports DEF 5.8 regular wiring: '*' coordinates, extension values, vias inside paths, TAPER,
RECT patches and a DEF VIAS section. Exit 2 on a construct it does not read (MASK, VIRTUAL,
SHAPE, STYLE, explicit width). Own code, standard formats only.
"""
from __future__ import annotations

import argparse
import collections
import json
import re
import sys
from pathlib import Path

TOKEN = re.compile(r"\(\s*(\S+)\s+(\S+)(?:\s+(\S+))?\s*\)|([^\s()]+)")


def technology(lef: str):
    routing, direction, pitch = [], {}, {}
    for m in re.finditer(r"^\s*LAYER\s+(\S+)\s*\n(.*?)^\s*END\s+\1\b", lef, re.M | re.S):
        body = m.group(2)
        if re.search(r"^\s*TYPE\s+ROUTING\s*;", body, re.M):
            name = m.group(1)
            routing.append(name)
            d = re.search(r"^\s*DIRECTION\s+(\S+)", body, re.M)
            direction[name] = "h" if d and d.group(1).upper().startswith("H") else "v"
            p = re.search(r"^\s*PITCH\s+([\d.]+)", body, re.M)
            pitch[name] = float(p.group(1)) if p else 0.0
    order = {n: i for i, n in enumerate(routing)}
    vias = {}
    for m in re.finditer(r"^\s*VIA\s+(\S+).*?^\s*END\s+\1\b", lef, re.M | re.S | re.I):
        layers = sorted(set(re.findall(r"LAYER\s+(\S+)", m.group(0))) & set(routing), key=order.get)
        if len(layers) == 2:
            vias[m.group(1)] = tuple(layers)
    return routing, order, direction, pitch, vias


def def_vias(text: str, routing, order, vias):
    sec = re.search(r"^VIAS \d+ ;(.*?)^END VIAS", text, re.M | re.S)
    if not sec:
        return
    for m in re.finditer(r"^\s*- (\S+)(.*?);", sec.group(1), re.M | re.S):
        lay = re.search(r"\+ LAYERS (\S+) \S+ (\S+)", m.group(2))
        names = [lay.group(1), lay.group(2)] if lay else re.findall(r"\+ RECT (\S+)", m.group(2))
        names = sorted(set(names) & set(routing), key=order.get)
        if len(names) == 2:
            vias[m.group(1)] = tuple(names)


def parse_net(entry: str, vias):
    """One NETS entry without its ';' -> (name, pins, wires, vias, patches, statements)."""
    cut = re.search(r"\+\s*(ROUTED|FIXED|COVER|NOSHIELD)\b", entry)
    header = entry[:cut.start()] if cut else entry
    name = header.split()[1]
    pins = len(re.findall(r"\(\s*\S+\s+\S+\s*\)", header))
    wires, landings, patches, statements = [], [], 0, 0
    if not cut:
        return name, pins, wires, landings, patches, statements
    body = re.split(r"\+\s*(?:ROUTED|FIXED|COVER|NOSHIELD)\b|\+\s*(?:USE|WEIGHT|SOURCE|PROPERTY)\b.*$", entry[cut.start():])
    for part in body:
        for stmt in re.split(r"\bNEW\b", part or ""):
            stmt = re.sub(r"\b(TAPER|TAPERRULE\s+\S+)\b", " ", stmt).strip()
            if not stmt:
                continue
            if re.search(r"\b(MASK|VIRTUAL|SHAPE|STYLE)\b", stmt):
                raise ValueError("unsupported routing construct in net " + name)
            m = re.match(r"(\S+)\s*(.*)$", stmt, re.S)
            layer, rest = m.group(1), m.group(2)
            if re.match(r"-?\d", rest):
                raise ValueError("explicit width in net " + name)
            if re.search(r"\bRECT\b", rest):
                patches += 1
                continue
            statements += 1
            last, cur = None, layer
            for t in TOKEN.finditer(rest):
                if t.group(1) is not None:
                    x = last[0] if t.group(1) == "*" else int(t.group(1))
                    y = last[1] if t.group(2) == "*" else int(t.group(2))
                    if last is not None and (x, y) != last:
                        if x != last[0] and y != last[1]:
                            raise ValueError("diagonal segment in net " + name)
                        wires.append((cur, last[0], last[1], x, y))
                    last = (x, y)
                    continue
                word = t.group(4)
                if word not in vias or last is None:
                    raise ValueError("unknown via %r in net %s" % (word, name))
                landings.append((word, last[0], last[1]))
                lo, hi = vias[word]
                cur = hi if cur == lo else lo if cur == hi else None
                if cur is None:
                    raise ValueError("via %s off the path layer in net %s" % (word, name))
    return name, pins, wires, landings, patches, statements


def runs_of(wires):
    lines = collections.defaultdict(list)
    for layer, x1, y1, x2, y2 in wires:
        if y1 == y2:
            lines[(layer, "h", y1)].append((min(x1, x2), max(x1, x2)))
        else:
            lines[(layer, "v", x1)].append((min(y1, y2), max(y1, y2)))
    out = []
    for (layer, o, c), ivs in sorted(lines.items()):
        ivs.sort()
        lo, hi = ivs[0]
        for a, b in ivs[1:]:
            if a <= hi:
                hi = max(hi, b)
            else:
                out.append((layer, o, c, lo, hi))
                lo, hi = a, b
        out.append((layer, o, c, lo, hi))
    return out


def ends(run):
    layer, o, c, lo, hi = run
    return ((lo, c), (hi, c)) if o == "h" else ((c, lo), (c, hi))


def net_stats(wires, landings, vias, routing, direction, pitch, units):
    runs = runs_of(wires)
    land = collections.defaultdict(set)
    for v, x, y in landings:
        for layer in vias[v]:
            land[layer].add((x, y))
    by_line = collections.defaultdict(list)
    for r in runs:
        by_line[(r[0], r[1], r[2])].append((r[3], r[4]))

    def inside(layer, p):
        for o, c, a in (("h", p[1], p[0]), ("v", p[0], p[1])):
            for lo, hi in by_line.get((layer, o, c), ()):
                if lo < a < hi:
                    return True
        return False

    at = collections.defaultdict(list)
    for i, r in enumerate(runs):
        for p in ends(r):
            at[(r[0], p)].append(i)
    # junctions strictly inside a run: via landings on its layer and ends of the net's other runs
    # on that layer (a branch that meets the run's interior)
    junction = collections.defaultdict(set)
    for layer, pts in land.items():
        junction[layer] |= pts
    for r in runs:
        junction[r[0]].update(ends(r))
    inner = []
    for r in runs:
        layer, o, c, lo, hi = r
        inner.append(sum(1 for (x, y) in junction[layer] if (o == "h" and y == c and lo < x < hi) or (o == "v" and x == c and lo < y < hi)))
    # chains of >= 3 runs joined at plain corners
    plain = lambda layer, p: len(at[(layer, p)]) == 2 and p not in land[layer] and not inside(layer, p)
    kind, seen = {}, set()
    for i in range(len(runs)):
        if i in seen:
            continue
        seen.add(i)
        chain = collections.deque([i])
        for side in (0, 1):
            cur, p = i, ends(runs[i])[side]
            while plain(runs[cur][0], p):
                nxt = [j for j in at[(runs[cur][0], p)] if j != cur]
                if not nxt or nxt[0] in seen:
                    break
                cur = nxt[0]
                seen.add(cur)
                (chain.appendleft if side == 0 else chain.append)(cur)
                a, b = ends(runs[cur])
                p = b if a == p else a
        if len(chain) < 3:
            continue
        first, second = ends(runs[chain[0]]), ends(runs[chain[1]])
        cur = first[0] if first[1] in second else first[1]
        points = [cur]
        for j in chain:
            a, b = ends(runs[j])
            cur = b if a == cur else a
            points.append(cur)
        sx = {q[0] > p[0] for p, q in zip(points, points[1:]) if q[0] != p[0]}
        sy = {q[1] > p[1] for p, q in zip(points, points[1:]) if q[1] != p[1]}
        for j in chain:
            kind[j] = "staircase" if len(sx) <= 1 and len(sy) <= 1 else "zigzag"
    cause = collections.Counter()
    by_layer = collections.defaultdict(collections.Counter)
    for i, r in enumerate(runs):
        layer, o, c, lo, hi = r
        preferred = o == direction[layer]
        by_layer[layer]["preferred" if preferred else "wrong_way"] += 1
        a, b = ends(r)
        if layer == routing[0]:
            k = "metal1"
        elif preferred and a in land[layer] and b in land[layer] and hi - lo <= round(2 * pitch[layer] * units):
            k = "via_pair_jog"
        elif i in kind:
            k = kind[i]
        elif routing.index(layer) >= 3:
            k = "upper"
        else:
            k = "other"
        cause[k] += 1 + inner[i]
    # trail lower bound over the segments as written
    parent = {}

    def find(v):
        parent.setdefault(v, v)
        while parent[v] != v:
            parent[v] = parent[parent[v]]
            v = parent[v]
        return v
    degree = collections.Counter()
    edges = [((w[0], w[1], w[2]), (w[0], w[3], w[4])) for w in wires]
    edges += [((vias[v][0], x, y), (vias[v][1], x, y)) for v, x, y in landings]
    for a, b in edges:
        degree[a] += 1
        degree[b] += 1
        parent[find(a)] = find(b)
    odd = collections.Counter()
    for v, d in degree.items():
        odd[find(v)] += d % 2
    bound = sum(max(1, n // 2) for n in odd.values())
    return dict(runs=len(runs), pieces=len(runs) + sum(inner), inner=sum(inner), cause=cause,
                by_layer=by_layer, trail_bound=bound)


def analyse(def_path: Path, tech):
    routing, order, direction, pitch, lef_vias = tech
    text = def_path.read_text()
    vias = dict(lef_vias)
    def_vias(text, routing, order, vias)
    units = int(re.search(r"^UNITS DISTANCE MICRONS (\d+)", text, re.M).group(1))
    sec = re.search(r"^NETS (\d+) ;\n(.*?)^END NETS", text, re.M | re.S)
    total = collections.Counter()
    cause = collections.Counter()
    by_layer = collections.defaultdict(collections.Counter)
    nets = 0
    for entry in re.split(r";\s*\n(?=\s*- |\Z)", sec.group(2)):
        entry = entry.strip().rstrip(";")
        if not entry:
            continue
        nets += 1
        name, pins, wires, landings, patches, statements = parse_net(entry, vias)
        total["segments"] += len(wires)
        total["vias"] += len(landings)
        total["patches"] += patches
        total["statements"] += statements
        if wires or landings:
            total["routed_nets"] += 1
            total["connections"] += max(pins - 1, 0)
            s = net_stats(wires, landings, vias, routing, direction, pitch, units)
            for k in ("runs", "pieces", "inner", "trail_bound"):
                total[k] += s[k]
            cause.update(s["cause"])
            for layer, c in s["by_layer"].items():
                by_layer[layer].update(c)
    if nets != int(sec.group(1)):
        raise ValueError("NETS count %s, read %d" % (sec.group(1), nets))
    out = dict(def_=str(def_path), nets=nets, **total)
    out["interior_junctions"] = out.pop("inner", 0)
    out["pieces_per_connection"] = round(out["pieces"] / max(out["connections"], 1), 4)
    out["pieces_by_cause"] = dict(sorted(cause.items()))
    out["runs_by_layer"] = {l: dict(sorted(by_layer[l].items())) for l in routing if l in by_layer}
    out["def"] = out.pop("def_")
    return out


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--tech-lef", type=Path, required=True)
    ap.add_argument("defs", type=Path, nargs="+")
    ap.add_argument("--json", type=Path)
    a = ap.parse_args(argv)
    tech = technology(a.tech_lef.read_text())
    try:
        result = [analyse(d, tech) for d in a.defs]
    except ValueError as e:
        print("def_pieces: " + str(e), file=sys.stderr)
        return 2
    text = json.dumps(result, indent=1)
    if a.json:
        a.json.write_text(text + "\n")
    print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
