# Bounded geometry substage profiling

This timing-only change extends the existing `RUSTROAD_EXTRACT_PROFILE` opt-in
stderr record. Extraction algorithms, traversal order, numeric operations,
allocation/admission policy, public defaults and all original counters remain in
place. No worker or GPU implementation is added.

The existing parent `stage_ns.geometry_pieces_passive_crossings` timer is retained.
A separate geometry clock measures six fixed boundaries:

1. Initial owned shapes and virtual prefix construction.
2. Coarse instance index, owned insertion bins and net-index setup.
3. End extensions and via landing-pad pieces.
4. Routing patches.
5. Retained passive crossings and their strips.
6. Virtual passive crossing activation.

The new `geometry.stage_ns` fields contain these durations. The accompanying
`geometry.legacy_work_snapshots` contains work before geometry and after each
boundary. Snapshots copy existing work without changing it or adding work units.
Both methods return immediately when the existing profiling opt-in is disabled;
no extra clock is read in that case. There are no clocks or records in inner
loops. The separate clock never changes the parent timer's mark.

The reported parent duration includes profiling overhead. The sum of child
durations excludes the small gap before the child clock starts and the gap
before the parent boundary is sampled. These values are not a performance
improvement claim. Virtual-crossing cost remains unchecked until a qualified
replay after the active timed runs end.

## Upfront memory and work estimate

The complete private `PhaseProfile` state is estimated at352 B on x86_64 (old
state176 B plus one Instant, six u128 durations and seven usize snapshots,
including alignment). A compile-time assertion requires the complete state to
remain strictly below512 B. State is constant O(1), independent of shapes, nets,
layers, candidates, intervals, threads and corners. There is no path enumeration,
transcript or new concurrency. Sampling adds exactly one start and six boundary
clock reads for a successful opted-in extraction.

Only the opted-in report extends the existing bounded JSON object. All added keys
and text are fixed, with six durations and seven integer snapshots. A source
control checks the full newline-terminated record at maximum serializable integer
counters is strictly below4 KiB, retaining the original eight phase counters.
Reporting uses the existing serde_json Value path; conservative temporary
profiling/report overhead is below64 KiB and adds no algorithmic buffer. The
3584 MiB engine allowance and32 MiB query scratch are unchanged.

No build, format tool, test or extraction replay was run while the parent T1/C7
timed jobs were active. The source controls and compile-time assertion are pending
execution; do not describe them as passed. Static `git diff --check` is separate
from functional validation.

Later validation should use an isolated target, CARGO_BUILD_JOBS=1 and
RUST_TEST_THREADS=2. Prior qualified workspace tests peaked at1,055,456 KiB;
estimate this unchanged suite below1.5 GiB with the small fixed controls. Bound
tests at2 GiB/512 MiB swap/180s, builds at4 GiB/1 GiB swap/900s, and retain GNU-time
RSS plus the usual host-memory/disk preflight. Formatting, full workspace tests,
Clippy and docs remain required before a validated code claim.

The first timing replay must use a fresh private workspace copy, pinned identical
inputs/profile, unchanged original extraction limits, matched global threads and
CPU affinity, original process guard and GNU time. Require exact SPEF, electrical
stats and legacy work equality; only opt-in profile timings/state and the added
diagnostic fields differ. This timing-only candidate must not replace frozen T1
sources, engine, helpers, inputs or evidence. Architecture selection and any
parallel geometry code require the resulting substage measurements first.
