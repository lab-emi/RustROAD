#!/usr/bin/env python3
"""Rebuild the extractor ablation variants used by docs/extraction.md.

Each variant is the committed extractor with one modelling rule reverted:

* strict_plates: a plate needs to extend only beyond the strip edges
  (margin 1 DBU) instead of one strip width;
* any_overlap_plates: any overlapping conductor on the nearest other layer is
  a plate (owner: largest overlap), as before this stream;
* pads_ends: additionally the earlier split (substrate share kept, the excess
  to the plate, a crossing counted from the upper wire only), i.e. only the
  via pads, end extensions and covered sides of this stream.

usage: extraction_ablation.py --out DIR [--build]
Writes DIR/<variant>/src (a copy of the workspace crates with the patch),
DIR/<variant>/extract.diff and, with --build, DIR/<variant>/target/release/
rustroad (CARGO_BUILD_JOBS from the environment; run it under the usual
systemd-run memory guard).
"""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys

SRC = Path(__file__).resolve().parents[1]
PLATE_OLD = '''fn plate(
    layered: &[(u32, i64, i64, u32)],
    own: u32,
    upper: bool,
    minus: i64,
    plus: i64,
) -> Option<(u32, [u32; 2])> {
    let mut best: Option<(u32, i64, u32)> = None;
    for e in layered {
        if (upper && e.0 > own) || (!upper && e.0 < own) {
            let overlap = e.2.min(plus) - e.1.max(minus);
            let better = match best {
                None => true,
                Some((m, o, s)) => {
                    if upper {
                        (std::cmp::Reverse(e.0), overlap, std::cmp::Reverse(e.3))
                            > (std::cmp::Reverse(m), o, std::cmp::Reverse(s))
                    } else {
                        (e.0, overlap, std::cmp::Reverse(e.3)) > (m, o, std::cmp::Reverse(s))
                    }
                }
            };
            if better {
                best = Some((e.0, overlap, e.3));
            }
        }
    }
    best.map(|(m, _, s)| (m, [s, s]))
}
'''
SPLIT_OLD = '''                let delta = cg - cg_sub;
                let (d_below, d_above) = match plane_split {
                    (true, true) => {
                        let db = below_only.map_or(delta / 2.0, |r| sample(r, spacing).1 - cg_sub);
                        (db, delta - db)
                    }
                    (true, false) => (delta, 0.0),
                    (false, true) => (0.0, delta),
                    (false, false) => (0.0, 0.0),
                };
                let mut g = cg_sub;
                for (d, plane, upper) in [(d_below, below, false), (d_above, above, true)] {
                    if d < 0.0 {
                        g += d;
                    } else if d > 0.0 {
                        match plane {
                            Some((_, owners))
                                if upper && matches!(shapes[owners[k] as usize].owner, Owner::Run(_)) => {}
                            Some((_, owners)) => contributions.push((owners[k], d * length, 1.0)),
                            None => g += d,
                        }
                    }
                }
'''
def build(out, name, patch, compile_):
    d = out / name
    if d.exists():
        shutil.rmtree(d)
    (d / 'src').mkdir(parents=True)
    for item in ('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'crates'):
        src = SRC / item
        (shutil.copytree if src.is_dir() else shutil.copyfile)(src, d / 'src' / item)
    f = d / 'src/crates/rustroad-core/src/extract.rs'
    s = f.read_text()
    s = patch(s)
    f.write_text(s)
    (d / 'extract.diff').write_text(subprocess.run(['diff', '-u', str(SRC / 'crates/rustroad-core/src/extract.rs'), str(f)],
                                                   capture_output=True, text=True).stdout)
    if compile_:
        env = dict(os.environ, CARGO_TARGET_DIR=str(d / 'target'))
        subprocess.run(['cargo', 'build', '--release', '-p', 'rustroad-cli'], cwd=d / 'src', env=env, check=True)
def strict(s):
    assert s.count('let margin = plus - minus;') == 1
    return s.replace('let margin = plus - minus;', 'let margin = 1;')
def nearest(s):
    start = s.index('fn plate(\n'); end = s.index('fn sample(rows')
    return s[:start] + PLATE_OLD + '\n' + s[end:]
def oldsplit(s):
    start = s.index('                // Split this side\'s context value between the substrate and')
    end = s.index('                ground += g.max(0.0) * length;')
    return s[:start] + SPLIT_OLD + s[end:]
VARIANTS = dict(strict_plates=strict, any_overlap_plates=nearest,
                pads_ends=lambda s: oldsplit(nearest(s)))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[1])
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--build", action="store_true")
    args = parser.parse_args()
    for name, patch in VARIANTS.items():
        build(args.out.resolve(), name, patch, args.build)
    return 0


if __name__ == "__main__":
    sys.exit(main())
