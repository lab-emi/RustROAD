#!/usr/bin/env python3
"""Rebuild the extractor ablation variants used by docs/extraction.md.

Each variant is the committed extractor with one modelling rule switched off:

* no_passive_cuts: pins, obstructions and supply metal do not count their own
  cross-section toward a routed conductor that crosses them;
* no_overlap_area: non-plate overlaps (stacked, offset or barely wider metal on
  another layer) add nothing, as before this round;
* split_only: both off, i.e. the previous round's model plus the
  storage-independent run splitting of this round;
* round1: the previous round's committed extractor (git revision ROUND1),
  rebuilt from git; it reads profiles without the overlap-area fields
  (rc_oracles.py --variant NAME=BIN:legacy).

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
ROUND1 = "0851d03"


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
def no_passive(s):
    call = "    add_passive_strips(input, limits, &mut built, &shapes, &bins, &mut stats)?;\n"
    assert s.count(call) == 1
    return s.replace(call, "")


def no_overlap(s):
    cond = "                && let Some(area_cap) = model.area_cap\n"
    assert s.count(cond) == 1
    return s.replace(cond, "                && let Some(area_cap) = model.area_cap.filter(|_| false)\n")


VARIANTS = dict(no_passive_cuts=no_passive, no_overlap_area=no_overlap,
                split_only=lambda s: no_overlap(no_passive(s)))


def build_round1(out, compile_):
    d = out / "round1"
    if d.exists():
        shutil.rmtree(d)
    (d / "src").mkdir(parents=True)
    archive = subprocess.run(["git", "-C", str(SRC), "archive", ROUND1, "Cargo.toml", "Cargo.lock",
                              "rust-toolchain.toml", "crates"], capture_output=True, check=True).stdout
    subprocess.run(["tar", "-x", "-C", str(d / "src")], input=archive, check=True)
    (d / "REVISION").write_text(ROUND1 + "\n")
    if compile_:
        env = dict(os.environ, CARGO_TARGET_DIR=str(d / "target"))
        subprocess.run(["cargo", "build", "--release", "-p", "rustroad-cli"], cwd=d / "src", env=env, check=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[1])
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--build", action="store_true")
    args = parser.parse_args()
    for name, patch in VARIANTS.items():
        build(args.out.resolve(), name, patch, args.build)
    build_round1(args.out.resolve(), args.build)
    return 0


if __name__ == "__main__":
    sys.exit(main())
