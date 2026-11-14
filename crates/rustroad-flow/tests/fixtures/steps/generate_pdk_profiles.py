#!/usr/bin/env python3
"""Write the SKY130- and GF180-shaped RC profile fixtures with the reference builder.

usage: generate_pdk_profiles.py --rc-builder scripts/build_rc_profile.py

The inputs are synthetic: small files in the formats and the directory layout of the two PDK
trees the builder's `sky130` and `gf180` table entries name, with made-up numbers. The layer,
Magic type, plane and contact names are those of the table entries; the SKY130 tree gives its
via contacts through aliases, and the GF180 technology LEF says its parasitics are placeholders,
as the real files do. The expected profiles are what the unchanged reference builder writes for
them at the three corners. Everything is generated under a temporary root; in the stored
expectations that root is written as @ROOT@.
"""
import argparse
import importlib.util
from pathlib import Path
import shutil
import sys
import tempfile

HERE = Path(__file__).resolve().parent
VERSION = 'f6eeac7dad085ffcc829ccfd721f7b4ce39edcf7'
PDKS = dict(
    sky130=dict(
        base=f'sky130/versions/{VERSION}/sky130A',
        lef='libs.ref/sky130_fd_sc_hd/techlef/sky130_fd_sc_hd__nom.tlef',
        rules='libs.tech/librelane/rules.openrcx.sky130A.{corner}.calibre',
        magic='libs.tech/magic/sky130A.tech',
        routing=['li1', 'met1', 'met2', 'met3', 'met4', 'met5'],
        widths=[0.17, 0.14, 0.14, 0.3, 0.3, 1.6],
        types=['allli', 'allm1', 'allm2', 'allm3', 'allm4', 'allm5'],
        planes=['locali', 'metal1', 'metal2', 'metal3', 'metal4', 'metal5'],
        cuts=['mcon', 'via', 'via2', 'via3', 'via4'],
        # Magic contact type of each cut and the names its types line gives it.
        contacts=[('mcon', 'mcon,viali'), ('m2contact', 'm2contact,m2c,via1,v1'),
                  ('m3contact', 'm3contact,m3c,via2,v2'), ('via3', 'via3,v3'), ('via4', 'via4,v4')],
        valid=True),
    gf180=dict(
        base=f'gf180mcu/versions/{VERSION}/gf180mcuD',
        lef='libs.ref/gf180mcu_fd_sc_mcu7t5v0/techlef/gf180mcu_fd_sc_mcu7t5v0__nom.tlef',
        rules='libs.tech/librelane/rules.openrcx.gf180mcuD.{corner}',
        magic='libs.tech/magic/gf180mcuD.tech',
        routing=['Metal1', 'Metal2', 'Metal3', 'Metal4', 'Metal5'],
        widths=[0.23, 0.28, 0.28, 0.28, 0.44],
        types=['allm1', 'allm2', 'allm3', 'allm4', 'allm5'],
        planes=['metal1', 'metal2', 'metal3', 'metal4', 'metal5'],
        cuts=['Via1', 'Via2', 'Via3', 'Via4'],
        contacts=[('m2c', 'm2c,via1,v1'), ('m3c', 'm3c,via2,v2'), ('via3', 'via3,v3'), ('via4', 'via4,v4')],
        valid=False),
)


def load(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    sys.path.insert(0, str(Path(path).parent))
    spec.loader.exec_module(module)
    return module


def tech_lef(pdk):
    out = ['VERSION 5.7 ;', 'UNITS', '  DATABASE MICRONS 1000 ;', 'END UNITS', '']
    if not pdk['valid']:
        out += ['# This file has not correct Parasitic Information: the values are placeholders.', '']
    out += ['LAYER nwell', '  TYPE MASTERSLICE ;', 'END nwell', '']
    for i, name in enumerate(pdk['routing']):
        out += [f'LAYER {name}', '  TYPE ROUTING ;', f'  WIDTH {pdk["widths"][i]} ;',
                f'  RESISTANCE RPERSQ {0.125 - 0.02 * i:.4f} ;',
                f'  CAPACITANCE CPERSQDIST {2.5e-05 - 2e-06 * i:.3e} ;', f'  EDGECAPACITANCE {3.7e-05 - 2e-06 * i:.3e} ;',
                f'END {name}', '']
        if i < len(pdk['cuts']):
            cut = pdk['cuts'][i]
            out += [f'LAYER {cut}', '  TYPE CUT ;', f'  RESISTANCE {9.3 - 1.5 * i} ;', f'END {cut}', '']
    return '\n'.join(out) + 'END LIBRARY\n'


def rules(pdk, scale):
    count = len(pdk['routing'])
    out = ['Extraction Rules for OpenRCX', '', 'DIAGMODEL ON', f'LayerCount {count}', 'DensityRate 1  0', '']

    def table(header, rows, width):
        out.append(header)
        out.append(f'DIST count {len(rows)} width {width}')
        out.extend(' '.join(repr(v) for v in row) for row in rows)
        out.append('END DIST')
    for m in range(1, count + 1):
        w = pdk['widths'][m - 1]
        out += [f'Metal {m} RESOVER', f'WIDTH Table 1 entries:  {w}']
        table(f'Metal {m} RESOVER 0', [[w * (k + 1), 0.0, 0.0, 0.0005 * m] for k in range(2)], w)
        out += [f'Metal {m} OVER', f'WIDTH Table 1 entries:  {w}']
        for under in range(0, m):
            # A distant plate rated below the substrate row exercises the substrate-ratio rule.
            base = (2.3e-05 + 2.5e-06 * under + 1e-06 * m) * scale if under != 1 or m < 4 else 1.1e-05 * scale
            table(f'Metal {m} OVER {under}', [[w * (k + 1), 8.5e-05 * scale / (k + 1), base * (1 + 0.15 * k), 0.0]
                                              for k in range(4)], w)
        if m < count:
            out += [f'Metal {m} UNDER', f'WIDTH Table 1 entries:  {w}']
            for above in range(m + 1, count + 1):
                table(f'Metal {m} UNDER {above}', [[w * (k + 1), 7.5e-05 * scale / (k + 1), 1.7e-05 * scale * (1 + 0.1 * k), 0.0]
                                                   for k in range(2)], w)
            out += [f'Metal {m} DIAGUNDER', f'WIDTH Table 1 entries:  {w}']
            table(f'Metal {m} DIAGUNDER {m + 1}', [[w, 1e-06, 0.0, 0.0]], w)
        if 1 < m < count:
            out += [f'Metal {m} OVERUNDER', f'WIDTH Table 1 entries:  {w}']
            table(f'Metal {m} OVER {m - 1} UNDER {m + 1}', [[w * (k + 1), 6.5e-05 * scale / (k + 1), 3.1e-05 * scale, 0.0]
                                                             for k in range(2)], w)
        out.append('')
    return '\n'.join(out) + '\n'


def magic_tech(tag, pdk):
    out = ['tech', ' format 35', f' {tag}', 'end', '', 'types']
    for plane, name in zip(pdk['planes'], pdk['routing']):
        out.append(f' {plane} {name.lower()},{plane}wire')
    for index, (_, names) in enumerate(pdk['contacts']):
        out.append(f' {pdk["planes"][index]} {names}')
    out += ['end', '', 'contact']
    for index, (canonical, _) in enumerate(pdk['contacts']):
        out.append(f' {canonical} {pdk["planes"][index]} {pdk["planes"][index + 1]}')
    out += [' pc poly ' + pdk['planes'][0], 'end', '', 'extract', ' style ngspice variants (),(hrhc),(lrlc)', ' cscale 1', '']
    # The contact lines of the extract section use the short names of the builder's table.
    short = [names.split(',')[1] if canonical.endswith('contact') else canonical for canonical, names in pdk['contacts']]
    contacts = short + ['pc', 'ndc', 'pdc']
    for variant, r, blanket in (('()', 1.0, None), ('(hrhc)', 1.25, 15000), ('(lrlc)', 0.75, 0)):
        out.append(f' variants {variant}')
        for i, (mtype, plane) in enumerate(zip(pdk['types'], pdk['planes'])):
            out.append(f' resist ({mtype})/{plane} {round((125 - 20 * i) * r, 3)}   # mOhm/sq')
        for k, name in enumerate(contacts):
            out.append(f' contact {name} {blanket if blanket is not None else 9300 - 1500 * k}')
        out.append('')
    out.append(' variants *')
    for i, (mtype, plane) in enumerate(zip(pdk['types'], pdk['planes']), start=1):
        out.append(f' defaultareacap {mtype} {plane} {round(36.9 - 4.1 * i, 3)}')
        out.append(f' defaultperimeter {mtype} {plane} {round(40.7 - 2.6 * i, 3)}')
        out.append(f' defaultsidewall {mtype} {plane} {round(25.5 + 3 * i, 3)}' + (' -0.05' if i % 2 else ''))
        for j in range(1, i):
            out.append(f' defaultoverlap {mtype} {plane} {pdk["types"][j - 1]} {pdk["planes"][j - 1]} {round(55.3 - 6 * (i - j), 3)}')
    out += ['end', '']
    return '\n'.join(out)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--rc-builder', type=Path, required=True)
    args = parser.parse_args()
    builder = load(args.rc_builder, 'reference_rc_builder')
    target = HERE / 'pdk-profiles'
    shutil.rmtree(target, ignore_errors=True)
    with tempfile.TemporaryDirectory(prefix='rustroad-pdk-profile-fixtures-') as temp:
        root = Path(temp).resolve()
        for tag, pdk in PDKS.items():
            files = {pdk['lef']: tech_lef(pdk), pdk['magic']: magic_tech(tag, pdk)}
            for corner, scale in (('nom', 1.0), ('max', 1.12), ('min', 0.91)):
                files[pdk['rules'].format(corner=corner)] = rules(pdk, scale)
            for rel, text in files.items():
                for base in (root / 'pdk', target / 'inputs/pdk'):
                    path = base / pdk['base'] / rel
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text(text)
            for corner in builder.CORNERS:
                text = builder.render(builder.build(tag, corner, root / 'pdk'))
                path = target / f'expected/{tag}-rc-{corner}.json'
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text.replace(str(root), '@ROOT@'))
    return 0


if __name__ == '__main__':
    sys.exit(main())
