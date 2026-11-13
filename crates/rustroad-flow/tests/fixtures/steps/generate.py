#!/usr/bin/env python3
"""Write the step fixtures and their expected outputs with the Python reference helpers.

usage: generate.py --recipe DIR --rc-builder FILE
  --recipe      integration/ibex_sg13g2_ez130 of the RustEDA superproject (the reference helpers:
                native_physical_inputs.py, native_postroute.py)
  --rc-builder  scripts/build_rc_profile.py with the recipe's `rules_override` argument (the
                script of this repository plus `rules_override: Path | None = None` as the fifth
                parameter of build(), used for `rules_path` and, at the nominal corner, for the
                nominal table)

The inputs are synthetic: small files in the formats of the PDK data the steps read, with made-up
numbers. Everything is generated under a temporary root; in the stored expectations that root is
written as @ROOT@, and the SHA-256/size of a file whose own text holds the root is written as
@SHA:<relative path>@ / "@BYTES:<relative path>@" (the tests recompute those from the files they
have just compared).
"""
import argparse
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import struct
import sys
import tempfile

HERE = Path(__file__).resolve().parent
ROUTING = ['Metal1', 'Metal2', 'Metal3', 'Metal4', 'Metal5', 'TopMetal1', 'TopMetal2']
CUTS = ['Via1', 'Via2', 'Via3', 'Via4', 'TopVia1', 'TopVia2']
WIDTH = [0.16, 0.2, 0.2, 0.2, 0.2, 1.64, 2.0]


def load(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    sys.path.insert(0, str(Path(path).parent))
    spec.loader.exec_module(module)
    return module


def tech_lef():
    out = ['VERSION 5.8 ;', 'UNITS', '  DATABASE MICRONS 1000 ;', 'END UNITS', '']
    for i, name in enumerate(ROUTING):
        out += [f'LAYER {name}', '  TYPE ROUTING ;   # routing layer', f'  WIDTH {WIDTH[i]} ;',
                '  WIDTH 0.5 0.6 ;', f'  RESISTANCE RPERSQ {0.11 - 0.012 * i:.4f} ;',
                f'  CAPACITANCE CPERSQDIST {3.5e-05 - 3e-06 * i:.3e} ;', f'  EDGECAPACITANCE {4.1e-05 - 2e-06 * i:.3e} ;',
                f'END {name}', '']
        if i < len(CUTS):
            out += [f'LAYER {CUTS[i]}', '  TYPE CUT ;', f'  RESISTANCE {20.0 - 2.5 * i} ;', f'END {CUTS[i]}', '']
    return '\n'.join(out) + 'END LIBRARY\n'


def rules(scale, resistance_scale):
    out = ['Extraction Rules for OpenRCX', '', 'DIAGMODEL ON', 'LayerCount 7', 'DensityRate 1  0', '']
    def table(header, rows):
        out.append(header)
        out.append(f'DIST count {len(rows)} width {rows[0][4] if rows else 0.16}')
        out.extend(' '.join(repr(v) for v in row[:4]) for row in rows)
        out.append('END DIST')
    for m in range(1, 8):
        w = WIDTH[m - 1]
        res = (0.11 - 0.012 * (m - 1)) / 1000.0 / w * resistance_scale
        out += [f'Metal {m} RESOVER', f'WIDTH Table 1 entries:  {w}']
        table(f'Metal {m} RESOVER 0', [[w * (k + 1), 0.0, 0.0, res, w] for k in range(3)])
        out += [f'Metal {m} OVER', f'WIDTH Table 1 entries:  {w}']
        for under in range(0, m):
            base = (2.1e-05 + 3e-06 * under + 1e-06 * m) * scale
            table(f'Metal {m} OVER {under}', [[w * (k + 1), 9e-05 * scale / (k + 1), base * (1 + 0.2 * k), 0.0, w]
                                              for k in range(4)])
        if m < 7:
            out += [f'Metal {m} UNDER', f'WIDTH Table 1 entries:  {w}']
            for above in range(m + 1, 8):
                table(f'Metal {m} UNDER {above}', [[w * (k + 1), 8e-05 * scale / (k + 1), 1.9e-05 * scale * (1 + 0.1 * k), 0.0, w]
                                                   for k in range(2)])
            out += [f'Metal {m} DIAGUNDER', f'WIDTH Table 1 entries:  {w}']
            table(f'Metal {m} DIAGUNDER {m + 1}', [[w, 1e-06, 0.0, 0.0, w]])
        if 1 < m < 7:
            out += [f'Metal {m} OVERUNDER', f'WIDTH Table 1 entries:  {w}']
            table(f'Metal {m} OVER {m - 1} UNDER {m + 1}', [[w * (k + 1), 7e-05 * scale / (k + 1), 3.3e-05 * scale, 0.0, w]
                                                             for k in range(2)])
        out.append('')
    return '\n'.join(out) + '\n'


def magic_tech():
    out = ['tech', ' format 35', ' ihp-sg13g2', 'end', '', 'types']
    for i in range(1, 8):
        out.append(f' metal{i} metal{i},m{i}')
    for i in range(1, 7):
        out.append(f' metal{i} via{i},v{i}')
    out += ['end', '', 'contact']
    for i in range(1, 7):
        out.append(f' via{i} metal{i} metal{i + 1}')
    out += [' pc poly metal1', 'end', '']
    return '\n'.join(out)


def magic_extract():
    out = ['extract', ' style ngspice variants (),(hrhc),(lrlc)', ' cscale 1', '']
    contacts = [f'via{i}' for i in range(1, 7)] + ['pc', 'ndc', 'mimcc']
    for variant, r, c in (('()', 1.0, None), ('(hrhc)', 1.2, 15000), ('(lrlc)', 0.8, 0)):
        out.append(f' variants {variant}')
        for i in range(1, 8):
            out.append(f' resist (allm{i})/metal{i} {round((110 - 12 * (i - 1)) * r, 3)}   # mOhm/sq')
        for k, name in enumerate(contacts):
            out.append(f' contact {name} {c if c is not None else 20000 - 2500 * k}')
        out.append('')
    out.append(' variants *')
    for i in range(1, 8):
        out.append(f' defaultareacap allm{i} metal{i} {round(35.2 - 3.1 * i, 3)}')
        out.append(f' defaultperimeter allm{i} metal{i} {round(40.1 - 2.2 * i, 3)}')
        out.append(f' defaultsidewall allm{i} metal{i} {round(20.5 + i, 3)}' + (' 0.1' if i % 2 else ''))
        for j in range(1, i):
            out.append(f' defaultoverlap allm{i} metal{i} allm{j} metal{j} {round(60.3 - 4 * (i - j), 3)}')
    out += ['end', '']
    return '\n'.join(out)


def gds_record(kind, dtype, payload=b''):
    if len(payload) % 2:
        payload += b'\0'
    return struct.pack('>HBB', len(payload) + 4, kind, dtype) + payload


def gds(names, shapes):
    out = gds_record(0, 2, struct.pack('>h', 600)) + gds_record(1, 2, b'\0' * 24) + gds_record(2, 6, b'LIB')
    out += gds_record(3, 5, bytes.fromhex('3e4189374bc6a7f0') + bytes.fromhex('3944b82fa09b5a54'))
    for name in names:
        out += gds_record(5, 2, b'\0' * 24) + gds_record(6, 6, name.encode())
        for layer in shapes.get(name, [8]):
            out += gds_record(8, 0) + gds_record(13, 2, struct.pack('>h', layer)) + gds_record(14, 2, struct.pack('>h', 0))
            out += gds_record(16, 3, struct.pack('>10i', 0, 0, 100 * layer, 0, 100 * layer, 200, 0, 200, 0, 0)) + gds_record(17, 0)
        out += gds_record(7, 0)
    return out + gds_record(4, 0)


def layer_map():
    out = ['# synthetic LEF/DEF layer map', 'DIEAREA ALL 189 4', 'NAME Metal1/PIN 8 25', 'Metal1 NET,SPNET,VIA 8 0',
           'Metal1 PIN,LEFPIN 8 2', 'Metal1 LEFOBS 8 0', 'Metal1 FILL 8 22', 'Metal1 odd text here 8 x']
    numbers = dict(Via1=19, Metal2=10, Via2=29, Metal3=30, Via3=49, Metal4=50, Via4=66, Metal5=67, TopVia1=125,
                   TopMetal1=126, TopVia2=133, TopMetal2=134)
    for name, layer in numbers.items():
        if 'Via' in name:
            out += [f'{name} NET,SPNET,VIA,PIN {layer} 0', f'{name} VIA {layer} 0']
        else:
            out += [f'NAME {name}/PIN {layer} 25', f'{name} NET,SPNET {layer} 0', f'{name} VIA {layer} 0',
                    f'{name} PIN {layer} 2', f'{name} PIN {layer} 0']
    return '\n'.join(out) + '\n'


LIBERTY = '''/* synthetic sizing library, {corner} */
library(fixture_{corner}) {{
  time_unit : "1ns";
  capacitive_load_unit(1,pf);
  cell(BUFX1) {{
    area : 5.4;
    pin(A) {{ direction : input; }}
    pin(Y) {{
      direction : output;
      function : "A";
      max_capacitance : {c1};
      timing() {{
        related_pin : "A";
        cell_rise(tmpl) {{ values("0.1, 0.2"); }}
      }}
    }}
  }}
  cell(BUFX2) {{
    area : 5.4;
    pin(A) {{
      direction : input;
    }}
    pin(Y) {{
      direction : output;
      function : " A ";
      max_capacitance : 0.2;
      max_transition : 1.5;
    }}
  }}
  cell(BUFX4) {{
    area : 5.4;
    pin(A) {{
      direction : input;
    }}
    pin(Y) {{
      direction : output;
      function : "A";
      max_capacitance : 0.4;
    }}
  }}
  cell(BUFX8) {{
    area : 10.8;
    pin(A) {{
      direction : input;
    }}
    pin(Y) {{
      direction : output;
      function : "A";
      max_capacitance : 0.8;
    }}
  }}
  cell(INVX1) {{
    area : 3.6;
    pin(A) {{
      direction : input;
    }}
    pin(Y) {{
      direction : output;
      function : "!A";
      max_capacitance : 0.1;
    }}
  }}
  cell(INVX2) {{
    area : 0;
    pin(A) {{
      direction : input;
    }}
    pin(Y) {{
      direction : output;
      function : "!A";
    }}
  }}
  cell(INVX3) {{
    dont_use : true;
    area : 3.6;
    pin(A) {{
      direction : input;
    }}
    pin(Y) {{
      direction : output;
      function : "!A";
      max_capacitance : 0.3;
    }}
  }}
  cell(NANDX1) {{
    area : 4.5;
    pin(A) {{
      direction : input;
    }}
    pin(B) {{
      direction : input;
    }}
    pin(Y) {{
      direction : output;
      function : "{nand}";
      max_capacitance : 0.1;
    }}
  }}
  cell(NANDX2) {{
    area : 4.5;
    pin(A) {{
      direction : input;
    }}
    pin(B) {{
      direction : input;
    }}
    pin(Y) {{
      direction : output;
      function : "!(A & B)";
      max_capacitance : 0.2;
    }}
  }}
  cell(DFFX1) {{
    area : 20;
    ff(IQ, IQN) {{
      clocked_on : "CK";
      next_state : "D";
    }}
    pin(D) {{
      direction : input;
    }}
    pin(CK) {{
      direction : input;
    }}
    pin(Q) {{
      direction : output;
      function : "IQ";
      max_capacitance : 0.2;
    }}
  }}
  cell(DFFX2) {{
    area : 20;
    ff(IQ, IQN) {{
      clocked_on : "CK";
      next_state : "D";
    }}
    pin(D) {{
      direction : input;
    }}
    pin(CK) {{
      direction : input;
    }}
    pin(Q) {{
      direction : output;
      function : "IQ";
      max_capacitance : 0.4;
    }}
  }}
  cell(TIEHI) {{
    area : 1.8;
    pin(Y) {{
      direction : output;
      function : "1";
    }}
  }}
  cell(TIELO) {{
    area : 1.8;
    pin(Y) {{
      direction : output;
      function : "0";
    }}
  }}
  cell(ICGX1) {{
    area : 9;
    clock_gating_integrated_cell : "latch_posedge";
    pin(CK) {{
      direction : input;
    }}
    pin(GCK) {{
      direction : output;
      function : "CK";
    }}
  }}
  cell(IOBUF) {{
    area : 9;
    pin(PAD) {{
      direction : inout;
      function : "A";
    }}
    pin(A) {{
      direction : input;
    }}
  }}{extra}
}}
'''
ONLY_TT = '''
  cell(ONLYTT1) {
    area : 2;
    pin(A) {
      direction : input;
    }
    pin(Y) {
      direction : output;
      function : "A";
      max_capacitance : 0.1;
    }
  }
  cell(ONLYTT2) {
    area : 2;
    pin(A) {
      direction : input;
    }
    pin(Y) {
      direction : output;
      function : "A";
      max_capacitance : 0.2;
    }
  }'''
LEF = '''VERSION 5.8 ;
MACRO BUFX1
  CLASS CORE ;
  SIZE 1.44 BY 3.33 ;
  PIN A
    DIRECTION INPUT ;
  END A
END BUFX1
MACRO BUFX2
  SIZE 1.44 BY 3.33 ;
END BUFX2
MACRO BUFX4
  FOREIGN BUFX4 ;
  SIZE   1.440   BY   3.330 ;
END BUFX4
MACRO BUFX8
  SIZE 2.88 BY 3.33 ;
END BUFX8
MACRO INVX1
  SIZE 0.9605 BY 3.33 ;
END INVX1
MACRO INVX2
  SIZE 0.9605 BY 3.33 ;
END INVX2
MACRO INVX3
  SIZE 0.9605 BY 3.33 ;
END INVX3
MACRO NANDX1
  RESIZE 9 BY 9 ;
  SIZE 1.92 BY 3.33 ;
END NANDX1
MACRO NANDX2
  SIZE 1.92 BY 3.33 ;
END NANDX2
MACRO DFFX1
  SIZE 6.24 BY 3.33 ;
END DFFX1
MACRO DFFX2
  SIZE 6.24 BY 3.33 ;
END DFFX2
MACRO TIEHI
  SIZE 0.96 BY 3.33 ;
END TIEHI
MACRO TIELO
  SIZE 0.96 BY 3.33 ;
END TIELO
MACRO ONLYTT1
  SIZE 0.48 BY 3.33 ;
END ONLYTT1
MACRO ONLYTT2
  SIZE 0.48 BY 3.33 ;
END ONLYTT2
MACRO NOSIZE
  CLASS CORE ;
END NOSIZE
MACRO FILL1
  CLASS CORE SPACER ;
  SIZE 0.48 BY 3.33 ;
  PIN VDD
    DIRECTION INOUT ;
    USE POWER ;
  END VDD
  PIN VSS
    USE GROUND ;
  END VSS
END FILL1
MACRO FILL2
  SIZE 0.96 BY 3.33 ;

  PIN VDD
    USE POWER ;
  END VDD

  PIN VSS
    USE  GROUND  ;
  END VSS
END FILL2
MACRO TAP
  SIZE 0.96 BY 3.33 ;
  PIN VDD
    USE POWER ;
  END VDD
  PIN SUB
    USE SIGNAL ;
  END SUB
END TAP
MACRO EMPTY
  SIZE 0.96 BY 3.33 ;
END EMPTY
END LIBRARY
'''
NETLIST = '''module top (a, b, clk, z, \\bus[1] );
  input a;
  input b;
  input clk;
  output z;
  output \\bus[1] ;
  wire n1;
  wire \\esc.net[3] ;
  BUFX1 g1 (.A(a), .Y(n1));
  BUFX2 g2 (.A(n1), .Y(n2));
  BUFX4 \\g3.x[0]  (.A(n2), .Y(\\esc.net[3] ));
  INVX1 g4 (.A(\\esc.net[3] ), .Y(n4));
  INVX2 g5 ( .A( n4 ) , .Y( n5 ) );
  NANDX1 g6 (.A(n5), .B(b), .Y(n6));
  NANDX2 g7 (.A(n6), .B(1'b1), .Y(n7));
  DFFX1 g8 (.D(n7), .CK(clk), .Q(q8));
  BUFX8 g9 (.A(q8), .Y(z));
  BUFX1 g10 (.A(n1), .Y());
  BUFX1 clkbuf_1 (.A(clk), .Y(clk_1));
  ONLYTT1 g11 (.A(a), .Y(n11));
  BUFX1 g12 (.A({a, b}), .Y(\\bus[1] ));
  TIEHI g13 (.Y(hi));
  BUFX1 g14 (.A(hi), .Y(n14));	
  UNKNOWN g15 (.A(n14), .Y(n15));
  BUFX2 g16 (.A(n15), .Y(n16));
  BUFX4 g17 (.A(n16), .Y(n17));
 BUFX1 g18(.A(n17),.Y(n18));
  FILL1 fill_1 ();
endmodule
'''
PRECTS = NETLIST.replace('  BUFX1 clkbuf_1 (.A(clk), .Y(clk_1));\n', '')


def row(kind, net, pin, value, limit, slack):
    return dict(kind=kind, net=net, pin=pin, value=value, limit=limit, slack=slack)


SCENARIOS = {
    'empty': dict(tt=[], ss=[]),
    'mixed': dict(
        tt=[row('max_capacitance', 'n1', 'g1/Y', 0.12, 0.1, -0.02),            # BUFX1 -> BUFX2 (0.2*0.8 >= 0.12)
            row('max_capacitance', 'n1', 'g1/Y', 0.17, 0.1, -0.07),            # same driver: need 0.17 -> BUFX4
            row('max_transition', 'n2', 'g2/Y', 1.4, 1.5, 0.1),                # near limit, slew -> strongest
            row('max_capacitance', '\\\\esc.net[3] ', '\\\\g3.x[0] /Y', 0.39, 0.4, 0.01),  # strongest already
            row('max_capacitance', 'n4', 'g4/Y', 0.2, 0.1, -0.1),              # INVX1: INVX2 has no limit
            row('max_capacitance', 'n6', 'g6/Y', 0.05, 0.1, 0.05),             # below near limit: ignored
            row('max_capacitance', 'n6', 'g6/Y', 1, 1, 0),                     # integers, ratio 1.0, no family
            row('max_fanout', 'n1', 'g1/Y', 30, 20, -10),                      # other kind
            row('max_capacitance', 'q8', 'g8/Q', 0.3, 0.2, -0.1),              # sequential: no family
            row('max_capacitance', 'a', 'a', 0.5, 0.2, -0.3),                  # port-driven
            row('max_capacitance', 'clk_1', 'clkbuf_1/Y', 0.15, 0.1, -0.05),   # not in the pre-CTS netlist
            row('max_capacitance', 'n16', 'g16/Y', 0.19, 0.2, 0.01),           # near: BUFX4 covers 0.19 at 0.8? no -> strongest>need
            row('max_capacitance', 'z', 'g9/Y', 0.9, 0.8, -0.1),               # BUFX8 has no family (width)
            row('max_capacitance', 'n11', 'g11/Y', 0.2, 0.1, -0.1),            # cell missing in SS
            row('max_capacitance', 'n15', 'g15/Y', 0.2, 0.1, -0.1),            # unknown master: undriven
            row('max_capacitance', 'n18', 'g18/Y', 0.095, 0.1, 0.005),         # near only, upsize exists
            row('max_capacitance', 'n14', 'g14/Y', 0.12, 0, -0.1),             # zero limit: skipped row
            row('max_capacitance', 'n14', 'g14/Y', 0.12, None, -0.1)],
        ss=[row('max_capacitance', 'n1', 'g1/Y', 0.13, 0.11, -0.02),
            row('max_transition', 'n17', 'g17/Y', 2.0, 1.5, -0.5),             # strongest already: residual
            row('max_capacitance', 'n16', 'g16/Y', 0.3, 0.2, -0.1)]),
    'second_round': dict(
        tt=[row('max_capacitance', 'n1', 'g1/Y', 0.19, 0.2, 0.01),             # g1 already BUFX2 -> BUFX4
            row('max_capacitance', 'n2', 'g2/Y', 0.5, 0.4, -0.1)],             # g2 already BUFX4: residual
        ss=[]),
}
RESIZED = {'empty': {}, 'mixed': {}, 'second_round': {'g1': 'BUFX2', 'g2': 'BUFX4'}}


def write(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    if isinstance(text, bytes):
        path.write_bytes(text)
    else:
        path.write_text(text)


def expected(root, value, text=None):
    """Templated JSON/text: the root and the identities of root-bearing files become placeholders."""
    root = str(root)

    def walk(item):
        if isinstance(item, dict):
            if set(item) >= {'path', 'sha256', 'bytes'} and isinstance(item['path'], str) and item['path'].startswith(root):
                path = Path(item['path'])
                if root.encode() in path.read_bytes():
                    rel = os.path.relpath(path, root)
                    assert hashlib.sha256(path.read_bytes()).hexdigest() == item['sha256']
                    item = dict(item, sha256=f'@SHA:{rel}@', bytes=f'@BYTES:{rel}@')
            return {key: walk(v) for key, v in item.items()}
        if isinstance(item, list):
            return [walk(v) for v in item]
        return item.replace(root, '@ROOT@') if isinstance(item, str) else item
    return walk(value)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--recipe', type=Path, required=True)
    parser.add_argument('--rc-builder', type=Path, required=True)
    args = parser.parse_args()
    physical_inputs = load(args.recipe / 'native_physical_inputs.py', 'reference_physical_inputs')
    postroute = load(args.recipe / 'native_postroute.py', 'reference_postroute')
    builder = load(args.rc_builder, 'reference_rc_builder')
    for name in ('physical', 'postroute'):
        shutil.rmtree(HERE / name, ignore_errors=True)
    with tempfile.TemporaryDirectory(prefix='rustroad-step-fixtures-') as temp:
        root = Path(temp).resolve()
        # ---- physical inputs: synthetic PDK tree ----
        pdk = root / 'pdk/ihp-sg13g2'
        files = {
            'pdk/ihp-sg13g2/libs.ref/sg13g2_stdcell/lef/sg13g2_tech.lef': tech_lef(),
            'pdk/ihp-sg13g2/libs.tech/librelane/openrcx/ihp-sg13g2.nom.magic.rules': rules(1.0, 1.0),
            'pdk/ihp-sg13g2/libs.tech/librelane/openrcx/ihp-sg13g2.max.magic.rules': rules(1.13, 1.0),
            'pdk/ihp-sg13g2/libs.tech/librelane/openrcx/ihp-sg13g2.min.magic.rules': rules(0.9, 1.0),
            'pdk/ihp-sg13g2/libs.tech/librelane/openrcx/IHP_rcx_patterns.rules': rules(1.04, 1.02),
            'pdk/ihp-sg13g2/libs.tech/magic/ihp-sg13g2.tech': magic_tech(),
            'pdk/ihp-sg13g2/libs.tech/magic/ihp-sg13g2-extract.tech': magic_extract(),
            'cells-a.gds': gds(['BUFX1', 'INVX1', 'TAP'], dict(BUFX1=[8, 10])),
            'cells-b.gds': gds(['RAM', 'INVX1', 'AAA'], dict(RAM=[8, 30, 50])) + b'\0' * 6,
            'layers.map': layer_map(),
        }
        for rel, text in files.items():
            write(root / rel, text)
            write(HERE / 'physical/inputs' / rel, text)
        out = HERE / 'physical/expected'
        for corner in builder.CORNERS:
            for model in ('magic', 'openrcx-pdk'):
                builder.PDKS['ihp']['technology_lef'] = 'libs.ref/sg13g2_stdcell/lef/sg13g2_tech.lef'
                text = builder.render(builder.build('ihp', corner, root / 'pdk', model))
                write(out / f'rc-{corner}-{model}.json', text.replace(str(root), '@ROOT@'))
        text = builder.render(builder.build('ihp', 'nom', root / 'pdk', 'openrcx-pdk',
                                            pdk / 'libs.tech/librelane/openrcx/IHP_rcx_patterns.rules'))
        write(out / 'rc-nom-openrcx-pdk-patterns.json', text.replace(str(root), '@ROOT@'))
        (root / 'bundle').mkdir()
        bundle = physical_inputs.bundle([root / 'cells-a.gds', root / 'cells-b.gds'], root / 'bundle/combined.gds')
        write(out / 'bundle.json', json.dumps(expected(root, bundle), indent=2) + '\n')
        write(out / 'combined.gds', (root / 'bundle/combined.gds').read_bytes())
        write(out / 'layer-map.json', json.dumps(expected(root, physical_inputs.layer_map(root / 'layers.map')), indent=2) + '\n')
        # The whole step, with the recipe's v1 inputs (which also pin the builder script).
        keys = dict(std_gds='cells-a.gds', ram_gds='cells-b.gds', layer_map='layers.map',
                    tech_lef='pdk/ihp-sg13g2/libs.ref/sg13g2_stdcell/lef/sg13g2_tech.lef',
                    rules_nom='pdk/ihp-sg13g2/libs.tech/librelane/openrcx/IHP_rcx_patterns.rules',
                    magic_tech='pdk/ihp-sg13g2/libs.tech/magic/ihp-sg13g2.tech',
                    magic_extract='pdk/ihp-sg13g2/libs.tech/magic/ihp-sg13g2-extract.tech')
        config = dict(schema='rusteda.native-physical-inputs.v1', pdk_root=str(root / 'pdk'),
                      files={key: physical_inputs.identity(root / rel) for key, rel in keys.items()})
        config['files']['rc_builder'] = physical_inputs.identity(args.rc_builder)
        step = root / 'step'
        step.mkdir()
        result = physical_inputs.prepare(config, step)
        # The one record the native v2 inputs do not have.
        result = dict(result, inputs={key: item for key, item in result['inputs'].items() if key != 'rc_builder'})
        write(out / 'step-result.json', json.dumps(expected(root, result), indent=2) + '\n')
        write(out / 'step-stdout.txt', json.dumps(expected(root, result)) + '\n')
        for name in ('gds-inputs.json', 'gds-map.json', 'ihp-rc-nom.json'):
            write(out / 'step' / name, json.dumps(expected(root, json.loads((step / name).read_text())),
                                                  indent=1 if name.startswith('ihp') else 2) + '\n')
        write(out / 'step/combined-cells.gds', (step / 'combined-cells.gds').read_bytes())
        native = dict(config, schema='rusteda.native-physical-inputs.v2',
                      files={key: item for key, item in config['files'].items() if key != 'rc_builder'})
        write(HERE / 'physical/step-inputs.json', json.dumps(expected(root, native), indent=2) + '\n')

        # ---- post-route policy ----
        post = root / 'postroute'
        inputs = {'tt.lib': LIBERTY.format(corner='tt', c1='0.1', nand='!(A&B)', extra=ONLY_TT),
                  'ss.lib': LIBERTY.format(corner='ss', c1='0.09', nand='!( A & B )', extra=''),
                  'cells.lef': LEF, 'netlist.v': NETLIST, 'prects.v': PRECTS}
        for name, text in inputs.items():
            write(post / name, text)
            write(HERE / 'postroute/inputs' / name, text)
        out = HERE / 'postroute/expected'
        libs = {corner: postroute.liberty(post / f'{corner}.lib') for corner in postroute.CORNERS}
        sizes = postroute.lef_sizes(post / 'cells.lef')
        fams, member = postroute.families(libs, sizes)
        write(out / 'liberty.json', json.dumps(libs, indent=1) + '\n')
        write(out / 'sizes.json', json.dumps({name: list(size) for name, size in sizes.items()}, indent=1) + '\n')
        write(out / 'families.json', json.dumps(fams, indent=2) + '\n')
        instances = postroute.netlist_instances(post / 'netlist.v')
        write(out / 'instances.json', json.dumps({name: [master, pins] for name, (master, pins) in instances.items()},
                                                 indent=1) + '\n')
        runner = object.__new__(postroute.Runner)
        runner.libs, runner.member, runner.out = libs, member, post
        runner.c = dict(policy=dict(near_limit_ratio=0.9, target_ratio=0.8, max_rounds=2, max_resizes=256),
                        prects_netlist=str(post / 'prects.v'), profiles=dict(cell_lef=str(post / 'cells.lef')))
        _, names = runner.physical_models(post / 'netlist.v', post)
        write(out / 'physical-only.json', json.dumps(names) + '\n')
        for corner in postroute.CORNERS:
            write(out / f'{corner}-physical.lib', (post / f'{corner}-physical.lib').read_text())
        for name, corners in SCENARIOS.items():
            write(HERE / 'postroute/inputs' / f'rows-{name}.json',
                  json.dumps(dict(resized=RESIZED[name], corners=corners), indent=1) + '\n')
            ops, residual, skipped = runner.plan({co: dict(rows=rows) for co, rows in corners.items()},
                                                 post / 'netlist.v', dict(RESIZED[name]))
            write(out / f'plan-{name}.json', json.dumps(dict(ops=ops, residual=residual, skipped=skipped), indent=2) + '\n')
    print('fixtures written under', HERE)


if __name__ == '__main__':
    main()
