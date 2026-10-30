# GDSII streamout resource contract

Written before implementing `rustroad export-gds`, its record codec and its
verification harness (AGENTS.md Rule 9). Run receipts are kept as historical
evidence in `docs/evidence/gds-verification.json`.

## Record codec (`rustroad-formats::gds`)

Inputs are standard PDK cell libraries: `sky130_fd_sc_hd.gds` is 4,194,544 bytes
(307,425 records, 446 structures, ~56,100 elements) and
`gf180mcu_fd_sc_mcu7t5v0.gds` is 1,450,948 bytes (100,021 records, 230
structures). The reader works on one borrowed byte buffer and keeps one record
of lookahead. No graph, path, cut or clause is materialized.

- Index mode (used by `export-gds`) validates every record and element grammar
  but retains only structure names, byte ranges and distinct child names:
  input bytes + ~200 bytes/structure. Sky130: 4.2 MB + 0.1 MB.
- Full mode (tests, independent tools) retains elements. Conservative charge:
  160 bytes per element + 8 bytes per point + string bytes. Sky130: 56,100 x 160
  + ~0.8 M points x 8 = ~15 MB.
- Hard caps (`GdsLimits::default()`): 256 MiB input, 16 M records, 200,000
  structures, 8 M elements, 8,191 points per element (the record-size bound),
  64 M retained points, 1,024-byte names/strings, 256 properties per element,
  reference depth 64. Reference closure is an iterative DFS with colour marks
  (cycle guard) and the depth cap; work is O(structures + references).
- Worst case at the caps: 256 MiB input + 8 M x 160 B + 64 M x 8 B = ~2.0 GiB in
  full mode; index mode stays at ~input size. `export-gds` only uses index mode.

## `export-gds`

Per call: one workspace snapshot (existing loader, metadata cap 16 MiB),
the cell GDS buffer (cap 256 MiB, actual 4.2 MB), the cell LEF re-read for the
FOREIGN binding check (cap 64 MiB, actual 1.4 MB; LEF reader AST budget 256 MiB,
expected < 64 MiB for real PDKs), the layer map (cap 1 MiB) and a streaming
writer. Output is written through a buffered, byte-counted, hashing writer to a
temporary file and renamed; it is never held in memory. Output cap: 256 MiB and
16 M records (`--max-gds-bytes`, `--max-gds-records`).

Top-cell element count = instances + routed wires + routed vias + supply wires
+ supply vias + pin shapes + labels + 1. For the pipeline fixture: ~131 + 4,964
+ 101 + 10 + 9 + 24 + 12 + 1 (Sky130). One corner, one thread: the export is a
single linear pass, O(elements). Expected peak RSS for the fixture < 128 MiB.

## Tests and builds

- Unit tests use self-authored streams < 64 KiB; the truncation test re-parses
  every prefix of a ~2 KiB stream (O(n^2) = ~4 M byte visits); the mutation test
  parses 2,000 deterministic single-byte corruptions of the same stream.
- The ignored real-PDK tests parse both PDK libraries in full mode (~20 MB).
- `CARGO_BUILD_JOBS=4`, `RUST_TEST_THREADS=4`: build peak expected < 3 GiB.
  All runs use `systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G
  timeout ...`; nothing here approaches the 4 GiB scale-lock threshold.

## Verification harness (`scripts/verify_gds.py`)

External tools run one at a time inside the pinned iic-osic-tools image with
`--memory=4g`: KLayout DEF reference streamout / strmxor / DRC / LVS, Magic
DRC/extraction and Netgen. Native exports run on the host under the cgroup cap above.
Workspaces are copies under `.work/` (never RAM-backed `/tmp`); the read-only
`.work/rc-timing/ready` run is never mutated.

The independent KLayout streamout counts hierarchy contents with a bounded
DAG reduction. Each unique cell is visited once; SREF and AREF multiplicities
are propagated without walking every instantiated leaf shape. It rejects
cycles, overflowing counts and configured cell, reference, edge, layer, work
and count-entry limits. A real nested-array fixture matches KLayout's flattened
shape iterator exactly. This reporting change does not alter written geometry.

The reference DEF metadata scan also streams bounded statements rather than
copying a complete COMPONENTS section: at most16GiB/file,16MiB/statement,
65536 statement lines,64M components and65536 distinct masters. Declared counts,
unterminated records and GF180 coordinate parity are checked. Large reference
streamout jobs may explicitly set `--memory-gib` (1..64) and `--threads` (1..64);
defaults remain4GiB and2 workers. Adapter hashes and the selected limits are
recorded in the receipt. These are validation/streamout limits, not a statement
that a full OpenC910 layout has passed signoff.
