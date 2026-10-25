#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# Reproduce the signoff baseline, planted controls and flow re-runs end to end.
# usage: scripts/signoff/reproduce.sh RUN_DIR [READY_DIR]
#   RUN_DIR   fresh output directory on disk (never under /tmp)
#   READY_DIR read-only raw flow run providing {sky130,gf180}.def and *_powered.v
#             (default /mnt/data2/cgao/git/RustROAD/.work/rc-timing/ready; it is copied, never mutated)
# Environment: RUSTROAD (default <main tree>/target/release/rustroad), STA_BIN (RustSTA 0.3 release dir).
set -euo pipefail
REPO=$(cd "$(dirname "$0")/../.." && pwd)
RUN=$(mkdir -p "$1" && cd "$1" && pwd)
READY=${2:-/mnt/data2/cgao/git/RustROAD/.work/rc-timing/ready}
RUSTROAD=${RUSTROAD:-/mnt/data2/cgao/git/RustROAD/target/release/rustroad}
STA_BIN=${STA_BIN:-/mnt/data2/cgao/git/RustSTA/.work/rc-wave/target/release}
case "$RUN" in /tmp|/tmp/*) echo "refusing RAM-backed /tmp" >&2; exit 2;; esac
free -h > "$RUN/free.txt"
B=$RUN/baseline
mkdir -p "$B/inputs" "$RUN/w4"
for f in sky130.def gf180.def sky130_powered.v gf180_powered.v; do cp "$READY/$f" "$B/inputs/"; done
(cd "$B/inputs" && sha256sum ./* > SHA256SUMS)
t() { local name=$1; shift; /usr/bin/time -v -o "$RUN/$name.time" "$@" > "$RUN/$name.out" 2>&1 || echo "$name exit=$?" >> "$RUN/exit-codes.txt"; }
cd "$REPO"
t identity python3 scripts/signoff/manifest.py verify --out "$RUN/identity"
for p in sky130 gf180; do
  t "ref_$p" python3 scripts/signoff/def2gds_reference.py --def "$B/inputs/$p.def" --pdk "$p" --out "$B/ref/$p.gds"
  t "signoff_$p" python3 scripts/signoff/signoff.py all --gds "$B/ref/$p.gds" --top lane_smoke --pdk "$p" \
      --netlist "$B/inputs/${p}_powered.v" --out "$B/signoff_$p"
  t "controls_$p" python3 scripts/signoff/controls.py --pdk "$p" --gds "$B/ref/$p.gds" --top lane_smoke \
      --netlist "$B/inputs/${p}_powered.v" --def "$B/inputs/$p.def" --baseline "$B/signoff_$p" --out "$B/controls_$p"
done
# Tampered manifest -> every verdict unverified.
python3 - "$REPO/fixtures/flow/signoff-pinned.json" "$RUN/tampered.json" <<'EOF'
import json, sys
m = json.load(open(sys.argv[1])); m["pdks"]["gf180"]["files"]["klayout_drc"]["sha256"] = "0" * 64
json.dump(m, open(sys.argv[2], "w"), indent=1)
EOF
t tampered python3 scripts/signoff/signoff.py all --gds "$B/ref/gf180.gds" --top lane_smoke --pdk gf180 \
    --netlist "$B/inputs/gf180_powered.v" --manifest "$RUN/tampered.json" --out "$B/tampered_gf180"
# Flow re-runs: historical frozen producer, pinned producer + cell policy + reference signoff,
# and the native-GDS code path (the reference streamout stands in for a native GDS).
common=(--rustroad "$RUSTROAD" --rusteda "$STA_BIN/rusteda" --ruststa "$STA_BIN/ruststa")
t flow_frozen python3 scripts/verify_routing.py --workspace "$RUN/w4/flow-frozen" --base-rev empty --producer frozen "${common[@]}"
t flow_policy python3 scripts/verify_routing.py --workspace "$RUN/w4/flow-policy" --base-rev empty --producer policy \
    --signoff-reference "${common[@]}"
t flow_nativepath python3 scripts/verify_routing.py --workspace "$RUN/w4/flow-nativepath" --base-rev empty --producer frozen \
    --signoff-gds "sky130=$B/ref/sky130.gds" --signoff-gds "gf180=$B/ref/gf180.gds" "${common[@]}"
# A planted (spacing-control) GDS posing as the native GDS: XOR differs -> gds failed -> status failed (exit 1).
t flow_nativebad python3 scripts/verify_routing.py --workspace "$RUN/w4/flow-nativebad" --base-rev empty --producer frozen \
    --signoff-gds "sky130=$B/controls_sky130/spacing.gds" --signoff-gds "gf180=$B/controls_gf180/spacing.gds" "${common[@]}"
t gap_crosscheck python3 scripts/signoff/gap_crosscheck.py --summary "$RUN/w4/flow-frozen/summary.json" \
    --gds "$B/ref/gf180.gds" --out "$RUN/w3"
t gate_whatif python3 scripts/flow_status.py "$RUN/w4/flow-frozen/summary.json" --assume-timing-passed
# Independent cell-policy checker: every instance syntax is seen, uncertifiable statements are unverified.
t cellpol python3 scripts/signoff/cellpol_controls.py --mapped-gf180 "$RUN/w4/flow-policy/gf180_mapped.v" \
    --mapped-sky130 "$RUN/w4/flow-policy/sky130_mapped.v" --out "$RUN/cellpol"
t unit python3 -m unittest discover -s scripts/tests -v
python3 scripts/signoff/evidence.py --run "$RUN" --out "$RUN/signoff-results.json"
echo "done: $RUN"
