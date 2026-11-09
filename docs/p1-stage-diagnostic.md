# Stage-current placement diagnostic

This opt-in research command observes at most three current optimizer stage-end
iterates. It completes the ordinary global-best selection first, then applies
the existing retarget, legalize and detail helpers serially to those observations.
It never chooses a trial as the ordinary result, inserts physical cells, changes
the source HEAD, or asserts foundry/timing/equivalence acceptance.

```
rustroad --workspace /private/import --threads 2 place-p1-diagnose \
  --base-rev rev1 --out-workspace /private/new-observations --out-base-rev empty
```

Only a fresh disjoint output path is accepted. An existing different HEAD gives
`REVISION_CONFLICT`; an existing matching or partial output is not adopted.
The output base accepts the `empty` sentinel independently of source revision
handles. An existing published HEAD must be exactly one positive canonical
`revN` followed by one newline before a conflict is reported; malformed HEADs
remain invalid workspaces. The bounded read and both identity checks precede
any output creation, and the shared legacy source-base logic is unchanged.
Source SH and output EX locks span the calculation and commit. Source data must
be regular, unique-inode, read-only files below non-symlink ancestors. The
selected inline imported schema3/4 revision has exactly design.json and the four
canonical instance/connectivity side tables. Unknown files, routed/materialized
or seeded state, or unsupported metadata reject explicitly. Nonempty generated
tap/filler arrays reject in a lexical seed before their values are restored.

## Observed phases and authority

The ordinary legacy baseline is observed once, followed by the unchanged
ordinary selected geometry. Up to three reached stages retain their **current**
iterate, even if it differs from the optimizer's global best. Identical f64
payloads share storage but retain separate stage labels and stage reports.
Each reached stage then emits integer projection, after-retarget, after-legalize
and after-detail coordinates. A zero-horizon stage is not invented. A successful
late stall retains its actual reached current iterate; numerical/budget/observer
failures are unverified errors, never successful baseline fallback.

`observations.json` uses `rustroad.p1-stage-observations.v1` in its header. It
contains borrowed imported metadata, initial fixed-order packing, the exact
movable-to-instance mapping, native centre-net occurrence order and free
intervals, plus the already computed named/layered effective I/O rectangles,
followed by streaming global integer movable positions. Imported
master pin shapes, fixed placement and N/FS rows remain available for an
independent adapter. This raw document is not the separate tiny semantic schema.
Legal/detail orientations follow the actual row. Projected/retargeted targets
have no assigned physical orientation; any terminal-pin metric at those targets
needs a separately declared orientation scope. Only native centre cost is
reported here. Pin HPWL, displacement, DRC, LVS and timing remain unchecked.

Every set has a unique `position_set_id`; header and summary explicitly identify
`ordinary_selected` as the ordinary reference. The writer checks required
baseline/default and reached-stage phase order before commit. Effective I/O
is retained only in diagnostic preparation, with<=4096 bits and<=1MiB aggregate
name/layer bytes admitted before the existing helper clones names. The ordinary
None path retains no additional I/O view. Both admission-record passes and the
retained-record visit are charged; no I/O placement is recomputed for output.

Every observation includes actual cumulative defined work and numerical
counters. Reference geometry/status/selection equal the ordinary path when the
diagnostic completes; its serialized work does not equal an ordinary call:
diagnostic preparation and capture work already incurred are included. The
reference counters are frozen before the later trial/metric passes. Final
diagnostic counters include those later passes. No trial resets the meter.

The independently authored tiny oracle was frozen before this implementation:
25 cases, wide integer geometry/native comparability, 40 rejection controls.
Its data and API may be consumed; its validator implementation was deliberately
not read before source freeze. Existing self-authored numerical tests and a
successful compilation alone cannot qualify this change. Tiny independent
geometry, dual-PDK controls and the old qualified default geometry remain gates.

## Numerical admission and simultaneous buffers

Let O=N+F, P=all pin occurrences, p=modelled pin occurrences, B=4096,
T=2B(nx+ny), J=max_iterations and E=max_evaluations. The existing requested-work
bound is W=E*(p+D+T+10B)+20OJ+P*(J+2), where D counts both deposition directions.
The diagnostic explicitly admits **W+E+H**, with H=6O+6. The additional E covers
the evaluation-control charge omitted by the old conservative formula. H holds
up to three O-sized copies, 0+O+2O comparisons, and three entered/finished pairs.
Held units exclude competing work but are not completed work. Consumption is
atomic; unused hold is released on both successful and failed optimizer exit.
Retarget/legal/detail and all exact metric passes share the same remaining
20-billion total and existing numerical counters, without reserve reset.

The old engine model is 256O+64P+96M+112I+1MiB. The diagnostic adds at most48O
for three snapshots,48N for one serial trial's target/local/global trio, and
128KiB of writer/control scratch. The old model already includes the ordinary
baseline, translated view, optimizer histories/global-best outcome, ordinary
targets/global candidate and selected coordinates that can coexist with trials.
During each trial projected/retargeted locals are dropped before subsequent
locals, and float targets are dropped before detail. No DesignDb/PDK/library is
deep-cloned. Model/metadata/config borrows end before physical materialization;
the new command does not call that materialization at all.

At N=100000,F=4096,P=1000000,M=100000,I=250000, the admitted old+new engine
estimate is139224832 bytes. The selected workspace front door is narrower:
20000 cells,1024 rows,20000 nets,100000 pins,256 masters,131072 aggregate pin
shapes,200000 netlist records; every array also obeys one million aggregate
elements and two million nodes. Rows*(fixed+1)<=250000 is admitted **before**
initial packing; tap sites and blockage split bounds are separately admitted
before their diagnostic output vectors grow. Source JSON<=16MiB and all five
revision extents<=32MiB, with raw/decoded string, depth and duplicate-key limits
before typed restoration. Unsupported arithmetic rejects before legacy native
costs: primitives<=2^40, i64 midpoint/span safety and u64 cost/weight upper bounds.

The whole-process expected model is1850500864 bytes: parsed IR768MiB, executable
image128MiB, parser/physical preparation128MiB, numerical139224832 bytes,
source clean file cache256MiB, output cache64MiB, four8MiB stacks, allocator128MiB
and controller128MiB. This is a conservative engineering estimate, not a proof
about allocator or kernel memory. A separately authorized2GiB physical+512MiB
swap/no-AS foreground guard, measured RSS and cleanup are required. OOM or timeout
is unverified. No combinatorial paths or graph histories are materialized.
The new API requires an explicitly installed pool of at most four workers;
checking it does not initialize an unconstrained global Rayon pool. The CLI
rejects diagnostic thread counts above four before building its pool.

## Explicit byte passes and commit

One2GiB ledger covers named read/hash/scan/encode/write byte passes. The old
loader's admitted read/mapped extents are charged once, not misrepresented as
syscall tracing. Lexical nodes, initial packing expansions and the bounded
physical preparation's defined record/tap visits are carried into the numerical
total. Existing loader/fixed validation retain their own finite work guards;
the reported number is not every CPU instruction.

Each observation file is<=60MiB, each summary/failure/CLI JSON<=64KiB, and the
entire output inventory remains below64MiB and16files. The193MiB held tail is
released once the variable writer closes, then pays actual observation
read/SHA<=120MiB, one source read/SHA<=64MiB+128bytes, summary encode/write/read/SHA
<=256KiB, ten HEAD encode/write units, and admits the fixed CLI<=128KiB tail.
The receipt labels its charged total before CLI stdout; it does not call held
or unexecuted work completed. Errors report attempted/charged scope.

All variable-sized validation, source/namespace/identity checks and counter
admission precede HEAD rename. Precommit failures retain private partial files
without HEAD. Rename/fsync/kill/stdout uncertainty may leave a **provisional**
HEAD; only actual exit0, the typed completion receipt and current source/output
identities establish diagnostic completion. HEAD is never a physical design
revision or acceptance gate. There is no rollback loop or failed-evidence deletion.

## Source and execution gates

The source-only freeze, formatter logs and mechanical default-seam review are
separate from later compiler/test qualification. A fresh isolated target and
owned guards are required for every future build stage. No EDA/native/reference
execution or promotion is authorized by the presence of this source document.
The old qualified P1 checkouts, binaries and all failed research receipts remain
unchanged. In particular, inherited OpenROAD exit42 remains unverified.
