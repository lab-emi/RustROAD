#!/usr/bin/env python3
"""Reference outputs for the pyjson tests, written by CPython's own json module.

usage: python3 generate.py   (run in this directory; rewrites floats.txt and doc*.json)
"""
import json
import random
import struct

random.seed(20261112)
values = [0.0, -0.0, 1.0, 0.1, 1e-4, 1e-5, 9.999e-5, 1e15, 1e16, 9.999999999999999e15, 1e22, 1e23,
          5e-324, 2.2250738585072014e-308, 1.7976931348623157e308, 0.30000000000000004,
          123456789012345680.0, 0.1 + 0.2, 2 / 3, 1 / 3, 1e-7, 1.5e-10, 299792458.0, 4.35e-321]
for _ in range(400):
    bits = random.getrandbits(64)
    value = struct.unpack('<d', struct.pack('<Q', bits))[0]
    if value == value and abs(value) != float('inf'):
        values.append(value)
for _ in range(400):
    values.append(random.uniform(-1, 1) * 10 ** random.randint(-12, 20))
for _ in range(200):
    values.append(round(random.uniform(0, 1000), random.randint(0, 6)))
with open('floats.txt', 'w') as out:
    for value in values:
        out.write('%016x %s\n' % (struct.unpack('<Q', struct.pack('<d', value))[0], repr(value)))

docs = [
    '{"b": 1, "a": [1, 2.0, 3e5, -0, -0.0, 1E-7, 12345678901234567890], "c": {"z": null, "y": true, "x": false},'
    ' "e": [], "f": {}, "g": [[], {}, [{}]], "s": "q\\"b\\\\ \\u00e9 \\ud83d\\ude00 \\n\\t\\r\\b\\f\\u0001 \\u007f /"}',
    '[1.0, 100.0, 1e16, 1e-5, 0.1, 123456.789, 1.5e300, 2.5e-300, 49.636384244388736, 0.11431166591439057]',
    '"plain"',
    '{"dup": 1, "other": 2, "dup": 3}',
    ' \n\t{ "nested" : { "deep" : [ { "k" : [ 1 , 2 , { "m" : "v" } ] } ] } , "n" : -12 } \r\n',
]
for index, text in enumerate(docs):
    value = json.loads(text)
    with open(f'doc{index}.src.json', 'w', encoding='utf-8') as out:
        out.write(text)
    for name, rendered in [('line', json.dumps(value)), ('indent2', json.dumps(value, indent=2)),
                           ('indent1', json.dumps(value, indent=1)),
                           ('compact', json.dumps(value, ensure_ascii=False, separators=(',', ':'))),
                           ('lineutf8', json.dumps(value, ensure_ascii=False))]:
        with open(f'doc{index}.{name}.json', 'w', encoding='utf-8') as out:
            out.write(rendered)
