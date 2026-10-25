# SPDX-License-Identifier: Apache-2.0
"""Timing-closure ECO planners of the track-recipe closure loop (clean-room).

Used by scripts/hold_closure.Flow (track recipe, `sizing_profile` set). Every
planner reads RustSTA 0.4 JSON reports of the current layout (the timing
oracle), the edited mapped netlist, the layout's DEF (integer DBU positions)
and the Liberty-derived sizing profile (`drive` data per corner,
scripts/build_sizing_profile.py), and returns typed `rustroad.eco-plan.v1`
operations that `rustroad apply-eco` checks and applies. Nothing here edits a
netlist itself, reads a reference tool's source or changes a constraint.

* `plan_buffering` -- buffer insertion on nets with max_capacitance /
  max_transition / max_fanout violations (any corner, 0.95 x limit planning
  margin): the movable sinks (instance input pins and output ports read
  through an `assign`) are split by location into clusters (recursive
  bisection on DEF positions, integer DBU) whose estimated load lets one
  buffer meet the transition target, each cluster is driven by one buffer
  (`buffer_sinks`; the analytic placer pulls it towards its cluster), and a
  second level is added when the driver would still see too many buffer
  inputs. Input ports with many sinks are buffered the same way (port nets
  cannot be resized). Nets on failing setup paths get critical-sink
  isolation. A setup guard keeps every sink whose estimated slack would
  drop (or that has < CRITICAL_SLACK_NS) on its driver and charges each
  moved sink's estimated cost to its downstream cone, so two buffers of one
  plan cannot spend one path's slack twice. Sizing is not planned here:
  `hold_closure.Flow.whatif_sizing` decides resizes with RustSTA `whatif`.
* `net_weights` -- timing-driven placement: nets whose driver pin is within a
  window of the worst setup slack get a weight up to `MAX_WEIGHT` (milli-units,
  `rustroad place --net-weights`).

Estimates (linear load model between the profile's two Liberty points, wire
capacitance per DBU from the net's own reported load) only choose the edit;
the loop re-places, re-routes, re-extracts and re-times every ECO.

Bounds: `MAX_BUFFER_OPS` buffers per call, `MAX_CLUSTERS` clusters per
net, `MAX_CLUSTER_FANOUT` items per cluster, a two-level tree at most,
`MAX_NET_ITEMS` items per net, `MAX_CONSUME_VISITS` pin visits of the guard
walks; the bisection is linear in the items times log(items).
"""
from __future__ import annotations

from dataclasses import dataclass, field
import json
import re
from pathlib import Path

MAX_BUFFER_OPS = 512
MAX_CLUSTERS = 32
MAX_CLUSTER_FANOUT = 16
MAX_NET_ITEMS = 4096
MAX_TREE_LEVELS = 2
# Transition target of a planned driver, as a fraction of the limit: the
# re-placed, re-routed net differs from the estimate.
TRANSITION_TARGET = 0.6
# Nets above this fraction of a transition/capacitance limit are planned.
DRV_PLAN_MARGIN = 0.95
# A driver keeps (or gets resized to) a transition below this fraction.
DRIVER_TARGET = 0.8
CAPACITANCE_TARGET = 0.6
# Bound of the downstream slack-consumption walks of one call (pin visits).
MAX_CONSUME_VISITS = 2_000_000
# Buffering estimate: a sink's delay grows by this much per ns of input slew.
SLEW_SENSITIVITY = 0.35
# A moved sink must keep this much estimated setup slack (or not get slower).
SETUP_GUARD_NS = 0.15
# Sinks with less setup slack (worst of the pins-report corners) stay on
# their driver: a buffer in front of them would cost the path its delay.
CRITICAL_SLACK_NS = 0.75
# Critical-sink isolation: sinks at least this much above the net's worst
# setup slack move behind a buffer (they absorb the buffer delay).
ISOLATION_MARGIN_NS = 0.6
ISOLATION_MIN_FANOUT = 3
# Timing-driven weights: nets within WINDOW of the worst setup slack.
WEIGHT_WINDOW_NS = 1.0
MAX_WEIGHT = 5000
OUTPUT_PORT_LOAD_PF = 0.01
DEFAULT_PIN_CAP_PF = 0.005
BUFFER_PREFIX = "rr_eco_buf_"
# Hold-ECO cells are never resized (their delay is the hold fix): a weak one
# driving a long wire gets a buffer behind it (more delay, which hold likes).
HOLD_PREFIX = "rr_eco_hold_"
# Driver/buffer co-sizing. A driver whose best family member cannot reach
# DRIVER_TARGET even at the smallest load a buffer arrangement can present
# (smallest buffer input + NEAR_ROWS row heights of wire) plans with a target
# this fraction of the way from that floor to the limit (GF180 aoi22 at ss:
# floor 0.89 ns at 4 fF, target 0.96 ns).
DRIVER_HEADROOM = 0.6
# Wire between a driver and a cell placed next to it (seeded at the driver):
# NEAR_ROWS row heights at the layout's median wire capacitance per DBU.
# Measured on the GF180 final2 layouts (two-pin nets, routed): median 0.9-1.1
# fF under 5 um, 1.7-1.9 fF at 5-10 um; median 0.245 fF/um over all nets, so
# 1.5 rows (5.9 um) is 1.4 fF.
NEAR_ROWS = 1.5
# Placement weight (milli-units) of a weak driver's net to the buffer seeded
# next to it: detailed placement then never moves the buffer towards its
# cluster (that move costs the driver-side HPWL as much as it saves).
DRIVER_NET_WEIGHT = 8000
# Best-effort rebalance of a violating weak-driver net (nothing meets the
# planning target): accepted when the driver's model transition is at most
# this fraction of the limit and BEST_EFFORT_GAIN_NS below the observed one.
BEST_EFFORT = 0.985
# Output target (fraction of the limit) of a buffer behind a headroom-limited
# driver (buffer_target).
HEADROOM_BUFFER_TARGET = 0.7
BEST_EFFORT_GAIN_NS = 0.02
# A rebalance must take at least 10 % of the driver's load away (estimated):
# otherwise it re-plans the arrangement the layout already has (alu: a root
# in front of a root next to the same driver on consecutive layouts).
REBALANCE_RELIEF = 0.9
# A weak driver's input-slew degradation (observed transition above the
# Liberty model at the observed load, 0.2 ns input) is subtracted from its
# target, at most this fraction of the limit.
MAX_SLEW_DEGRADATION = 0.3

INSTANCE_LINE = re.compile(r"^\s*(\S+)\s+(\S+)\s+\((.*)\);\s*$")
CONNECTION = re.compile(r"\.(\w+)\(([^()]*)\)")
PORT_DECL = re.compile(r"^\s*(input|output|inout)\s+(?:wire\s+)?(?:\[(-?\d+):(-?\d+)\]\s*)?(\\?\S+)\s*$")
HEADER = re.compile(r"(?s)\bmodule\s+\S+\s*\((.*?)\)\s*;")
ASSIGN_LINE = re.compile(r"^\s*assign\s+(.+?)\s*=\s*(.+?)\s*;\s*$")


def key(net: str) -> str:
    """Netlist/report net name without Verilog escape (`\\q[0] ` -> `q[0]`)."""
    return net.strip().lstrip("\\").strip()


@dataclass
class Netlist:
    """Flat structural view: drivers, sinks and output-port aliases per net."""
    instances: dict = field(default_factory=dict)       # inst -> (master, {pin: net})
    inputs: set = field(default_factory=set)             # input port bits
    outputs: set = field(default_factory=set)            # output port bits
    driver: dict = field(default_factory=dict)           # net -> "inst/pin" | port bit
    sinks: dict = field(default_factory=dict)            # net -> [(inst, pin)]
    port_alias: dict = field(default_factory=dict)       # net -> [output port bits read by assign]


def parse_netlist(text: str, lef: dict) -> Netlist:
    n = Netlist()
    header = HEADER.search(text)
    for decl in (header[1].split(",") if header else []):
        m = PORT_DECL.match(decl)
        if m:
            name = key(m[4])
            bits = [name] if m[2] is None else [f"{name}[{i}]" for i in range(min(int(m[2]), int(m[3])),
                                                                              max(int(m[2]), int(m[3])) + 1)]
            (n.inputs if m[1] == "input" else n.outputs).update(bits)
    for line in text.splitlines():
        m = ASSIGN_LINE.match(line)
        if m:
            lhs, rhs = key(m[1]), key(m[2])
            if lhs in n.outputs:
                n.port_alias.setdefault(rhs, []).append(lhs)
            continue
        m = INSTANCE_LINE.match(line)
        if m and m[1] not in ("module", "input", "output", "wire", "assign", "inout"):
            conns = {p: key(v) for p, v in CONNECTION.findall(m[3])}
            n.instances[key(m[2])] = (m[1], conns)
    for name, (master, conns) in n.instances.items():
        pins = lef.get(master) or {}
        for pin, net in conns.items():
            if not net or net.startswith(("1'b", "0'", "{")):
                continue
            d = (pins.get(pin) or {}).get("direction") or ""
            if d.startswith("OUTPUT"):
                n.driver[net] = f"{name}/{pin}"
            elif d == "INPUT":
                n.sinks.setdefault(net, []).append((name, pin))
    for bit in n.inputs:
        n.driver.setdefault(bit, bit)
    return n


# Physical-only masters that the next placement re-inserts (free space for a
# seeded cell); taps and endcaps stay and split the rows into the
# legalizer's intervals.
FILLER = re.compile(r"__(fill_|fillcap_|decap_)")
TAP = re.compile(r"(filltie|tapvpwrvgnd|__tap|endcap)")
# Rows searched above and below the wanted position for room.
FREE_SPOT_ROWS = 6


class Occupancy:
    """Free capacity of the legalizer's intervals (DEF ROWs split at tap and
    endcap cells; every other non-filler component uses its width) of a
    placed layout, for seeding a new or moved cell where the next placement
    keeps it: the legalizer puts a cell into the nearest interval with room
    and packs that interval by target x, so a cell seeded into such an
    interval lands next to its seed; seeded next to a driver in a full
    interval it is pushed to the nearest one with room (measured on GF180
    alu: 87 um). Reservations keep the seeds of one plan within capacity.
    Integer DBU."""

    def __init__(self, def_text: str, widths: dict, spacing: int = 0):
        rows = []
        for m in re.finditer(r"(?m)^ROW \S+ \S+ (-?\d+) (-?\d+) \S+ DO (\d+) BY 1 STEP (\d+) 0 ;", def_text):
            x0, y, n, step = int(m[1]), int(m[2]), int(m[3]), int(m[4])
            rows.append((y, x0, x0 + n * step, step))
        rows.sort()
        self.spacing = spacing
        taps: dict[int, list] = {r[0]: [] for r in rows}
        cells: dict[int, list] = {r[0]: [] for r in rows}
        body = def_text.split("\nCOMPONENTS ", 1)[1].split("\nEND COMPONENTS", 1)[0] if "\nCOMPONENTS " in def_text else ""
        for m in re.finditer(r"(?m)^- (\S+) (\S+) .*?\+ (?:PLACED|FIXED) \( (-?\d+) (-?\d+) \)", body):
            y = int(m[4])
            if FILLER.search(m[2]) or y not in taps:
                continue
            span = (int(m[3]), int(m[3]) + widths.get(m[2], 0))
            (taps if TAP.search(m[2]) else cells)[y].append((span, key(m[1])))
        # Intervals: [y, lo, hi, site, free capacity]; the cell -> interval map.
        self.intervals, self.owner = [], {}
        for y, x0, x1, site in rows:
            edges = [x0] + [e for (a, b), _ in sorted(taps[y]) for e in (a, b)] + [x1]
            for j in range(0, len(edges), 2):
                lo, hi = edges[j], edges[j + 1]
                if hi > lo:
                    self.intervals.append([y, lo, hi, site, hi - lo])
            for (a, b), name in cells[y]:
                for k, iv in enumerate(self.intervals):
                    if iv[0] == y and iv[1] <= a < iv[2]:
                        iv[4] -= (b - a) + spacing
                        self.owner[name] = (k, (b - a) + spacing)
                        break

    def release(self, inst: str):
        """A cell that moves frees its old capacity."""
        if inst in self.owner:
            k, used = self.owner.pop(inst)
            self.intervals[k][4] += used

    def spot(self, centre: tuple, width: int, inst: str, budget: list) -> tuple | None:
        """Lower-left nearest `centre` (Manhattan, as the legalizer measures)
        in an interval within FREE_SPOT_ROWS rows that has room for `width`;
        reserved for `inst`. None: no room nearby."""
        if not self.intervals:
            return None
        ys = sorted({iv[0] for iv in self.intervals})
        k = min(range(len(ys)), key=lambda i: (abs(ys[i] - centre[1]), i))
        near = set(ys[max(0, k - FREE_SPOT_ROWS):k + FREE_SPOT_ROWS + 1])
        best = None
        for idx, (y, lo, hi, site, free) in enumerate(self.intervals):
            budget[0] -= 1
            if budget[0] < 0:
                raise RuntimeError("closure ECO: free-spot search budget exceeded")
            if y not in near or free < width + self.spacing or hi - lo < width:
                continue
            x = min(max(centre[0] - width // 2, lo), hi - width)
            x = lo + ((x - lo) // site) * site
            cost = (abs(x + width // 2 - centre[0]) + abs(y - centre[1]), y, x)
            if best is None or cost < best[0]:
                best = (cost, idx, y, x)
        if best is None:
            return None
        _, idx, y, x = best
        self.intervals[idx][4] -= width + self.spacing
        self.owner[inst] = (idx, width + self.spacing)
        return (x, y)


def def_positions(text: str) -> tuple[dict, dict]:
    """(instance -> lower-left (x, y), port -> pin centre (x, y)) in DBU."""
    comps, pins = {}, {}
    sect = None
    for line in text.splitlines():
        if line.startswith("COMPONENTS "):
            sect = "c"
        elif line.startswith("PINS "):
            sect = "p"
        elif line.startswith("END "):
            sect = None
        elif sect == "c" and line.startswith("- "):
            m = re.match(r"- (\S+) \S+ .*?\+ (?:PLACED|FIXED) \( (-?\d+) (-?\d+) \)", line)
            if m:
                comps[key(m[1])] = (int(m[2]), int(m[3]))
        elif sect == "p" and line.startswith("- "):
            m = re.match(r"- (\S+) .*?\( (-?\d+) (-?\d+) \) \( (-?\d+) (-?\d+) \) \+ (?:FIXED|PLACED) \( (-?\d+) (-?\d+) \)", line)
            if m:
                pins[key(m[1])] = (int(m[6]) + (int(m[2]) + int(m[4])) // 2, int(m[7]) + (int(m[3]) + int(m[5])) // 2)
    return comps, pins


@dataclass
class Buffer:
    name: str
    input_pin: str
    width: int
    cap: float          # input capacitance at the planning corner (pF)
    loads: tuple        # (C1, C2) pF
    slew: tuple         # (t1, t2) ns
    delay: tuple        # (d1, d2) ns

    def slew_at(self, c: float) -> float:
        return interp(self.loads, self.slew, c)

    def delay_at(self, c: float) -> float:
        return interp(self.loads, self.delay, c)

    def max_load(self, target: float) -> float:
        (c1, c2), (t1, t2) = self.loads, self.slew
        k = (t2 - t1) / (c2 - c1)
        return c1 + (target - t1) / k if k > 0 else float("inf")


def normalized_function(function: str) -> str:
    """Liberty function without whitespace, quotes and fully enclosing
    parentheses (the rule `rustroad apply-eco` checks buffers with)."""
    s = "".join(c for c in function if not c.isspace() and c != '"')
    while len(s) >= 2 and s[0] == "(" and s[-1] == ")":
        depth = 0
        for k, ch in enumerate(s):
            depth += ch == "("
            depth -= ch == ")"
            if depth == 0 and k + 1 < len(s):
                return s
        s = s[1:-1]
    return s


def interp(xs, ys, x):
    (x1, x2), (y1, y2) = xs, ys
    return y1 + (y2 - y1) * (x - x1) / (x2 - x1)


class Profile:
    """Sizing profile view: families, widths and drive data at one corner."""

    def __init__(self, data: dict, corner: str, max_width: int | None):
        self.loads = tuple(data["drive_model"]["loads_pf"])
        self.family, self.width, self.drive = {}, {}, {}
        self.buffers: list[Buffer] = []
        for f in data["families"]:
            names = [c["name"] for c in f["cells"]]
            for k, c in enumerate(f["cells"]):
                self.family[c["name"]] = (f["id"], k, names)
                self.width[c["name"]] = c["width_dbu"]
                d = (c.get("drive") or {}).get(corner)
                if d:
                    self.drive[c["name"]] = d
            ins = [p for p, d in f["pins"].items() if d == "input"]
            outs = [p for p, d in f["pins"].items() if d == "output"]
            if len(ins) == 1 and len(outs) == 1 and len(f["pins"]) == 2 and \
                    normalized_function(f["functions"].get(outs[0], "")) == ins[0]:
                for c in f["cells"]:
                    d = self.drive.get(c["name"])
                    if d and "slew_ns" in d and (max_width is None or c["width_dbu"] <= max_width):
                        self.buffers.append(Buffer(c["name"], ins[0], c["width_dbu"], d["input_cap_pf"][ins[0]], self.loads,
                                                   tuple(d["slew_ns"]), tuple(d["delay_ns"])))
        self.buffers.sort(key=lambda b: (b.width, b.name))

    def pin_cap(self, master: str, pin: str) -> float:
        return float(((self.drive.get(master) or {}).get("input_cap_pf") or {}).get(pin, DEFAULT_PIN_CAP_PF))

    def slew_at(self, master: str, c: float) -> float | None:
        d = self.drive.get(master)
        return interp(self.loads, d["slew_ns"], c) if d and "slew_ns" in d else None

    def best_slew(self, master: str, c: float, max_width: int | None) -> float | None:
        """Smallest output transition of any family member (within the
        width bound) at load `c`."""
        if master not in self.family:
            return None
        vals = [self.slew_at(m, c) for m in self.family[master][2]
                if max_width is None or self.width[m] <= max_width]
        vals = [v for v in vals if v is not None]
        return min(vals) if vals else None

    def delay_at(self, master: str, c: float) -> float | None:
        d = self.drive.get(master)
        return interp(self.loads, d["delay_ns"], c) if d and "delay_ns" in d else None

    def delay_slope(self, master: str | None) -> float:
        d = self.drive.get(master or "")
        if not d or "delay_ns" not in d:
            return 0.0
        return (d["delay_ns"][1] - d["delay_ns"][0]) / (self.loads[1] - self.loads[0])

    def capacity(self, master: str, target: float) -> float | None:
        """Load (pF) at which `master` reaches transition `target` on the
        linear model (0 if it cannot even at no load); None: no drive data."""
        d = self.drive.get(master)
        if not d or "slew_ns" not in d:
            return None
        (c1, c2), (t1, t2) = self.loads, d["slew_ns"]
        k = (t2 - t1) / (c2 - c1)
        return max(0.0, c1 + (target - t1) / k) if k > 0 else float("inf")

    def members_up(self, master: str, max_width: int | None, steps: int = 2) -> list:
        """`master` and up to `steps` larger members of its family within the
        width bound (ascending)."""
        if master not in self.family:
            return [master]
        _, k, members = self.family[master]
        return [master] + [m for m in members[k + 1:k + 1 + steps] if max_width is None or self.width[m] <= max_width]

    def same_family(self, a: str, b: str) -> bool:
        return a in self.family and b in self.family and self.family[a][0] == self.family[b][0]

    def upsize_for(self, master: str, c: float, target: float, max_width: int | None) -> str | None:
        """The larger family member that meets `target` at load `c` with the
        smallest delay at `c` (then the narrowest); None if none does."""
        if master not in self.family:
            return None
        _, k, members = self.family[master]
        ok = []
        for m in members[k + 1:]:
            if max_width is not None and self.width[m] > max_width:
                continue
            s, d = self.slew_at(m, c), self.delay_at(m, c)
            if s is not None and d is not None and s <= target:
                ok.append((d, self.width[m], m))
        return min(ok)[2] if ok else None


def hpwl(points) -> int:
    xs = [p[0] for p in points]
    ys = [p[1] for p in points]
    return (max(xs) - min(xs)) + (max(ys) - min(ys)) if points else 0


@dataclass
class Item:
    """A movable sink: an instance input pin (`inst/pin`) or an output port."""
    name: str
    pos: tuple
    cap: float
    slack: float | None
    port: bool = False


def bisect_clusters(items: list, limit_pf: float, cw: float, max_fanout: int, budget: list) -> list:
    """Recursive bisection by position until every cluster's estimated load
    (pin caps + cw * HPWL) is within `limit_pf` and its size within
    `max_fanout` (deterministic: ties by name)."""
    def load(group):
        return sum(i.cap for i in group) + cw * hpwl([i.pos for i in group])
    out, stack = [], [sorted(items, key=lambda i: i.name)]
    while stack:
        g = stack.pop()
        budget[0] -= len(g)
        if budget[0] < 0:
            raise RuntimeError("closure ECO: clustering work budget exceeded")
        if len(g) <= 1 or (len(g) <= max_fanout and load(g) <= limit_pf):
            out.append(g)
            continue
        xs, ys = [i.pos[0] for i in g], [i.pos[1] for i in g]
        axis = 0 if (max(xs) - min(xs)) >= (max(ys) - min(ys)) else 1
        g = sorted(g, key=lambda i: (i.pos[axis], i.pos[1 - axis], i.name))
        half = len(g) // 2
        stack.extend([g[half:], g[:half]])
    out.sort(key=lambda g: g[0].name)
    if len(out) > MAX_CLUSTERS:
        raise RuntimeError(f"closure ECO: more than {MAX_CLUSTERS} clusters on one net")
    return out


def centroid(group) -> tuple:
    return (sum(i.pos[0] for i in group) // len(group), sum(i.pos[1] for i in group) // len(group))


def manhattan(a: tuple, b: tuple) -> int:
    return abs(a[0] - b[0]) + abs(a[1] - b[1])


def fit_buffers(buffers: list, loads: list, target: float, slope: float, per_cap: float,
                smallest: bool = False) -> list | None:
    """One buffer per load: each meets `target` at its load and has an input
    capacitance within `per_cap` (the driver's share). Chosen by the
    smallest estimated delay (own delay + `slope` x input capacitance), or
    with `smallest` by the smallest input capacitance (a driver at its
    limit). None when some load has no such buffer."""
    out = []
    for load in loads:
        ok = [b for b in buffers if b.cap <= per_cap + 1e-12 and b.slew_at(load) <= target]
        if not ok:
            return None
        if smallest:
            out.append(min(ok, key=lambda b: (b.cap, b.delay_at(load), b.width, b.name)))
        else:
            out.append(min(ok, key=lambda b: (b.delay_at(load) + slope * b.cap, b.width, b.name)))
    return out


def buffer_target(target: float, model) -> float:
    """Output transition target of the buffers next to a driver: the planning
    target, or HEADROOM_BUFFER_TARGET x the limit behind a headroom-limited
    driver (GF180 aoi22 at ss): there the smallest buffer input decides
    whether the driver meets the limit at all, and buf_1 drives 1.25x the
    load at 0.7 x the limit that it drives at 0.6 x. (0.8 x was tried: a
    buf_1 behind an aoi22, whose ~1 ns input edge the 0.2 ns model does not
    see, then violated on its own output.)"""
    if model is not None and model.headroom:
        return target * HEADROOM_BUFFER_TARGET / TRANSITION_TARGET
    return target


@dataclass
class DriverModel:
    """What a net's driver can take: its family members (current first,
    then larger ones within the width bound) with the load each reaches the
    driver target at, the target itself and whether it is a headroom target
    (the family cannot reach DRIVER_TARGET at any arrangement's load)."""
    members: list           # [(master, capacity pF)]
    target: float
    headroom: bool
    slope: float            # current master's delay slope (ns/pF)
    degradation: float = 0.0


def pick_buffer(prof: Profile, load_pf: float, target_ns: float, driver_slope: float = 0.0,
                max_cap_pf: float | None = None) -> Buffer:
    """The buffer meeting the slew target at `load_pf` with the smallest
    estimated delay: its own delay plus the driver's extra delay for its input
    capacitance (`driver_slope`, ns/pF), among the buffers whose input
    capacitance fits the driver's budget `max_cap_pf`. None meets the target:
    the lowest output slew within the budget; none fits it: the smallest."""
    fit = [b for b in prof.buffers if max_cap_pf is None or b.cap <= max_cap_pf] or prof.buffers[:1]
    ok = [b for b in fit if b.slew_at(load_pf) <= target_ns]
    if not ok:
        return min(fit, key=lambda b: (b.slew_at(load_pf), b.width, b.name))
    return min(ok, key=lambda b: (b.delay_at(load_pf) + driver_slope * b.cap, b.width, b.name))


class Planner:
    """One call of the DRV/setup buffering planner on one layout."""

    def __init__(self, netlist: Netlist, comps: dict, ports: dict, prof: Profile, widths: dict, row_h: int,
                 seq: int):
        self.n, self.comps, self.ports, self.prof = netlist, comps, ports, prof
        self.widths, self.row_h = widths, row_h
        self.seq = seq
        self.ops, self.resizes, self.records = [], {}, []
        self.budget = [64 * MAX_NET_ITEMS * MAX_CLUSTERS]
        self.added_width = 0
        # Estimated setup slack already spent downstream of each pin by the
        # buffers planned so far in this call (max over paths), so two
        # buffers on one path cannot both spend the same slack.
        self.consumed: dict[str, float] = {}
        self.sequential: set[str] = set()
        self.walk_budget = [MAX_CONSUME_VISITS]
        # Placement seeds (lower-left DBU) of buffers that must sit next to
        # their driver, placement weights of those drivers' nets, and every
        # instance this call decided (kept out of the whatif sizing).
        self.positions: dict[str, tuple] = {}
        self.weights: dict[str, int] = {}
        self.touched: set[str] = set()
        self.occupancy: Occupancy | None = None
        self.spot_budget = [MAX_CONSUME_VISITS]

    def place_at(self, inst: str, width: int, centre: tuple):
        """Seed `inst` at the free spot nearest `centre` (the layout's
        occupancy, when known), else centred on it."""
        occ = getattr(self, "occupancy", None)
        at = None
        if occ is not None:
            occ.release(inst)
            at = occ.spot((int(centre[0]), int(centre[1])), width, inst, self.spot_budget)
        self.positions[inst] = at or (int(centre[0]) - width // 2, int(centre[1]) - self.row_h // 2)
        self.touched.add(inst)

    def near_pf(self, cw: float) -> float:
        """Estimated wire of a connection to a cell placed next to the pin
        (the layout's median wire capacitance per DBU; a short net's own
        ratio is dominated by pin access)."""
        return (getattr(self, "cw_design", 0.0) or cw) * NEAR_ROWS * self.row_h

    def arrange(self, groups: list, loads: list, driver_pos: tuple, trunk: float, base_pf: float, cw: float,
                target: float, model: DriverModel) -> dict | None:
        """Co-size the buffers of one net with its driver. Options, first
        feasible wins: buffers at their clusters (the driver takes the trunk),
        buffers next to the driver (each takes its own trunk), each with the
        current driver and then with a larger family member; then one root
        buffer next to the driver in front of the cluster buffers. Feasible:
        every buffer meets `target` at its load, and the driver meets its
        target with the buffer inputs, `base_pf` (kept sinks, ports) and the
        estimated wire. None: no option is feasible."""
        n = len(groups)
        near = self.near_pf(cw)
        target = buffer_target(target, model)
        far = [l + cw * manhattan(driver_pos, centroid(g)) for g, l in zip(groups, loads)]
        bufs = self.prof.buffers
        ups = list(range(1, len(model.members)))
        order = [("cluster", 0), ("driver", 0)] + [(m, k) for k in ups for m in ("cluster", "driver")] + \
            [("root", k) for k in range(len(model.members))]
        for mode, k in order:
            member, cap = model.members[k]
            if cap is None:
                continue
            room = cap - base_pf
            if mode == "cluster":
                picks = fit_buffers(bufs, loads, target, model.slope, (room - trunk) / n, model.headroom)
                if picks:
                    return dict(mode=mode, member=member, picks=picks, loads=loads)
            elif mode == "driver" and n <= MAX_CLUSTER_FANOUT:
                picks = fit_buffers(bufs, far, target, model.slope, (room - near * n) / n, model.headroom)
                if picks:
                    return dict(mode=mode, member=member, picks=picks, loads=far)
            elif mode == "root" and n <= MAX_CLUSTER_FANOUT:
                mid = self.prof.delay_slope(bufs[len(bufs) // 2].name)
                level1 = fit_buffers(bufs, loads, target, mid, float("inf"))
                if not level1:
                    continue
                root_load = sum(b.cap for b in level1) + trunk
                root = fit_buffers(bufs, [root_load], target, model.slope, room - near, model.headroom)
                if root:
                    return dict(mode=mode, member=member, picks=level1, loads=loads, root=root[0], root_load=root_load)
        return None

    def consume(self, pin: str, delta: float):
        """Charge `delta` ns to `pin` and every pin downstream of it through
        combinational cells (stops at sequential cells; bounded work)."""
        stack = [(pin, delta)]
        while stack:
            p, d = stack.pop()
            if d <= self.consumed.get(p, 0.0) + 1e-12:
                continue
            self.consumed[p] = d
            self.walk_budget[0] -= 1
            if self.walk_budget[0] < 0:
                raise RuntimeError("closure ECO: slack-consumption walk budget exceeded")
            inst = p.rsplit("/", 1)[0] if "/" in p else None
            if inst is None or inst not in self.n.instances:
                continue
            master, conns = self.n.instances[inst]
            if master in self.sequential:
                continue
            for out_pin, net in conns.items():
                if self.n.driver.get(net) == f"{inst}/{out_pin}":
                    for si, sp in self.n.sinks.get(net, []):
                        stack.append((f"{si}/{sp}", d))

    def pos_of(self, name: str) -> tuple | None:
        if "/" in name:
            inst = name.rsplit("/", 1)[0]
            p = self.comps.get(inst)
            if p is None:
                return None
            master = self.n.instances.get(inst, ("", {}))[0]
            return (p[0] + self.widths.get(master, 0) // 2, p[1] + self.row_h // 2)
        return self.ports.get(name)

    def new_name(self) -> str:
        name = f"{BUFFER_PREFIX}{self.seq}"
        self.seq += 1
        return name

    def items_of(self, net: str) -> tuple[list, list]:
        """(movable items, fixed output ports) of a net."""
        movable, fixed = [], []
        for inst, pin in sorted(self.n.sinks.get(net, [])):
            master = self.n.instances[inst][0]
            pos = self.pos_of(f"{inst}/{pin}")
            if pos is None:
                continue
            movable.append(Item(f"{inst}/{pin}", pos, self.prof.pin_cap(master, pin), None))
        for port in sorted(self.n.port_alias.get(net, [])):
            pos = self.ports.get(port)
            if pos is not None:
                movable.append(Item(port, pos, OUTPUT_PORT_LOAD_PF, None, port=True))
        if net in self.n.outputs:
            fixed.append(net)
        return movable, fixed

    def emit(self, net_items: list, cell: Buffer) -> str:
        name = self.new_name()
        self.ops.append(dict(op="buffer_sinks", cell=cell.name, instance=name, net=f"{name}_n",
                             sinks=[dict(instance=i.name.rsplit("/", 1)[0], pin=i.name.rsplit("/", 1)[1])
                                    for i in net_items if not i.port],
                             **({"ports": [i.name for i in net_items if i.port]} if any(i.port for i in net_items) else {})))
        self.added_width += cell.width
        return name

    def buffer_net(self, net: str, why: str, items: list, fixed: list, total_pf: float, cw: float,
                   driver_slew_target: float, cap_limit: float, driver_slope: float = 0.0,
                   driver_cap_pf: float | None = None, model: DriverModel | None = None,
                   kept_pf: float = 0.0) -> dict:
        """Clusters -> level-1 buffers -> (level 2 if the driver load stays
        too big). With a driver `model` the buffers are co-sized with the
        driver (`arrange`: at their clusters, next to the driver, or behind
        one root buffer next to the driver; a larger driver member if
        needed). Otherwise `driver_cap_pf`: the load the driver can take
        within its transition target (None: unbounded, e.g. an input port);
        the buffer input capacitances share it."""
        prof = self.prof
        target = driver_slew_target
        big = prof.buffers[-1]
        limit = min(big.max_load(target), cap_limit)
        driver = self.n.driver.get(net, "")
        driver_pos = self.pos_of(driver) or centroid(items)
        # Clusters as large as the strongest buffer can drive within the
        # target (fewest buffer inputs on the driver); each cluster's buffer
        # is then the fastest one whose input capacitance fits the driver's
        # remaining budget.
        groups = bisect_clusters(items, limit, cw, MAX_CLUSTER_FANOUT, self.budget)
        loads = [sum(i.cap for i in g) + cw * hpwl([i.pos for i in g]) for g in groups]
        fixed_pts = [self.ports[p] for p in fixed if p in self.ports]
        trunk_est = cw * hpwl([driver_pos] + [centroid(g) for g in groups] + fixed_pts)
        fixed_pf = OUTPUT_PORT_LOAD_PF * len(fixed)
        per_group = None
        if driver_cap_pf is not None:
            per_group = (driver_cap_pf - kept_pf - trunk_est - fixed_pf) / len(groups)
            if per_group < prof.buffers[0].cap:
                per_group = None  # over budget anyway: the driver is resized next
        arr = dict(mode="cluster", member=None, loads=loads,
                   picks=[pick_buffer(prof, load, target, driver_slope, per_group) for load in loads])
        if model is not None and self.pos_of(driver) is not None:
            # The plain arrangement relies on the driver taking its load, if
            # need be after the whatif sizing grows it. A weak driver (even
            # its best member cannot take it: the largest member, a delay
            # cell, a family slow at any load) gets the co-sized arrangement.
            plain = sum(b.cap for b in arr["picks"]) + trunk_est + fixed_pf + kept_pf
            best = max((c for _, c in model.members if c is not None), default=None)
            if best is not None and plain > best:
                # New trees keep the driver's size unless it is headroom-
                # limited (only the largest member reaches the limit at all):
                # resizes are left to the whatif sizing, which verifies them.
                if not model.headroom:
                    model = DriverModel(model.members[:1], model.target, model.headroom, model.slope,
                                        model.degradation)
                co = self.arrange(groups, loads, driver_pos, trunk_est, fixed_pf + kept_pf, cw, target, model)
                if co is not None:
                    arr = co
        at_driver = arr["mode"] == "driver"
        level, made, cost, under = [], [], {}, {}
        for g, b, load in zip(groups, arr["picks"], arr["loads"]):
            inst = self.emit(g, b)
            pin = f"{inst}/{b.input_pin}"
            if at_driver:
                self.place_at(inst, b.width, driver_pos)
            level.append(Item(pin, driver_pos if at_driver else centroid(g), b.cap,
                              min((i.slack for i in g if i.slack is not None), default=None)))
            under[pin] = [i.name for i in g]
            for i in g:
                cost[i.name] = b.delay_at(load) + SLEW_SENSITIVITY * b.slew_at(load)
            made.append(dict(instance=inst, cell=b.name, items=len(g), load_pf=round(load, 5),
                             slew_ns=round(b.slew_at(load), 4), **({"at_driver": True} if at_driver else {})))
        levels = 1
        if arr["mode"] == "root":
            root, rl = arr["root"], arr["root_load"]
            inst = self.emit(level, root)
            self.place_at(inst, root.width, driver_pos)
            pin = f"{inst}/{root.input_pin}"
            under[pin] = [x for i in level for x in under.get(i.name, [i.name])]
            for x in under[pin]:
                cost[x] = cost.get(x, 0.0) + root.delay_at(rl) + SLEW_SENSITIVITY * root.slew_at(rl)
            made.append(dict(instance=inst, cell=root.name, items=len(level), load_pf=round(rl, 5),
                             slew_ns=round(root.slew_at(rl), 4), level=2, at_driver=True))
            level, levels = [Item(pin, driver_pos, root.cap, None)], 2
        while len(level) > MAX_CLUSTER_FANOUT and levels < MAX_TREE_LEVELS:
            groups = bisect_clusters(level, limit, cw, MAX_CLUSTER_FANOUT, self.budget)
            nxt = []
            for g in groups:
                if len(g) == 1:
                    nxt.append(g[0])
                    continue
                load = sum(i.cap for i in g) + cw * hpwl([i.pos for i in g])
                b = pick_buffer(prof, load, target, driver_slope, per_group)
                inst = self.emit(g, b)
                pin = f"{inst}/{b.input_pin}"
                nxt.append(Item(pin, centroid(g), b.cap, None))
                under[pin] = [x for i in g for x in under.get(i.name, [i.name])]
                for x in under[pin]:
                    cost[x] = cost.get(x, 0.0) + b.delay_at(load)
                made.append(dict(instance=inst, cell=b.name, items=len(g), load_pf=round(load, 5), level=levels + 1))
            level, levels = nxt, levels + 1
        trunk = [driver_pos] + [i.pos for i in level] + [self.ports[p] for p in fixed if p in self.ports]
        new_load = sum(i.cap for i in level) + OUTPUT_PORT_LOAD_PF * len(fixed) + cw * hpwl(trunk)
        if arr["mode"] in ("driver", "root"):
            new_load += self.near_pf(cw) * len(level)
            self.weights[net] = DRIVER_NET_WEIGHT
        rec = dict(net=net, why=why, items=len(items), load_before_pf=round(total_pf, 5),
                   driver_load_after_pf=round(new_load, 5), buffers=made, levels=levels, arrangement=arr["mode"])
        if model is not None:
            rec["driver_target_ns"] = round(model.target, 4)
            if model.headroom:
                rec["driver_headroom"] = True
            if model.degradation > 0:
                rec["input_slew_degradation_ns"] = round(model.degradation, 4)
        member = arr.get("member")
        if member and "/" in driver:
            dinst = driver.rsplit("/", 1)[0]
            if member != self.n.instances[dinst][0]:
                self.resizes[dinst] = (member, "co-sized with its buffers")
                rec["driver_resized_to"] = member
            self.touched.add(dinst)
        self.records.append(rec)
        self.costs = cost
        return rec

    def mark(self) -> tuple:
        return (len(self.ops), self.seq, self.added_width, len(self.records), dict(self.positions),
                dict(self.weights), dict(self.resizes), set(self.touched))

    def rollback(self, m: tuple):
        del self.ops[m[0]:]
        self.seq, self.added_width = m[1], m[2]
        del self.records[m[3]:]
        self.positions, self.weights, self.resizes, self.touched = m[4], m[5], m[6], m[7]

    def guarded(self, net: str, why: str, cand: list, kept: list, fixed: list, total: float, cw: float,
                target: float, cap_limit: float, slope: float, drv_cap: float | None, delay, slew,
                model: DriverModel | None = None) -> dict | None:
        """buffer_net, keeping on the driver every sink whose estimated
        setup slack after the edit would drop below SETUP_GUARD_NS.

        Estimate per moved sink: driver delay at its new load + the buffer
        delay(s) + SLEW_SENSITIVITY x the buffer's output slew, against the
        driver delay + SLEW_SENSITIVITY x slew at today's load (`delay`,
        `slew`: the driver's load models). At most three passes."""
        # Measured on the GF180 designs: a buffer in front of a sink with
        # less than CRITICAL_SLACK_NS of setup slack lost more than the
        # estimate promised; such sinks never move.
        def room(i):
            return None if i.slack is None else i.slack - self.consumed.get(i.name, 0.0)
        kept = kept + [i for i in cand if room(i) is not None and room(i) < CRITICAL_SLACK_NS]
        moving = [i for i in cand if i not in kept]
        base = delay(total) + SLEW_SENSITIVITY * slew(total)
        for _ in range(3):
            if not moving:
                return None
            m = self.mark()
            rec = self.buffer_net(net, why, moving, fixed, total, cw, target, cap_limit, slope, drv_cap, model,
                                  sum(i.cap for i in kept))
            after = rec["driver_load_after_pf"] + sum(i.cap for i in kept)
            bad, spend = [], {}
            for i in moving:
                d = delay(after) + self.costs.get(i.name, 0.0) - base
                spend[i.name] = d
                if d > 0 and room(i) is not None and room(i) - d < SETUP_GUARD_NS:
                    bad.append(i)
            if not bad:
                rec["kept_on_driver"] = [i.name for i in kept]
                rec["estimated_driver_delay_ns"] = dict(before=round(delay(total), 4), after=round(delay(after), 4))
                for name, d in spend.items():
                    if d > 0:
                        self.consume(name, self.consumed.get(name, 0.0) + d)
                return rec
            self.rollback(m)
            kept = kept + bad
            moving = [i for i in moving if i not in bad]
        return None

    def rebalance(self, net: str, items: list, fixed: list, total: float, cw: float, target: float,
                  model: DriverModel, out_load: dict, observed: float | None = None,
                  slew_limit: float | None = None) -> dict | None:
        """A violating net whose sinks are all ECO buffers and whose driver
        cannot take its load: co-size the existing tree with its driver.
        Options, first feasible wins (current driver member, then larger):
        `driver` -- every buffer moves next to the driver (placement seed;
        the trunk wire moves to the buffer outputs) and is re-sized within
        its family for its own load plus that trunk, its input capacitance
        within the driver's share; `root` -- one new buffer next to the
        driver in front of all of them (it drives the existing trunk).
        Setup guard as for new buffers: an extra delay in front of a buffer
        input pin with less than CRITICAL_SLACK_NS of setup slack, or one that
        leaves less than SETUP_GUARD_NS, blocks
        the option. When the buffers already sit next to the driver, the
        observed wire of the net (routed: pin access, couplings) replaces the
        near-wire estimate. Nothing feasible at the target: `best effort` --
        the root buffer whose input gives the driver the lowest model
        transition, if that is below BEST_EFFORT x the limit and at least
        BEST_EFFORT_GAIN_NS below the observed one. Returns the record,
        {failed: [...]}, or None (no data)."""
        driver = self.n.driver[net]
        driver_pos = self.pos_of(driver)
        if driver_pos is None or "/" not in driver:
            return None
        prof = self.prof
        near = self.near_pf(cw)
        target = buffer_target(target, model)
        fixed_pf = OUTPUT_PORT_LOAD_PF * len(fixed)
        bufs = []
        for it in items:
            inst = it.name.rsplit("/", 1)[0]
            master, conns = self.n.instances[inst]
            outs = [f"{inst}/{p}" for p, x in conns.items() if self.n.driver.get(x) == f"{inst}/{p}"]
            old = next((b for b in prof.buffers if b.name == master), None)
            if len(outs) != 1 or outs[0] not in out_load or old is None:
                return None
            bufs.append((inst, it, old, out_load[outs[0]], out_load[outs[0]] + cw * manhattan(driver_pos, it.pos)))
        wire = total - fixed_pf - sum(b[2].cap for b in bufs)
        dw = self.widths.get(self.n.instances[driver.rsplit("/", 1)[0]][0], 0)
        if all(manhattan(driver_pos, b[1].pos) <= (dw + b[2].width) // 2 + 2 * NEAR_ROWS * self.row_h for b in bufs):
            # Already next to the driver: moving them changes nothing, and
            # the routed wire is what a new root would see as well.
            near = max(near, wire / len(bufs))
            bufs = [(i, it, old, l0, l0) for i, it, old, l0, _ in bufs]

        def room(i):
            return None if i.slack is None else i.slack - self.consumed.get(i.name, 0.0)
        order = []
        for k in range(len(model.members)):
            order += [("driver", k), ("root", k)]
        tried = []
        for mode, k in order:
            member, cap = model.members[k]
            if cap is None:
                continue
            avail = cap - fixed_pf
            if mode == "driver":
                picks = []
                for inst, it, old, load0, load1 in bufs:
                    fam = [b for b in prof.buffers if prof.same_family(b.name, old.name)]
                    p = fit_buffers(fam, [load1], target, model.slope, (avail - near * len(bufs)) / len(bufs),
                                    model.headroom)
                    if not p:
                        break
                    picks.append(p[0])
                if len(picks) != len(bufs):
                    tried.append(f"{mode}@{member}: no buffer of the family fits")
                    continue
                if sum(b.cap for b in picks) + fixed_pf + near * len(bufs) > REBALANCE_RELIEF * total:
                    tried.append(f"{mode}@{member}: no relief (the buffers are next to the driver already)")
                    continue
                spend = {}
                for (inst, it, old, load0, load1), b in zip(bufs, picks):
                    spend[it.name] = (b.delay_at(load1) + SLEW_SENSITIVITY * b.slew_at(load1)) - \
                        (old.delay_at(load0) + SLEW_SENSITIVITY * old.slew_at(load0))
            else:
                root_load = total - fixed_pf
                r = fit_buffers(prof.buffers, [root_load], target, model.slope, avail - near, model.headroom)
                if not r:
                    tried.append(f"{mode}@{member}: no root buffer fits")
                    continue
                root = r[0]
                if root.cap + fixed_pf + near > REBALANCE_RELIEF * total:
                    tried.append(f"{mode}@{member}: no relief (a root next to the driver sees the same load)")
                    continue
                spend = {it.name: root.delay_at(root_load) + SLEW_SENSITIVITY * root.slew_at(root_load)
                         for _, it, _, _, _ in bufs}
            # Same rule as for new buffers: no extra delay in front of a sink
            # with less than CRITICAL_SLACK_NS, and SETUP_GUARD_NS left after it.
            item = {i.name: i for _, i, _, _, _ in bufs}
            blocked = [n for n, d in spend.items() if d > 0 and room(item[n]) is not None and
                       (room(item[n]) < CRITICAL_SLACK_NS or room(item[n]) - d < SETUP_GUARD_NS)]
            if blocked:
                tried.append(f"{mode}@{member}: setup guard ({len(blocked)} buffers)")
                continue
            return self.rebalanced(net, mode, member, items, bufs, picks if mode == "driver" else None,
                                   root if mode == "root" else None, root_load if mode == "root" else None,
                                   spend, near, fixed_pf, total, model, tried, driver, driver_pos)
        if observed is not None and slew_limit is not None:
            # Best effort (headroom-limited drivers: GF180 aoi22 with a routed
            # wire of 4-6 fF even next to its buffer): the smallest load a root
            # buffer can present.
            best = None
            root_load = total - fixed_pf
            for member, cap in model.members:
                for b in prof.buffers:
                    if b.slew_at(root_load) > target:
                        continue
                    pred = prof.slew_at(member, b.cap + near + fixed_pf)
                    if pred is None:
                        continue
                    pred += model.degradation
                    key_ = (round(pred, 6), b.cap, b.name, member)
                    if best is None or key_ < best[0]:
                        best = (key_, member, b, pred)
            if best is not None and best[3] <= BEST_EFFORT * slew_limit and best[3] <= observed - BEST_EFFORT_GAIN_NS:
                _, member, root, pred = best
                spend = {it.name: root.delay_at(root_load) + SLEW_SENSITIVITY * root.slew_at(root_load)
                         for _, it, _, _, _ in bufs}
                item = {i.name: i for _, i, _, _, _ in bufs}
                if not any(d > 0 and room(item[n]) is not None and
                           (room(item[n]) < CRITICAL_SLACK_NS or room(item[n]) - d < SETUP_GUARD_NS)
                           for n, d in spend.items()):
                    tried.append(f"best effort: root {root.name}, model transition {pred:.3f} ns (observed {observed:.3f})")
                    return self.rebalanced(net, "root", member, items, bufs, None, root, root_load, spend, near,
                                           fixed_pf, total, model, tried, driver, driver_pos)
                tried.append("best effort: setup guard")
        return dict(failed=tried)

    def rebalanced(self, net, mode, member, items, bufs, picks, root, root_load, spend, near, fixed_pf, total,
                   model, tried, driver, driver_pos) -> dict:
        """Emit one rebalance decision (see `rebalance`)."""
        rec = dict(net=net, why="drv:rebalance ECO buffers", items=len(items), load_before_pf=round(total, 5),
                   arrangement=mode, driver_target_ns=round(model.target, 4), tried=tried)
        if mode == "driver":
            resized = {}
            for (inst, it, old, load0, load1), b in zip(bufs, picks):
                if b.name != old.name:
                    self.resizes[inst] = (b.name, "co-sized next to its driver")
                    resized[inst] = b.name
                self.place_at(inst, b.width, driver_pos)
            rec.update(moved=[inst for inst, *_ in bufs], resized=resized,
                       driver_load_after_pf=round(sum(b.cap for b in picks) + fixed_pf + near * len(bufs), 5))
        else:
            inst = self.emit(items, root)
            self.place_at(inst, root.width, driver_pos)
            rec.update(root=dict(instance=inst, cell=root.name, load_pf=round(root_load, 5),
                                 slew_ns=round(root.slew_at(root_load), 4)),
                       driver_load_after_pf=round(root.cap + fixed_pf + near, 5))
        for name, d in spend.items():
            if d > 0:
                self.consume(name, self.consumed.get(name, 0.0) + d)
        self.weights[net] = DRIVER_NET_WEIGHT
        dinst = driver.rsplit("/", 1)[0]
        if member != self.n.instances[dinst][0]:
            self.resizes[dinst] = (member, "co-sized with its buffers")
            rec["driver_resized_to"] = member
        self.touched.add(dinst)
        self.touched.update(inst for inst, *_ in bufs)
        self.records.append(rec)
        return rec


def hold_capacity(hold_drive: dict | None, master: str, target: float) -> float | None:
    """Load a delay cell may drive within `target`: its profile is
    characterized at one load only; the slew is taken as proportional to the
    load through that point (an upper bound of a line with a positive
    intercept, so the capacity is conservative)."""
    if not hold_drive or master not in hold_drive:
        return None
    load, slew = hold_drive[master]
    return load * target / slew if slew > 0 else None


def driver_model(prof: Profile, master: str, slew_limit: float, max_width: int | None, near_pf: float,
                 observed: float | None, total: float, hold_drive: dict | None) -> DriverModel | None:
    """What the driver of a planned net can take (see DriverModel). Target:
    DRIVER_TARGET x limit, or the headroom target when even the best member
    (current or up to two sizes larger) misses DRIVER_TARGET at the smallest
    arrangement load (smallest buffer input + `near_pf`); minus the observed
    input-slew degradation (observed transition above the model at the
    observed load, at most MAX_SLEW_DEGRADATION x limit). Hold delay cells:
    one member, `hold_capacity`. None: no drive data."""
    d = prof.drive.get(master)
    if d and "slew_ns" in d:
        members = prof.members_up(master, max_width)
        floor_load = prof.buffers[0].cap + near_pf
        floor = min(prof.slew_at(m, floor_load) for m in members)
        target, headroom = DRIVER_TARGET * slew_limit, False
        if target < floor < slew_limit:
            target, headroom = floor + DRIVER_HEADROOM * (slew_limit - floor), True
        degradation = 0.0
        if observed is not None:
            degradation = min(MAX_SLEW_DEGRADATION * slew_limit, max(0.0, observed - prof.slew_at(master, total)))
        target -= degradation
        return DriverModel([(m, prof.capacity(m, target)) for m in members], target, headroom,
                           prof.delay_slope(master), degradation)
    target = DRIVER_TARGET * slew_limit
    cap = hold_capacity(hold_drive, master, target)
    if cap is None:
        return None
    if observed and total > 0:
        cap = min(cap, total * target / observed)
    return DriverModel([(master, cap)], target, False, 0.5 * observed / total if observed and total > 0 else 0.0)


def design_wire_cap(n: Netlist, pl, prof: Profile, out_load: dict) -> float:
    """Median wire capacitance per DBU over the layout's nets (driver pin
    load minus sink pin capacitances, over the HPWL of driver and sinks)."""
    vals = []
    for net, sinks in sorted(n.sinks.items()):
        drv = n.driver.get(net)
        if not drv or drv not in out_load or "/" not in drv:
            continue
        pts = [pl.pos_of(f"{i}/{p}") for i, p in sinks] + [pl.pos_of(drv)]
        if any(x is None for x in pts) or len(pts) < 2:
            continue
        span = hpwl(pts)
        wire = out_load[drv] - sum(prof.pin_cap(n.instances[i][0], p) for i, p in sinks)
        if span > 0 and wire > 0:
            vals.append(wire / span)
    vals.sort()
    return vals[len(vals) // 2] if vals else 0.0


def plan_buffering(netlist_text: str, lef: dict, def_text: str, profile: dict, drv_rows: list, pins: dict,
                   corner: str, max_width: int | None, seq: int, row_h: int, widths: dict,
                   setup_critical: bool, frozen: set | frozenset = frozenset(),
                   max_added_width: int | None = None, hold_drive: dict | None = None, spacing: int = 0,
                   sizing_failed: set | frozenset = frozenset()) -> dict:
    """Plan buffers (and driver resizes) for one layout.

    drv_rows: RustSTA drv rows (every corner); pins: corner -> {pin: setup
    slack} (pins reports); corner: the planning corner (drive data, worst
    corner); frozen: `inst/pin` sinks another ECO of the same iteration
    already moves (the hold ECO's chains), never moved here;
    max_added_width: the buffer width (DBU) this layout may still take (the
    loop's utilization cap), a net whose buffers would exceed it is skipped;
    hold_drive: delay-cell master -> (characterized load pF, worst output
    transition ns there) from the delay-cell profile (hold cells have no
    sizing family; `hold_capacity`); sizing_failed: small-fanout nets the
    previous layout left to the whatif sizing (buffered now if they still
    violate).
    Returns {ops, resizes, records, skipped, added_width_dbu, next_seq,
    positions (instance -> lower-left DBU seed of the next placement),
    weights (net -> placement weight), touched (instances decided here)}.
    """
    n = parse_netlist(netlist_text, lef)
    comps, ports = def_positions(def_text)
    prof = Profile(profile, corner, max_width)
    if not prof.buffers:
        return dict(ops=[], resizes={}, records=[], skipped={"*": "no buffer with drive data fits the tap gap"},
                    added_width_dbu=0, next_seq=seq, nets_with_rows=0, positions={}, weights={}, touched=[])
    pl = Planner(n, comps, ports, prof, widths, row_h, seq)
    pl.sequential = {c["name"] for f in profile["families"] if f.get("sequential") for c in f["cells"]}
    # A moved sink gets at least one buffer delay: only sinks with that much
    # setup room (plus CRITICAL_SLACK_NS) move.
    critical = prof.buffers[0].delay_at(0.02) + CRITICAL_SLACK_NS
    slack = pins.get(corner) or {}
    other = [pins[c] for c in sorted(pins) if c != corner]

    def setup_slack(pin):
        vals = [s for s in [slack.get(pin)] + [o.get(pin) for o in other] if s is not None]
        return min(vals) if vals else None

    # Map a report pin to the edited netlist's net.
    def net_of(pin: str) -> str | None:
        if "/" in pin:
            inst, p = pin.rsplit("/", 1)
            conns = n.instances.get(inst, (None, {}))[1]
            return conns.get(p)
        if pin in n.inputs:
            return pin
        for net, aliases in n.port_alias.items():
            if pin in aliases:
                return net
        return pin if pin in n.outputs else None

    nets: dict[str, dict] = {}
    for r in drv_rows:
        net = net_of(key(r["pin"]))
        if net is None or net not in n.driver:
            continue
        drv = n.driver[net]
        if "/" in drv and drv.rsplit("/", 1)[0] not in n.instances:
            continue
        e = nets.setdefault(net, dict(kinds={}, cap={}, slew={}, drv_slew={}, fanout=0))
        if r["kind"] == "max_capacitance" and key(r["pin"]) == drv:
            e["cap"][r["corner"]] = max(e["cap"].get(r["corner"], 0.0), r["value"])
            e["cap_limit"] = r["limit"]
        if r["kind"] == "max_transition":
            e["slew"][r["corner"]] = max(e["slew"].get(r["corner"], 0.0), r["value"])
            e["slew_limit"] = r["limit"]
            if key(r["pin"]) == drv:
                e["drv_slew"][r["corner"]] = r["value"]
        if r["kind"] == "max_fanout":
            e["fanout"] = max(e["fanout"], int(r["value"]))
            e["fanout_limit"] = r["limit"]
        # Planning margin: a transition/capacitance within DRV_PLAN_MARGIN of
        # its limit is repaired too (the next layout moves every wire).
        margin = DRV_PLAN_MARGIN if r["kind"] in ("max_transition", "max_capacitance") else 1.0
        if r["value"] > margin * r["limit"]:
            e["kinds"][r["kind"]] = min(e["kinds"].get(r["kind"], 0.0), margin * r["limit"] - r["value"])
    design_cap_limit = max((r["limit"] for r in drv_rows if r["kind"] == "max_capacitance"), default=0.3)
    # Load of every driver pin at the planning corner (max_capacitance rows
    # carry the driver pin and its net's total load).
    out_load = {key(r["pin"]): r["value"] for r in drv_rows if r["kind"] == "max_capacitance" and r["corner"] == corner}
    pl.cw_design = design_wire_cap(n, pl, prof, out_load)
    pl.occupancy = Occupancy(def_text, widths, spacing)
    skipped, library_limited = {}, {}
    for net in sorted(nets, key=lambda k: (min(nets[k]["kinds"].values(), default=0.0), k)):
        e = nets[net]
        violated = bool(e["kinds"])
        drv = n.driver[net]
        is_port = "/" not in drv
        inst = None if is_port else drv.rsplit("/", 1)[0]
        master = None if is_port else n.instances[inst][0]
        items, fixed = pl.items_of(net)
        items = [i for i in items if i.name not in frozen]
        for i in items:
            i.slack = setup_slack(i.name)
        if len(items) > MAX_NET_ITEMS:
            skipped[net] = f"{len(items)} sinks exceed {MAX_NET_ITEMS}"
            continue
        total = e["cap"].get(corner) or (sum(i.cap for i in items) + OUTPUT_PORT_LOAD_PF * len(fixed))
        pts = [i.pos for i in items]
        if pl.pos_of(drv) is not None:
            pts.append(pl.pos_of(drv))
        span = max(hpwl(pts), 1)
        wire = max(0.0, total - sum(i.cap for i in items) - OUTPUT_PORT_LOAD_PF * len(fixed))
        cw = wire / span
        slew_limit = e.get("slew_limit", 1.0)
        # A buffer output is bounded by the design-level capacitance limit
        # (the largest limit any row states), not by the driver pin's own.
        cap_limit = design_cap_limit
        target = TRANSITION_TARGET * slew_limit
        if len(pl.ops) >= MAX_BUFFER_OPS:
            skipped[net] = "buffer budget per iteration reached"
            continue
        slope = 0.0 if is_port else prof.delay_slope(master)
        obs = e["drv_slew"].get(corner)
        drv_target = DRIVER_TARGET * slew_limit
        drv_cap = None
        if not is_port:
            d = prof.drive.get(master)
            if d and "slew_ns" in d:
                (c1, c2), (t1, t2) = prof.loads, d["slew_ns"]
                k = (t2 - t1) / (c2 - c1)
                drv_cap = c1 + (drv_target - t1) / k if k > 0 else None
            elif obs and total > 0:
                # No drive data (e.g. a hold delay cell): proportional to what
                # RustSTA observed at this load.
                drv_cap = total * drv_target / obs
                slope = 0.5 * obs / total
        if not is_port and prof.delay_at(master, total) is not None:
            def d_model(c, m=master):
                return prof.delay_at(m, c)

            def s_model(c, m=master):
                return prof.slew_at(m, c)
        else:
            # Ports and cells without drive data: proportional to the
            # observed transition (delay about half of it).
            o = obs or 0.1

            def d_model(c, o=o, t=max(total, 1e-6), port=is_port):
                return 0.0 if port else 0.5 * o * c / t

            def s_model(c, o=o, t=max(total, 1e-6)):
                return o * c / t
        dmodel = None if is_port else driver_model(prof, master, slew_limit, max_width, pl.near_pf(cw), obs, total,
                                                    hold_drive)
        if violated:
            kinds = set(e["kinds"])
            if not is_port and "max_transition" in kinds and master in prof.family:
                floor = prof.best_slew(master, prof.loads[0], max_width)
                if floor is not None and floor > slew_limit:
                    # No member of the family meets the limit even at the
                    # smallest characterized load: only a different cell
                    # (synthesis) or another limit can fix this net.
                    library_limited[net] = dict(driver=drv, master=master, min_family_slew_ns=round(floor, 4),
                                                load_pf=prof.loads[0], limit_ns=slew_limit, corner=corner)
                    skipped[net] = f"library-limited: no member of {master}'s family reaches {slew_limit} ns at {corner}"
                    continue
            if not items:
                skipped[net] = "no movable sink"
                continue
            model = None if is_port else prof.slew_at(master, total)
            if kinds == {"max_transition"} and model is not None and model <= DRIVER_TARGET * slew_limit:
                # The driver meets the limit at its own load with a clean
                # input: the transition comes from upstream (repaired there).
                skipped[net] = (f"input-slew propagated: {master} drives {total:.4f} pF in {model:.3f} ns "
                                f"(observed {obs})")
                continue
            if not is_port and kinds == {"max_transition"} and len(items) + len(fixed) <= 2 \
                    and not inst.startswith(HOLD_PREFIX):
                # Small fanout: a larger driver fixes it when one exists (the
                # sizing step decides with RustSTA whatif); a buffer only when
                # no family member meets the target at this load.
                if prof.upsize_for(master, total, DRIVER_TARGET * slew_limit, max_width) and net not in sizing_failed:
                    skipped[net] = "fanout <= 2: left to whatif sizing"
                    continue
            # Setup-critical sinks never go behind a buffer (it would add its
            # delay to their path); only the others move. A net whose sinks
            # are all critical is left to sizing.
            if not (kinds & {"max_capacitance", "max_fanout"}) and \
                    all(i.name.startswith(BUFFER_PREFIX) for i in items):
                # Already a buffer tree. If the driver can take the load, it is
                # resized (whatif sizing); if not (the largest member, a hold
                # cell, too much wire), the tree is co-sized with its driver:
                # buffers next to the driver, or one root buffer there.
                cap_now = dmodel.members[0][1] if dmodel is not None else None
                violating = max(e["slew"].values(), default=0.0) > slew_limit or \
                    max(e["cap"].values(), default=0.0) > e.get("cap_limit", float("inf"))
                if dmodel is None or cap_now is None or total <= cap_now or not violating:
                    skipped[net] = "sinks are ECO buffers already: left to sizing"
                    continue
                m = pl.mark()
                rec = pl.rebalance(net, items, fixed, total, cw, target, dmodel, out_load,
                                   max(e["slew"].values(), default=None), slew_limit)
                if rec is not None and "failed" not in rec and max_added_width is not None and \
                        pl.added_width > max_added_width:
                    pl.rollback(m)
                    skipped[net] = "area budget of this layout reached (utilization cap)"
                elif rec is None or "failed" in rec:
                    skipped[net] = "sinks are ECO buffers already; no co-sized arrangement: " + \
                        "; ".join((rec or {}).get("failed") or ["no position/load data"])
                continue
            m = pl.mark()
            rec = pl.guarded(net, "drv:" + "/".join(sorted(kinds)), items, [], fixed, total, cw, target,
                             CAPACITANCE_TARGET * cap_limit, slope, drv_cap, d_model, s_model, dmodel)
            if rec is not None and max_added_width is not None and pl.added_width > max_added_width:
                pl.rollback(m)
                skipped[net] = "area budget of this layout reached (utilization cap)"
                continue
            if rec is None:
                skipped[net] = f"every sink of {len(items)} would lose setup behind a buffer: left to sizing"
                continue
            if not is_port:
                after = rec["driver_load_after_pf"] + sum(i.cap for i in items if i.name in rec["kept_on_driver"])
                s_after = prof.slew_at(master, after)
                rec["driver_slew_after_ns"] = None if s_after is None else round(s_after, 4)
        elif setup_critical and not is_port and len(items) >= ISOLATION_MIN_FANOUT:
            vals = [i.slack for i in items if i.slack is not None]
            if not vals or min(vals) >= 0:
                continue
            worst = min(vals)
            rest = [i for i in items if i.slack is not None and i.slack >= max(worst + ISOLATION_MARGIN_NS, critical)]
            if len(rest) < 2 or sum(i.cap for i in rest) < 0.25 * total:
                continue
            kept = [i for i in items if i not in rest]
            m = pl.mark()
            rec = pl.guarded(net, "setup:critical-sink isolation", rest, kept, [], total, cw, target,
                             CAPACITANCE_TARGET * cap_limit, slope, drv_cap, d_model, s_model)
            if rec is not None and max_added_width is not None and pl.added_width > max_added_width:
                pl.rollback(m)
                skipped[net] = "area budget of this layout reached (utilization cap)"
    return dict(ops=pl.ops, resizes=pl.resizes, records=pl.records, skipped=skipped, library_limited=library_limited,
                added_width_dbu=pl.added_width, next_seq=pl.seq, nets_with_rows=len(nets),
                positions=dict(sorted(pl.positions.items())), weights=dict(sorted(pl.weights.items())),
                touched=sorted(pl.touched))


def layout_wire_cap(netlist_text: str, lef: dict, def_text: str, profile: dict, drv_rows: list, corner: str,
                    widths: dict, row_h: int) -> float:
    """Median wire capacitance per DBU of a timed layout (design_wire_cap)."""
    n = parse_netlist(netlist_text, lef)
    comps, ports = def_positions(def_text)
    prof = Profile(profile, corner, None)
    pl = Planner(n, comps, ports, prof, widths, row_h, 0)
    out_load = {key(r["pin"]): r["value"] for r in drv_rows if r["kind"] == "max_capacitance" and r["corner"] == corner}
    return design_wire_cap(n, pl, prof, out_load)


def plan_hold_isolation(netlist_text: str, lef: dict, def_text: str, profile: dict, points: list, hold_drive: dict,
                        slew_limit: float, seq: int, row_h: int, widths: dict, cw: float, setup_margin: float,
                        max_width: int | None = None, corners=("ss", "tt"), spacing: int = 0) -> dict:
    """DRV-aware hold insertion: a buffer behind a hold chain whose sink-side
    delay cell cannot drive the point's load.

    netlist_text: the hold ECO's output; points: the hold plan's points
    (path-aware plan: name, sinks, cells, instances, setup_slack_ns,
    added_setup_ns per corner); hold_drive: delay-cell master ->
    (characterized load pF, worst output transition ns) at the planning
    corner (`corners[0]`); cw: wire capacitance per DBU (design median).
    The sink-side cell (`instances[0]`, output net `<inst>_n`) drives the
    point's sinks: load = their pin capacitances + cw x HPWL(sinks) + a
    near-wire term. Above `hold_capacity` at DRIVER_TARGET x the limit, the
    sinks move behind one sizing-profile buffer (`buffer_sinks`) chosen to
    meet TRANSITION_TARGET at that load with its input within the delay
    cell's capacity, and only if every corner in `corners` keeps
    `setup_margin` + SETUP_GUARD_NS of the point's setup slack after the
    chain and the buffer (both estimates). The chain and the buffer get
    placement seeds at the sinks' centroid (the strong upstream driver takes
    the long wire, the weak delay cell drives only the buffer next to it) and
    the delay cell's net a placement weight. Returns {ops, positions,
    weights, records, skipped, next_seq}."""
    n = parse_netlist(netlist_text, lef)
    comps, ports = def_positions(def_text)
    profs = {c: Profile(profile, c, max_width) for c in corners}
    prof = profs[corners[0]]
    out = dict(ops=[], positions={}, weights={}, records=[], skipped={}, next_seq=seq)
    if not prof.buffers:
        out["skipped"]["*"] = "no buffer with drive data fits the tap gap"
        return out
    near = cw * NEAR_ROWS * row_h
    target = DRIVER_TARGET * slew_limit
    occ = Occupancy(def_text, widths, spacing)
    spot_budget = [MAX_CONSUME_VISITS]

    def centre(inst):
        p = comps.get(inst)
        if p is None:
            return None
        return (p[0] + widths.get(n.instances[inst][0], 0) // 2, p[1] + row_h // 2)
    for pt in points:
        insts = pt.get("instances") or []
        if not insts or insts[0] not in n.instances:
            continue
        first = insts[0]
        master = n.instances[first][0]
        net = key(f"{first}_n")
        # A chain cell with Liberty drive data (a logic buffer used for hold,
        # sky130 buf_1) uses it; delay cells the one characterized point.
        cap = prof.capacity(master, target) if master in prof.drive else hold_capacity(hold_drive, master, target)
        sinks = sorted(n.sinks.get(net, []))
        if cap is None or not sinks or n.driver.get(net, "").rsplit("/", 1)[0] != first:
            out["skipped"][pt["name"]] = "no drive data, no sinks or not the chain's sink-side cell"
            continue
        pos = [centre(i) for i, _ in sinks]
        if any(x is None for x in pos):
            out["skipped"][pt["name"]] = "a sink has no position on this layout"
            continue
        pins_pf = sum(prof.pin_cap(n.instances[i][0], p) for i, p in sinks)
        load = pins_pf + cw * hpwl(pos) + near
        rec = dict(point=pt["name"], cell=master, instance=first, load_pf=round(load, 5),
                   capacity_pf=round(cap, 5))
        if load <= cap:
            continue
        pick = fit_buffers(prof.buffers, [load], TRANSITION_TARGET * slew_limit, 0.0, cap - near)
        if not pick:
            out["skipped"][pt["name"]] = f"no buffer meets the target within {cap - near:.4f} pF"
            continue
        buf = pick[0]
        rooms = {}
        for c in corners:
            d = profs[c].delay_at(buf.name, load)
            s = (pt.get("setup_slack_ns") or {}).get(c)
            if d is None or s is None:
                continue
            rooms[c] = round(s - (pt.get("added_setup_ns") or {}).get(c, 0.0) - d, 4)
        if any(r < setup_margin + SETUP_GUARD_NS for r in rooms.values()):
            out["skipped"][pt["name"]] = f"setup room after the chain and a buffer: {rooms}"
            continue
        name = f"{BUFFER_PREFIX}{seq}"
        seq += 1
        out["ops"].append(dict(op="buffer_sinks", cell=buf.name, instance=name, net=f"{name}_n",
                               sinks=[dict(instance=i, pin=p) for i, p in sinks]))
        c = (sum(x[0] for x in pos) // len(pos), sum(x[1] for x in pos) // len(pos))
        for inst, w in [(name, buf.width)] + [(i, widths.get(n.instances[i][0], 0)) for i in insts if i in n.instances]:
            out["positions"][inst] = occ.spot(c, w, inst, spot_budget) or (c[0] - w // 2, c[1] - row_h // 2)
        out["weights"][net] = DRIVER_NET_WEIGHT
        rec.update(buffer=name, buffer_cell=buf.name, buffer_slew_ns=round(buf.slew_at(load), 4),
                   setup_room_ns=rooms)
        out["records"].append(rec)
    out["next_seq"] = seq
    return out


def next_sequence(text: str, prefix: str = BUFFER_PREFIX) -> int:
    nums = [int(m) for m in re.findall(r"(?m)\b" + re.escape(prefix) + r"(\d+)", text)]
    return max(nums) + 1 if nums else 0


def net_weights(netlist_text: str, lef: dict, pins: dict, corners=("ss", "tt"), window_ns: float = WEIGHT_WINDOW_NS,
                max_weight: int = MAX_WEIGHT) -> dict:
    """rustroad.net-weights.v1 from setup slack of each net's driver pin:
    w = 1000 + (max_weight - 1000) * crit^2, crit = clamp((worst + window -
    slack) / window, 0, 1) at the worst of `corners`. Only nets of the edited
    netlist with an instance driver; integer milli-units, sorted keys."""
    n = parse_netlist(netlist_text, lef)
    slack = {}
    for c in corners:
        for p, s in (pins.get(c) or {}).items():
            if s is not None:
                slack[p] = min(slack.get(p, s), s)
    drv = {net: d for net, d in n.driver.items() if "/" in d}
    vals = [slack[d] for d in drv.values() if d in slack]
    if not vals:
        return dict(schema="rustroad.net-weights.v1", source=dict(rule="no driver slack"), weights={})
    worst = min(vals)
    weights = {}
    for net, d in sorted(drv.items()):
        s = slack.get(d)
        if s is None:
            continue
        crit = min(1.0, max(0.0, (worst + window_ns - s) / window_ns))
        if crit > 0:
            w = 1000 + int(round((max_weight - 1000) * crit * crit))
            if w > 1000:
                weights[net] = w
    return dict(schema="rustroad.net-weights.v1",
                source=dict(rule=f"1000 + {max_weight - 1000} * crit^2, crit = clamp((worst + {window_ns} - slack) / "
                                 f"{window_ns}, 0, 1) of the net driver's setup slack, worst of {list(corners)}",
                            worst_setup_ns=round(worst, 6)),
                weights=weights)


def write_json(path: Path, value) -> Path:
    path.write_text(json.dumps(value, indent=1, sort_keys=False) + "\n")
    return path
