# Disabled-by-default serial crossing diagnostic

Implementation/source review only. No compilation, Rust tests, native/EDA/helper,
real revision copying, stream implementation, GPU, new target or production
integration is authorized by this source checkpoint. Root source authorization
is g1-legacy-bridge-root-review01/SOURCE_IMPLEMENTATION_AUTHORIZED.json.

Upfront memory: retain original engine admission; extra staging is bounded to
64MiB. One canonical packet is allocated once, directly filled in fixed-endian
sections; at original component maxima it is33,016,192B. One reused u32 CSR
count/cursor vector is at most4,000,000B, original returned activation bits at
most125,000B, bounded metadata/I/O<=1MiB. No second packet, full virtual geometry,
per-bin Vec, Q×I or pair vector. Runtime Vec capacities are checked. Tiny input
Q<=65,536, I<=100,000, T<=1024, R<=16,384, B<=1M, E<=4M, V<=1M. A larger real
query stream is explicitly unsupported by this first implementation.

Before lowering, estimate/check packet, CSR, output and remaining original work.
Use fallible allocations and structured budget errors. Capture/control and I/O
reserves lower the remaining original limit, never enlarge it. No hidden work
reset. Snapshot/ELF/profile bytes are admitted from metadata before large hashes.
Maximum engine memory3584MiB and work100B remain; selected tiny cases use the
original512MiB/200M limits. Ordinary extraction has no new defaults or report
fields. The capture feature is absent from default features.

Input preprocessing and geometry construction are shared with production,
without copies of their implementation. The additional entry exits immediately
after one call to the original serial virtual_crossings function; its entire
function/body bytes stay identical to32ced98. Source body SHA194d67d08dfc489884d9a55c27e414b1dcbcec06513eefb392515f931dd4f1cc.
Both global/query/crossing workers must be1. Bits are exported directly from the
actual returned Vec, with exact before/after legacy work and passive-strip counts.

The input workspace remains under a shared lock and all7 selected files/HEAD
are hashed before/after. Output is a fresh, disjoint locked workspace with
--base-rev empty. Typed stdout pins the complete immutable capture result after
rev1 publication; actual exit0 and an outer real command/time/cleanup receipt
remain required. Neither report nor native source can claim its own future exit.
Published R16 HEAD is never changed. Real rev4 needs a separately reviewed private
selected-revision preparation and a later one-foreground stream extension.

Future build proposal: isolated target<=2GiB disk, expected peak<=3GiB,
MemoryMax4GiB/SwapMax1GiB/job1/tests2/900s, explicit GNUtime and root CPU/disk
coordination. Timeout/resource failure remains unverified. No target exists yet.

Implemented protocol scope is tiny only: engine memory<=512MiB/work<=200M,
Q<=65,536 and the original tiny per-object limits. The core capture also
checks original100B/3584MiB ceilings, but its workspace/CLI endpoint does not
admit real Ibex. Later large tape capture requires a new reviewed extension.
The new CLI deliberately uses one strict hash-bound job instead of duplicate
source/limit flags from the unexecuted plan arrays:

`rustroad --workspace NEW_OUTPUT --threads 1 --max-cells 2000 --max-rows 128 capture-crossings --base-rev empty --inputs JOB --inputs-sha SHA`

JOB schema rustroad.legacy-crossings-inputs.v1 contains source_workspace,
source_revision, exact seven source_files FileIDs, profile FileID, corner,
actual frozen producer ELF FileID, source_manifest FileID, explicit limits
(memory/work/shapes/virtual/bin/instance-bin/nodes/elements/couplings/candidates/
max_segment_um/min_coupling_ff), and workspace_limits matching actual CLI.
Unknown/duplicate properties are rejected. No real job exists before source
review/build/freeze. The source-manifest FileID is a root-reviewed build binding,
not a runtime claim that an arbitrary JSON was independently qualified.

Bootstrap is bounded64KiB job +16KiB argv +64B HEAD and bounded filesystem
metadata before the no-large-read I/O reservation. Subsequent SHA/read/write
work uses64-byte logical units; JSON receipts/stdout have fixed sizes64KiB/8KiB
to avoid self-referential length accounting. Reserved, actually completed before
receipt, and conditional-on-success totals are distinct. Require exact typed
stdout +real exit0 +HEADrev1; nonzero/partial attempts never pass. File/parent
fsync, source snapshot and complete process startup/exit/RSS belong in the outer
actual timed receipt, not only the internal before-receipt timer.

Source02 clarification (source01 remains preserved): Common a8 I/O ALSO uses
ceil(n/64) blocks, not byte units; the earlier instruction/wording was wrong.
The two ledgers have different pass/control schedules and remain separate.
Profile loader charges are explicitly read+SHA+decode3 in addition to the outer
fingerprints. Each of the seven existing-loader files has a named one-unit-per
64B logical fee, not a claim to count every actual loader SHA/mmap/model traversal
or CPU instruction. The unchanged loader retains its own hard object/work guards,
and full outer time includes it. Fixed4096 covers bounded argv/stat/metadata and
small HEAD controls (source initial/final, shared loader, output final); it is a
defined bounded fee, not measured syscall count. Source01's under-specified fee
label and incorrect Common-unit prose do not modify any a8 code or old evidence.
