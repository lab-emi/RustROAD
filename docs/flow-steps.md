# Native flow steps

Seven commands do, inside the `rustroad` executable, the file-level work that
a producer manifest runs between the engine commands: preparing PDK
collateral, running and recording the grouped antenna session, reviewing that
record, and the post-route electrical repair loop. They replace the Python
helpers of the Ibex SG13G2/EZ130 recipe (`native_physical_inputs.py` with
`scripts/build_rc_profile.py`, `antenna_session/native_antenna_groups.py`,
`antenna_session/verify.py`, `native_postroute.py`), which remain in the
RustEDA repository as the reference implementations.

| Command | Replaces | Opens a workspace | Runs child processes |
| --- | --- | --- | --- |
| `physical-inputs` | `native_physical_inputs.py` | no | no |
| `bundle-cell-gds` | its GDS merge | no | no |
| `gds-layer-map` | its layer-map projection | no | no |
| `rc-profile` | `scripts/build_rc_profile.py --pdk ihp` | no | no |
| `antenna-session` | `native_antenna_groups.py` | no | one `repair-antenna-groups` |
| `verify-antenna-lineage` | `antenna_session/verify.py` | no | no |
| `postroute-repair` | `native_postroute.py` (screen order) | no | `rustroad`, `rusteda`, `ruststa` |

Conventions shared by all of them:

- **Inputs** are one JSON file (`--inputs`) of pinned file records
  `{"path", "sha256", "bytes"}`, re-hashed before use, or plain file
  arguments for the three single-purpose commands. `--workspace` is not used.
- **Output directory** (`--out-dir`) must be fresh: `postroute-repair`
  requires that it does not exist; the others create it, take
  `workspace.lock` exclusively and fail with `REVISION_CONFLICT` if it holds
  anything else. A successful step leaves `HEAD` = `rev1` where the reference
  helper did.
- **Result**: one line of JSON on stdout (the step result; the same object is
  stored indented in the output directory). Errors are one JSON object on
  stderr, `{"status":"error","code":CODE,"message":...}`, exit status 2. A
  failed `postroute-repair`, `antenna-session` or `verify-antenna-lineage`
  also leaves its failure record (`result.json` with `status: "error"`,
  `failure.json`, `result.json` with `status: "rejected_or_unverified"`).
- **Bytes**: every JSON file these commands write is what CPython's `json`
  module writes for the same data (key order, `repr(float)`, `ensure_ascii`
  escapes, indentation), because later steps pin these files by SHA-256 and
  two implementations must not disagree about them. `crates/rustroad-pyjson`
  is that writer and an order-preserving reader; its tests compare against
  strings written by CPython.
- **Engine work is not linked in.** A step that needs the timing engine or
  another `rustroad` run starts the pinned executable named in its inputs and
  reads the files that process wrote. `rustroad` does not link RustSTA.

## `physical-inputs`: cell GDS bundle, GDS layer map, nominal RC profile

```sh
rustroad physical-inputs --inputs physical-inputs.json --out-dir backend/profiles
```

`rusteda.native-physical-inputs.v2`:

```json
{
  "schema": "rusteda.native-physical-inputs.v2",
  "pdk_root": "/path/to/IHP-Open-PDK",
  "files": {
    "std_gds": {"path": "...", "sha256": "...", "bytes": 0},
    "ram_gds": {}, "layer_map": {}, "tech_lef": {},
    "rules_nom": {}, "magic_tech": {}, "magic_extract": {}
  }
}
```

Exactly these seven records. `rules_nom` must be one of the PDK's two nominal
calibration tables (`IHP_rcx_patterns.rules` or
`ihp-sg13g2.nom.magic.rules`) and the two Magic files the PDK's own, under
`pdk_root/ihp-sg13g2`. The directory then holds `combined-cells.gds`,
`gds-inputs.json`, `gds-map.json`, `ihp-rc-nom.json`, `result.json`
(`status: "prepared_not_qualified"`, the input records and the three artifact
identities), `HEAD` and `workspace.lock`. The v1 schema of the Python helper
additionally pinned `rc_builder`, the profile script it imported; that record
is the only difference between the two helpers' `result.json`.

The three parts are also commands of their own:

```sh
rustroad bundle-cell-gds --gds std.gds --gds ram.gds --out combined.gds
rustroad gds-layer-map --map sg13g2.map --out gds-map.json
rustroad rc-profile --corner nom --pdk-root /path/to/IHP-Open-PDK \
  --resistance-model openrcx-pdk --rules IHP_rcx_patterns.rules \
  --technology-lef ez130_sg13g2_tech.lef --out ihp-rc-nom.json
```

- `bundle-cell-gds` copies every structure (`BGNSTR`..`ENDSTR`) byte for
  byte: the first library's header through `UNITS`, the structures sorted by
  name, `ENDLIB`. Libraries must have the same `UNITS` record (nothing is
  rescaled) and a structure name that occurs twice must have identical bytes.
  Caps: 32 MiB, one million records, 8192 structures. Result: `sources`,
  `output`, `structures`, `records`, `scope`.
- `gds-layer-map` projects a KLayout LEF/DEF layer map (`<layer> <purposes>
  <gds layer> <datatype>`) onto `rustroad.gds-layer-map.v1`: per LEF layer
  the pairs of its `NET`, `SPNET`, `VIA` and `PIN` purposes, the `NAME
  <layer>/PIN` label pair and the source lines; a layer whose name contains
  `Via` keeps only `VIA`; `DIEAREA` gives the boundary. `--layer` (repeatable)
  selects the layers; the default is the IHP SG13G2 stack Metal1..TopMetal2.
- `rc-profile` builds a `rustroad.rc-profile.v1` profile from the PDK data
  files only ([extraction.md](extraction.md) describes the model). It is the
  IHP path of `scripts/build_rc_profile.py`, value for value: `--corner`
  `min|nom|max`, `--resistance-model` `magic|openrcx-pdk`, optional `--rules`
  and `--technology-lef` overrides. Only `--pdk ihp` is built natively: the
  SKY130 and GF180 profiles in `fixtures/pdk` are still produced by the
  script, because their PDK data was not available to check the port against.
  The profile's `generator` field keeps the script's name, so a profile has
  the same bytes whichever implementation wrote it.

## `antenna-session`: one grouped repair session and its lineage

```sh
rustroad antenna-session --inputs antenna-inputs.json --out-dir backend/antenna-repair
```

`rusteda.native-antenna-inputs.v2` is unchanged from the Python runner:
`design_workspace`, `initial_revision`, the pinned `tool` (the `rustroad`
that runs the session), `limits` (`threads` 2, 4 or 16, `max_rounds` 1..3 and
the fixed work bounds) and `grouping` (the pinned policy of
[antenna-groups.md](antenna-groups.md)).

The controller and the engine are two processes. The controller writes
`native-plan.json`, runs

```sh
<tool> --workspace W --threads N --max-expansions 8000000 \
  repair-antenna-groups --base-rev R --plan native-plan.json
```

as its one child, stores that argv (`native-session-command.json`), the
child's stdout (`native-session.json`) and stderr, and then rebuilds the
record from the report files on disk: for every check report its canonical
selection (`<name>-selection.json`) and its header (`<name>-header.json`,
every top-level field except the raw rows), for every repair its selection,
`final-head.txt`, and `lineage.json`
(`rusteda.native-antenna-lineage.v4`): one typed entry per in-process
operation with the report's SHA-256, the attempt chain with its logical
revisions, the two materialized revisions' design files, the child's receipt
and the session accounting copied from the engine summary.

The session stays a child process on purpose. The lineage's `commands` entry
is a parent-side receipt (exact argv, exit code, captured bytes, wall time);
an engine that wrote its own lineage could only report these about itself,
and the record would no longer be the one `verify-antenna-lineage` and the
earlier runs' reviewers check. The cost is one extra process start and a
second read of the reports.

A failed engine child or any binding failure leaves `failure.json` and the
partial evidence; an unresolved native result is a successful step with
`status: "unresolved"` (foundry antenna signoff is a separate gate).

## `verify-antenna-lineage`: independent review of the session record

```sh
rustroad verify-antenna-lineage --inputs antenna-verification-inputs.json \
  --out-dir backend/antenna-verification
```

Inputs `rusteda.antenna-v3-independent-verifier-inputs.v2`: `lineage` and
`controller_inputs` (file records), `tool`, `design_workspace`,
`evidence_kind`. The `.v1` schema of the Python verifier, which also names a
`source_manifest` of pinned source files, is accepted unchanged (each listed
file is re-hashed, and the review repeats the record).

The review never runs an EDA command and trusts nothing the session wrote in
memory: `crates/rustroad-antenna-verify` depends on neither the engine crate
nor the flow crate, decodes every file again as generic JSON and re-hashes
every file identity, twice (when first used and at the end). In the order of
the reference verifier, with its rejection codes:

- the controller inputs, the session plan, and the single native process
  receipt: exact 12-element argv, exit code 0, captured stdout byte-equal to
  `native-reports/summary.json`;
- every operation: name, verb, report path, selection and base revision;
- every check report: rows streamed one at a time and counted per net against
  the coverage (`rows`, `shapes`, `gate_terminals`, observation), verdict
  counts, worst-net summary, status, work and report-memory ledger, selection
  fingerprint and physical-source fingerprint; its header file must equal the
  report's own top-level fields;
- the attempt chain: eligible nets recomputed from the planning check, the
  fresh pre-repair check, the repair report, work accounting, logical
  revisions (not materialized, a new physical source after each mutating
  repair), the committed final revision and its parent;
- inventories before and after (names, ids, counts, LEF sources re-hashed),
  the connectivity model, the final groups as an exact partition of the
  inventory, observation counts, unresolved and macro-touching nets;
- the report-byte and work ledgers recomputed from the reports.

`result.json` is `rusteda.antenna-v3-independent-lineage-review.v1` with
`status: "metadata_lineage_verified_requires_full_signoff"`: the records are
consistent with each other and with the files. It never says the layout is
antenna clean. `verifier_source` is the reviewing executable, `resources`
the applied 2 GiB address-space limit and the peak RSS. A rejection writes
`{"status": "rejected_or_unverified", "error": CODE}` and exits 2 with
`ANTENNA_LINEAGE: CODE`.

## `postroute-repair`: screen, equal-width upsize, ECO rebuild

```sh
rustroad postroute-repair --inputs postroute-inputs.json --out-dir backend/postroute
```

`rusteda.native-postroute-inputs.v3` is the Python runner's v2 without its
Python pins:

- `top`, `trial` (`workspace`, `revision` `rev4`), `final_workspace`
  (`<out-dir>/road`), `final_spef` (a fresh path beside the output
  directory), `prects_netlist`;
- `recipe`: the argument lists of the final `import`, `place`, `cts`, `route`
  and `extract` runs (`@NETLIST@` and `@SPEF@` markers);
- `screen`: file records of the trial SPEF, the extraction report and the
  producer's command record of that extraction;
- `antenna`: file records of the session `inputs` and `lineage`, and of the
  `verifier_inputs` and `verification` (both or neither);
- `profiles` (`sizing`, `delay`, `cell_buses`, `cell_lef`), `timing`
  (`std_tt`, `ram_tt`, `std_ss`, `ram_ss`, `sdc`), `tools` (`rustroad`,
  `rusteda`, `ruststa`), `policy` (`near_limit_ratio`, `target_ratio`,
  `max_rounds` <= 3, `max_resizes` <= 1024), `limits` (`threads`,
  `deadline_seconds`).

One command instead of a planning command plus manifest steps: the repair is
a data-dependent loop of zero to `max_rounds` rebuilds, each of which is
import, place, clock tree, ECO route, antenna session, review and extraction;
a producer manifest is a fixed list of steps and cannot express it, and the
result record lists the commands that actually ran.

The protocol, per round:

1. Bind the screened layout: the workspace HEAD, the SHA-256 of its
   `design.json` and `routes.rbin` (the design file must be the antenna
   lineage's final one), the extraction report naming this SPEF and RC
   profile, and the SHA-256 of the exact extraction argv. The trial is never
   extracted again.
2. Time it: `rustroad export-verilog`, `rusteda init` and `import` (TT and SS
   Liberty, stub cells for physical-only LEF macros, the SPEF at both
   corners), `ruststa run`, then the `summary` and `drv` reports of both
   corners (the four read-only queries run together).
3. Plan: rows of `max_capacitance`/`max_transition` at or above
   `near_limit_ratio` of the limit, or violating, ask for an upsize of the
   net's driver within its equal-footprint family (same pins, directions and
   output functions in both Liberty corners, same LEF width and height, no
   sequential, bus, clock-gating or `dont_use` cell). A transition row takes
   the strongest member; capacitance rows take the weakest member whose limit
   times `target_ratio` covers the load. Rows without an upsize are recorded
   as residual (violating) or skipped.
4. No resize: the final workspace is the screened one, hard-linked file for
   file with a fresh `HEAD` and lock, and the final SPEF is a hard link of
   its SPEF. Otherwise `apply-eco` writes the resized pre-CTS netlist, the
   recipe is replayed with the screened route as ECO prior, the rebuild is
   antenna-repaired (`antenna-session`), reviewed (`verify-antenna-lineage`),
   extracted with the same argument template, and screened again.

Outputs: `equal-width-sizing.json`, `check<n>/` (stub libraries, the STA
workspace), `logs/<name>.stdout|.stderr` of every child, `plan<n>.json`,
`netlist<n>.v`, `build<n>/`, `antenna<n>/` for rebuilds, `road/`, and
`result.json` (`rusteda.native-postroute-result.v2`: status `clean`,
`repaired`, `residual_violations` or `round_budget_exhausted`, every screen,
round, resize and command). Nothing here is signoff: setup and hold are
recorded, not repaired, and the timing, DRC, LVS and antenna gates remain
separate. The earlier order of the recipe, in which the runner extracted the
pre-antenna trial itself, is not ported.

## Byte identity with the reference helpers

Checked on the Ibex SG13G2/EZ130 run by replaying each step at one path with
the Python helper and with the native command (the same pinned engine
binaries as children), and by a complete producer run:

| Step | Files | Identical | Differences |
| --- | ---: | ---: | --- |
| `physical-inputs` | 7 | 6 | `result.json` and stdout: `inputs.rc_builder` absent |
| `antenna-session` | 1,545 | 1,542 | run times only: `wall_seconds` of the engine summary (2 files) and of `lineage.json`, and the summary's SHA-256 in the lineage |
| `verify-antenna-lineage` (v1 inputs) | 3 | 2 | `result.json`: `verifier_source`, `resources.peak_rss_kib_self` |
| `postroute-repair` | 84 | 79 | `result.json`: `input_manifest` (the v3 inputs file), `policy_source`, no `tools.python`, `wall_seconds`, `resources`; the timing engine's own time stamps (4 files of its workspace) |

The same differences appear between two runs of the Python helpers, except
the provenance records (`rc_builder`, `verifier_source`, `policy_source`,
`tools.python`, the inputs schema).

The rebuild path of `postroute-repair` does not occur on that run (its
status is `clean`). It is covered by the policy fixtures of
`crates/rustroad-flow/tests/fixtures/steps` (expected plans written by the
Python runner) and by the RustEDA repository's protocol test, which runs the
Python runner and this command against the same stand-in tools.

Tests: `crates/rustroad-pyjson/tests` (CPython-written numbers and
documents), `crates/rustroad-flow/tests/steps_reference.rs` (synthetic PDK
files, Liberty, LEF, netlists and DRV rows with expectations written by the
reference helpers; `tests/fixtures/steps/generate.py` regenerates them).
