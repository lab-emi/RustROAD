# SPDX-License-Identifier: Apache-2.0
"""Minimal, bounded Liberty group reader (clean-room, standard-format data only).

Only what the delay-cell profile needs: library units, lu_table_template
groups and selected `cell` groups parsed into nested dicts. Cell groups are
located by a top-level scan so a 20 MB library is not tokenized as a whole.
Unknown statements are retained verbatim; nothing is silently reinterpreted.
"""
from __future__ import annotations

import bisect
import re

MAX_LIBRARY_BYTES = 128 * 1024 * 1024
MAX_GROUP_TOKENS = 2_000_000

_TOKEN = re.compile(r'''\s+|/\*.*?\*/|//[^\n]*|\\\n|"(?:[^"\\]|\\.)*"|[(){}:;,]|[^\s(){}:;,"]+''', re.S)


class LibertyError(ValueError):
    pass


def tokens(text: str):
    out = []
    pos = 0
    for m in _TOKEN.finditer(text):
        if m.start() != pos:
            raise LibertyError(f"untokenizable Liberty text at offset {pos}")
        pos = m.end()
        t = m.group(0)
        if t.isspace() or t.startswith('/*') or t.startswith('//') or t == '\\\n':
            continue
        out.append(t)
        if len(out) > MAX_GROUP_TOKENS:
            raise LibertyError("Liberty group token budget exceeded")
    if pos != len(text):
        raise LibertyError("trailing untokenizable Liberty text")
    return out


def unquote(t: str) -> str:
    return t[1:-1] if len(t) >= 2 and t[0] == '"' and t[-1] == '"' else t


class Group:
    """`name (args) { attributes; groups }` with ordered children."""

    def __init__(self, kind: str, args: list[str]):
        self.kind, self.args = kind, args
        self.attrs: dict[str, str] = {}
        self.complex: dict[str, list[list[str]]] = {}
        self.groups: list[Group] = []

    @property
    def name(self) -> str:
        return unquote(self.args[0]) if self.args else ''

    def sub(self, kind: str, name: str | None = None) -> list['Group']:
        return [g for g in self.groups if g.kind == kind and (name is None or g.name == name)]

    def attr(self, key: str, default=None):
        v = self.attrs.get(key)
        return default if v is None else unquote(v)


def parse_group(toks: list[str], i: int = 0) -> tuple[Group, int]:
    kind = toks[i]
    if toks[i + 1] != '(':
        raise LibertyError(f"group {kind} without arguments")
    j = i + 2
    args = []
    while toks[j] != ')':
        if toks[j] != ',':
            args.append(toks[j])
        j += 1
    if toks[j + 1] != '{':
        raise LibertyError(f"group {kind} without body")
    g = Group(kind, args)
    j += 2
    while toks[j] != '}':
        key = toks[j]
        if toks[j + 1] == ':':
            k = j + 2
            value = []
            while toks[k] != ';':
                value.append(toks[k])
                k += 1
                if toks[k] in ('}', '{'):
                    raise LibertyError(f"unterminated attribute {key}")
            g.attrs[key] = ' '.join(value)
            j = k + 1
        elif toks[j + 1] == '(':
            k = j + 2
            cargs = []
            while toks[k] != ')':
                if toks[k] != ',':
                    cargs.append(unquote(toks[k]))
                k += 1
            if toks[k + 1] == '{':
                child, j = parse_group(toks, j)
                g.groups.append(child)
            else:
                g.complex.setdefault(key, []).append(cargs)
                j = k + 1
                if j < len(toks) and toks[j] == ';':
                    j += 1
        else:
            raise LibertyError(f"unexpected Liberty token {key!r} in {kind}")
    return g, j + 1


def _block_end(text: str, open_brace: int) -> int:
    depth = 0
    i = open_brace
    n = len(text)
    while i < n:
        c = text[i]
        if c == '"':
            i = text.index('"', i + 1) + 1
            continue
        if text.startswith('/*', i):
            i = text.index('*/', i + 2) + 2
            continue
        if c == '{':
            depth += 1
        elif c == '}':
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    raise LibertyError("unbalanced Liberty braces")


_CELL = re.compile(r'(?m)^[ \t]*cell\s*\(\s*"?([A-Za-z0-9_$]+)"?\s*\)\s*\{')


class Library:
    def __init__(self, path, wanted: set[str] | None = None):
        data = open(path, 'rb').read(MAX_LIBRARY_BYTES + 1)
        if len(data) > MAX_LIBRARY_BYTES:
            raise LibertyError("Liberty file exceeds byte budget")
        text = data.decode('ascii')
        first = _CELL.search(text)
        if first is None:
            raise LibertyError("no cell groups")
        header = text[:first.start()] + '}'
        self.header, _ = parse_group(tokens(header))
        self.cells: dict[str, Group] = {}
        self.all_cells: list[str] = []
        for m in _CELL.finditer(text):
            self.all_cells.append(m[1])
            if wanted is not None and m[1] not in wanted:
                continue
            end = _block_end(text, m.end() - 1)
            g, _ = parse_group(tokens(text[m.start():end]))
            self.cells[m[1]] = g
        self.templates = {g.name: g for g in self.header.sub('lu_table_template')}

    def time_unit_ns(self) -> float:
        unit = self.header.attr('time_unit')
        table = {'1ns': 1.0, '1ps': 1e-3, '10ps': 1e-2, '100ps': 0.1}
        if unit not in table:
            raise LibertyError(f"unsupported time_unit {unit}")
        return table[unit]

    def cap_unit_pf(self) -> float:
        c = self.header.complex.get('capacitive_load_unit')
        if not c or len(c[0]) != 2:
            raise LibertyError("capacitive_load_unit missing")
        scale, unit = float(c[0][0]), c[0][1].lower()
        return scale * {'pf': 1.0, 'ff': 1e-3}[unit]


def numbers(s: str) -> list[float]:
    return [float(x) for x in s.replace('\\', ' ').replace('"', ' ').split(',') if x.strip()]


class Table:
    """NLDM 2-D table with axes named by the template variables."""

    def __init__(self, group: Group, library: Library):
        template = library.templates.get(group.name)
        if template is None:
            raise LibertyError(f"table template {group.name} missing")
        self.variables = [template.attr('variable_1'), template.attr('variable_2')]
        idx = lambda k: numbers(','.join(group.complex[k][0])) if k in group.complex else (  # noqa: E731
            numbers(','.join(template.complex[k][0])) if k in template.complex else None)
        self.index_1, self.index_2 = idx('index_1'), idx('index_2')
        rows = [numbers(r) for r in group.complex['values'][0]]
        if self.index_2 is None:
            raise LibertyError("only 2-D tables are supported")
        if len(rows) != len(self.index_1) or any(len(r) != len(self.index_2) for r in rows):
            raise LibertyError("table shape differs from its indices")
        self.values = rows

    def lookup(self, slew: float, load: float) -> float:
        point = {'input_net_transition': slew, 'total_output_net_capacitance': load}
        if set(self.variables) != set(point):
            raise LibertyError(f"unsupported table variables {self.variables}")
        x, y = point[self.variables[0]], point[self.variables[1]]
        return bilinear(self.index_1, self.index_2, self.values, x, y)


def bilinear(xs, ys, values, x, y):
    if not (xs[0] <= x <= xs[-1] and ys[0] <= y <= ys[-1]):
        raise LibertyError(f"point ({x}, {y}) outside the characterized table; no extrapolation")

    def bracket(axis, v):
        k = min(max(bisect.bisect_right(axis, v) - 1, 0), len(axis) - 2)
        t = (v - axis[k]) / (axis[k + 1] - axis[k])
        return k, t
    i, tx = bracket(xs, x)
    j, ty = bracket(ys, y)
    a = values[i][j] * (1 - ty) + values[i][j + 1] * ty
    b = values[i + 1][j] * (1 - ty) + values[i + 1][j + 1] * ty
    return a * (1 - tx) + b * tx
