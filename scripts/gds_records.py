#!/usr/bin/env python3
"""Minimal bounded GDSII record reader/writer (stdlib only).

Used by scripts/magic_rc.py to derive a routing-only terminal layout from a
native GDS: structures are kept as raw record lists, so everything that is not
explicitly removed is copied byte for byte. Only the record types RustROAD's
writer and the PDK cell libraries use are interpreted (BGNSTR/STRNAME/ENDSTR,
element starts, SNAME, ENDEL); everything else is opaque.
"""
from __future__ import annotations

from pathlib import Path
import struct

MAX_GDS_BYTES = 256 * 1024 * 1024
MAX_RECORDS = 20_000_000

HEADER, BGNLIB, LIBNAME, UNITS, ENDLIB = 0x00, 0x01, 0x02, 0x03, 0x04
BGNSTR, STRNAME, ENDSTR = 0x05, 0x06, 0x07
BOUNDARY, PATH, SREF, AREF, TEXT = 0x08, 0x09, 0x0A, 0x0B, 0x0C
LAYER, DATATYPE, XY, ENDEL, SNAME = 0x0D, 0x0E, 0x10, 0x11, 0x12
TEXTTYPE, PRESENTATION, STRING, STRANS = 0x16, 0x17, 0x19, 0x1A
BOX, NODE = 0x2D, 0x15
ELEMENT_STARTS = {BOUNDARY, PATH, SREF, AREF, TEXT, BOX, NODE}

NO_DATA, BITS, INT16, INT32, REAL8, ASCII = 0, 1, 2, 3, 5, 6


class GdsError(RuntimeError):
    pass


def records(data: bytes):
    """Yield (record_type, data_type, payload) with bounds checks."""
    if len(data) > MAX_GDS_BYTES:
        raise GdsError("GDS over the byte budget")
    pos, count = 0, 0
    while pos < len(data):
        if pos + 4 > len(data):
            raise GdsError("truncated GDS record header")
        length, rtype, dtype = struct.unpack(">HBB", data[pos:pos + 4])
        if length < 4 or pos + length > len(data):
            raise GdsError(f"bad GDS record length {length} at byte {pos}")
        yield rtype, dtype, data[pos + 4:pos + length]
        pos += length
        count += 1
        if count > MAX_RECORDS:
            raise GdsError("GDS record budget exceeded")
        if rtype == ENDLIB:
            return
    raise GdsError("GDS without ENDLIB")


def encode(rtype: int, dtype: int, payload: bytes = b"") -> bytes:
    if len(payload) % 2:
        payload += b"\0"
    if len(payload) + 4 > 0xFFFF:
        raise GdsError("GDS record too long")
    return struct.pack(">HBB", len(payload) + 4, rtype, dtype) + payload


def ascii_payload(text: str) -> bytes:
    raw = text.encode("ascii")
    return raw + (b"\0" if len(raw) % 2 else b"")


def text_of(payload: bytes) -> str:
    return payload.rstrip(b"\0").decode("ascii")


class Library:
    """Header records, then structures (name, [element record lists], raw head)."""

    def __init__(self, path: Path):
        data = Path(path).read_bytes()
        self.head: list[bytes] = []
        self.structures: list[dict] = []
        current = None
        element = None
        for rtype, dtype, payload in records(data):
            raw = encode(rtype, dtype, payload)
            if rtype == BGNSTR:
                current = dict(name=None, begin=[raw], elements=[])
                continue
            if current is None:
                if rtype != ENDLIB:
                    self.head.append(raw)
                continue
            if rtype == STRNAME:
                current["name"] = text_of(payload)
                current["begin"].append(raw)
            elif rtype == ENDSTR:
                if element is not None:
                    raise GdsError("element not closed before ENDSTR")
                self.structures.append(current)
                current = None
            elif rtype in ELEMENT_STARTS:
                if element is not None:
                    raise GdsError("nested GDS element")
                element = dict(kind=rtype, records=[raw], sname=None)
            elif element is not None:
                element["records"].append(raw)
                if rtype == SNAME:
                    element["sname"] = text_of(payload)
                if rtype == ENDEL:
                    current["elements"].append(element)
                    element = None
            else:
                current["begin"].append(raw)
        if current is not None or element is not None:
            raise GdsError("unterminated GDS structure")
        self.units = None
        for raw in self.head:
            if raw[2] == UNITS:
                user, meters = struct.unpack(">8s8s", raw[4:20])
                self.units = (real8(user), real8(meters))

    def names(self) -> list[str]:
        return [s["name"] for s in self.structures]

    def write(self, path: Path) -> None:
        out = bytearray()
        for raw in self.head:
            out += raw
        for s in self.structures:
            for raw in s["begin"]:
                out += raw
            for e in s["elements"]:
                for raw in e["records"]:
                    out += raw
            out += encode(ENDSTR, NO_DATA)
        out += encode(ENDLIB, NO_DATA)
        Path(path).write_bytes(bytes(out))


def real8(raw: bytes) -> float:
    """GDS excess-64 base-16 real."""
    sign = -1.0 if raw[0] & 0x80 else 1.0
    exponent = (raw[0] & 0x7F) - 64
    mantissa = int.from_bytes(raw[1:8], "big") / float(1 << 56)
    return sign * mantissa * (16.0 ** exponent)


def int32s(values) -> bytes:
    return b"".join(struct.pack(">i", int(v)) for v in values)


def boundary(layer: int, datatype: int, rect) -> dict:
    lx, ly, ux, uy = rect
    recs = [encode(BOUNDARY, NO_DATA), encode(LAYER, INT16, struct.pack(">h", layer)),
            encode(DATATYPE, INT16, struct.pack(">h", datatype)),
            encode(XY, INT32, int32s([lx, ly, ux, ly, ux, uy, lx, uy, lx, ly])), encode(ENDEL, NO_DATA)]
    return dict(kind=BOUNDARY, records=recs, sname=None)


def text(layer: int, texttype: int, at, string: str) -> dict:
    recs = [encode(TEXT, NO_DATA), encode(LAYER, INT16, struct.pack(">h", layer)),
            encode(TEXTTYPE, INT16, struct.pack(">h", texttype)), encode(XY, INT32, int32s(at)),
            encode(STRING, ASCII, ascii_payload(string)), encode(ENDEL, NO_DATA)]
    return dict(kind=TEXT, records=recs, sname=None)


def sref(name: str, at) -> dict:
    recs = [encode(SREF, NO_DATA), encode(SNAME, ASCII, ascii_payload(name)), encode(XY, INT32, int32s(at)),
            encode(ENDEL, NO_DATA)]
    return dict(kind=SREF, records=recs, sname=name)


def structure(name: str, elements: list[dict]) -> dict:
    begin = [encode(BGNSTR, INT16, struct.pack(">12h", *([2026, 1, 1, 0, 0, 0] * 2))),
             encode(STRNAME, ASCII, ascii_payload(name))]
    return dict(name=name, begin=begin, elements=elements)
