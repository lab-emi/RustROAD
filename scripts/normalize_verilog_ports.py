#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Bounded ANSI header normalization; copy structural netlist bodies verbatim."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re

MAX_STATEMENT = 1 << 20
MAX_PREFIX = 16 << 20
MAX_PORTS = 65_536
MAX_FILE = 16 << 30
CHUNK = 1 << 20
TOKEN = re.compile(r"\s+|//[^\n]*|/\*.*?\*/|\\[^\s]+|[A-Za-z_$][A-Za-z0-9_$]*|[-+]?\d+|[(),;:\[\]]", re.S)
NAME = re.compile(r"(?:[A-Za-z_$][A-Za-z0-9_$]*|\\[^\s]+)\Z")
DIRECTIONS = {"input", "output", "inout"}
NET_TYPES = {"wire", "tri", "tri0", "tri1", "trireg", "wand", "wor", "reg", "logic", "supply0", "supply1"}
QUALIFIERS = NET_TYPES | {"signed", "unsigned"}
LATER_DECL = re.compile(rb'(?<![A-Za-z0-9_$])(?:input|output|inout|wire|reg|logic|tri0?|tri1|trireg|wand|wor|supply0|supply1|integer|time|real|realtime|bit|byte|shortint|int|longint|shortreal|event|interconnect|uwire|var|enum|struct|union|typedef|import|string|chandle|class|interface|function|task|always|initial|generate|signed|unsigned)(?![A-Za-z0-9_$])|`|::')


class PrefixError(ValueError):
    """Unsupported, inconsistent or over-budget module/port prefix."""


def tokens(data: bytes) -> list[tuple[str, int]]:
    try:
        source = data.decode('ascii')
    except UnicodeDecodeError as error:
        raise PrefixError('non-ASCII module/port prefix') from error
    found = []
    at = 0
    while at < len(source):
        match = TOKEN.match(source, at)
        if match is None:
            raise PrefixError(f'unsupported prefix token at byte {at}')
        value = match[0]
        if value.startswith(('//', '/*')):
            if re.search(r'\b(?:translate_on|translate_off|full_case|parallel_case)\b', value, re.I):
                raise PrefixError('semantic pragma in module/port prefix')
        elif not value.isspace():
            if len(found) >= 262_144:
                raise PrefixError('prefix token limit')
            found.append((value, at))
        at = match.end()
    return found


def identifier(value: str) -> str:
    if not NAME.fullmatch(value) or len(value) > 4096:
        raise PrefixError('invalid or over-budget identifier')
    return value[1:] if value.startswith('\\') else value


def names(values: list[str]) -> list[str]:
    if not values:
        return []
    if len(values) % 2 == 0 or any(values[i] != ',' for i in range(1, len(values), 2)):
        raise PrefixError('unsupported port name list')
    result = values[::2]
    for name in result:
        identifier(name)
    if len(result) > MAX_PORTS:
        raise PrefixError('port count limit')
    return result


def declaration(statement: bytes) -> tuple[str, list[str]]:
    values = [value for value, _ in tokens(statement)]
    if not values or values[0] not in DIRECTIONS | NET_TYPES or values[-1] != ';':
        raise PrefixError('ports must be declared before other module statements')
    prefix = [values[0]]
    i = 1
    while i < len(values) and values[i] in QUALIFIERS:
        prefix.append(values[i])
        i += 1
        if len(prefix) > 8:
            raise PrefixError('port qualifier limit')
    if i < len(values) and values[i] == '[':
        if i + 4 >= len(values) or values[i + 2] != ':' or values[i + 4] != ']' \
                or not all(re.fullmatch(r'[-+]?\d+', values[j]) for j in [i + 1, i + 3]):
            raise PrefixError('unsupported port range')
        prefix.append('[' + values[i + 1] + ':' + values[i + 3] + ']')
        i += 5
    declared = names(values[i:-1])
    if not declared:
        raise PrefixError('empty port declaration')
    return ' '.join(prefix), declared


def net_signature(prefix: str) -> tuple:
    values = [v for v, _ in tokens((prefix + ';').encode('ascii'))][:-1]
    kinds = [v for v in values if v in NET_TYPES]
    signs = [v for v in values if v in {'signed', 'unsigned'}]
    if len(kinds) > 1 or len(signs) > 1:
        raise PrefixError('conflicting port qualifiers')
    range_ = None
    if '[' in values:
        i = values.index('[')
        range_ = (int(values[i + 1]), int(values[i + 3]))
    return (kinds[0] if kinds else 'wire', signs == ['signed'], range_)


class BodyDeclarations:
    """Reject late declarations that would change a normalized port's type.

    Generated mapped files use one declaration per line. Ambiguous inline or
    multiline declarations are refused rather than silently changing meaning.
    Non-declaration body bytes, including arbitrarily long leading comments,
    remain a bounded streaming copy.
    """
    def __init__(self, ports: dict):
        self.ports = ports
        self.block = False
        self.line_comment = False
        self.continuation = False
        self.tail = b''

    def check(self, data: bytes) -> None:
        code = data
        if self.line_comment:
            if data.endswith(b'\n'):
                self.line_comment = False
            return
        if self.continuation and not self.block:
            if LATER_DECL.search(self.tail + data):
                raise PrefixError('ambiguous declaration in a continued body line')
            self.continuation = not data.endswith(b'\n')
            self.tail = data[-16:] if self.continuation else b''
            return
        while True:
            if self.block:
                end = code.find(b'*/')
                if end < 0:
                    # Retain a possible closing '*' across chunks.
                    self.tail = code[-1:]
                    return
                code, self.block = code[end + 2:], False
            code = code.lstrip()
            if code.startswith(b'/*'):
                code, self.block = code[2:], True
                continue
            if code.startswith(b'//'):
                self.line_comment = not data.endswith(b'\n')
                return
            break
        if not code:
            return
        if b';' in code:
            statement, rest = code.split(b';', 1)
            rest = rest.strip()
            if rest and rest != b'endmodule' and not rest.startswith(b'//'):
                raise PrefixError('multiple or ambiguous inline body statements')
            code = statement + b';'
        match = re.match(rb'[A-Za-z_$][A-Za-z0-9_$]*', code)
        first = match[0].decode('ascii') if match else None
        if not self.continuation and first in NET_TYPES:
            if not data.endswith(b'\n') and len(data) == CHUNK:
                raise PrefixError('multiline or over-budget net declaration')
            prefix, declared = declaration(code)
            signature = net_signature(prefix)
            for name in declared:
                if identifier(name) in self.ports and signature != self.ports[identifier(name)]:
                    raise PrefixError('port redeclaration changes type, signedness or range')
        elif LATER_DECL.search(self.tail + code):
            raise PrefixError('late port declaration, macro or ambiguous inline net declaration')
        elif first not in {'assign', 'endmodule'}:
            if code.startswith(b'=') or (b'=' in code and b'=' in code.split(b'(', 1)[0]):
                raise PrefixError('unsupported body declaration or initializer')
            if b';' in code and not code.split(b';', 1)[0].rstrip().endswith(b')'):
                raise PrefixError('unsupported non-instance body statement')
        self.continuation = not data.endswith(b'\n')
        self.tail = code[-16:] if self.continuation else b''


def normalize(source: Path, target: Path, maximum: int = MAX_FILE) -> dict:
    """Normalize a mapped module prefix without interpreting or retaining its body."""
    if not 1 <= maximum <= MAX_FILE:
        raise PrefixError('file allowance outside1..16GiB')
    if target.exists() or target.is_symlink():
        raise FileExistsError(target)
    temporary = target.with_name(target.name + f'.partial-{os.getpid()}')
    source_hash, body_hash, output_hash = (hashlib.sha256() for _ in range(3))
    prefix_bytes = output_bytes = 0
    with source.open('rb') as stream:
        before = os.fstat(stream.fileno())
        if before.st_size > maximum:
            raise PrefixError('input file byte limit')

        def statement() -> bytes:
            nonlocal prefix_bytes
            data = bytearray()
            state = 'plain'
            previous = b''
            while True:
                char = stream.read(1)
                if not char:
                    raise PrefixError('unexpected EOF in module/port prefix')
                data.extend(char)
                source_hash.update(char)
                prefix_bytes += 1
                if len(data) > MAX_STATEMENT or prefix_bytes > MAX_PREFIX:
                    raise PrefixError('module/port prefix byte limit')
                if state == 'escaped':
                    if char.isspace():
                        state = 'plain'
                elif state == 'line':
                    if char == b'\n':
                        state = 'plain'
                elif state == 'block':
                    if previous == b'*' and char == b'/':
                        state = 'plain'
                elif char == b'\\':
                    state = 'escaped'
                elif char == b'/' and stream.peek(1)[:1] in (b'/', b'*'):
                    state = 'line' if stream.peek(1)[:1] == b'/' else 'block'
                elif char == b';':
                    return bytes(data)
                previous = char

        header = statement()
        lexed = tokens(header)
        values = [value for value, _ in lexed]
        if len(values) < 5 or values[0] != 'module' or values[2] != '(' or values[-2:] != [')', ';']:
            raise PrefixError('only a non-parameterized mapped module is supported')
        identifier(values[1])
        ansi = bool(values[3:-2] and values[3] in DIRECTIONS)
        ordered = [] if ansi else names(values[3:-2])
        canonical = [identifier(name) for name in ordered]
        if len(set(canonical)) != len(canonical):
            raise PrefixError('duplicate module port')
        wanted, declarations, wire_ports = set(canonical), {}, {}
        retained = bytearray()
        while not ansi and len(declarations) < len(ordered):
            raw = statement()
            prefix, declared = declaration(raw)
            if prefix.split()[0] in NET_TYPES:
                retained.extend(raw)
                for name in declared:
                    key = identifier(name)
                    if key in wanted:
                        signature = net_signature(prefix)
                        if key in wire_ports and wire_ports[key] != signature:
                            raise PrefixError('conflicting wire declarations for port')
                        wire_ports[key] = signature
                continue
            for name in declared:
                key = identifier(name)
                if key not in wanted or key in declarations:
                    raise PrefixError('extra or duplicate port declaration')
                declarations[key] = prefix
        port_types = {key: net_signature(prefix) for key, prefix in declarations.items()}
        if any(port_types[key] != value for key, value in wire_ports.items()):
            raise PrefixError('port redeclaration changes type, signedness or range')
        body_source_offset = stream.tell()
        created = False
        try:
            with temporary.open('xb') as output:
                created = True
                def emit(data: bytes) -> None:
                    nonlocal output_bytes
                    output_bytes += len(data)
                    if output_bytes > maximum:
                        raise PrefixError('output file byte limit')
                    output.write(data)
                    output_hash.update(data)

                if ansi:
                    emit(header)
                else:
                    emit(header[:lexed[0][1]])
                    module = values[1] + (' ' if values[1].startswith('\\') else '')
                    emit(('module ' + module + ' (\n').encode('ascii'))
                    for i, name in enumerate(ordered):
                        spelling = name + (' ' if name.startswith('\\') else '')
                        emit(('  ' + declarations[identifier(name)] + ' ' + spelling
                              + (',' if i + 1 < len(ordered) else '') + '\n').encode('ascii'))
                        if output_bytes > MAX_PREFIX:
                            raise PrefixError('normalized header byte limit')
                    emit(b');')
                    if output_bytes + len(retained) > MAX_PREFIX:
                        raise PrefixError('normalized declaration prefix byte limit')
                    emit(retained)
                body_target_offset = output_bytes
                checker = BodyDeclarations(port_types)
                while chunk := stream.readline(CHUNK):
                    if not ansi:
                        # A block comment may end exactly across chunk boundaries.
                        if checker.block and checker.tail == b'*':
                            checker.check(b'*' + chunk)
                        else:
                            checker.check(chunk)
                    source_hash.update(chunk)
                    body_hash.update(chunk)
                    emit(chunk)
                after = os.fstat(stream.fileno())
                current = source.stat()
                identity = lambda stat: (stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns)
                if identity(before) != identity(after) or identity(before) != identity(current):
                    raise PrefixError('source changed during normalization')
                output.flush()
                os.fsync(output.fileno())
            os.link(temporary, target)
        finally:
            if created:
                temporary.unlink(missing_ok=True)
    return dict(status='normalized' if not ansi else 'copied_ansi', ports=len(ordered) if not ansi else None,
                source_bytes=before.st_size, output_bytes=output_bytes,
                source_sha256=source_hash.hexdigest(), output_sha256=output_hash.hexdigest(),
                body_sha256=body_hash.hexdigest(), body_source_offset=body_source_offset,
                body_target_offset=body_target_offset, retained_declaration_bytes=len(retained),
                retained_declaration_sha256=hashlib.sha256(retained).hexdigest(),
                scope='header format only; intervening net declarations and body suffix retained verbatim')


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--max-mib', type=int, default=16_384)
    args = parser.parse_args()
    try:
        result = normalize(args.source, args.out, args.max_mib << 20)
    except (PrefixError, OSError) as error:
        print(json.dumps(dict(status='error', code='VERILOG_HEADER_ERROR', message=str(error))))
        return 3
    print(json.dumps(result, indent=2))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
