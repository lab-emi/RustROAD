# GUI benchmark and acceptance plan

Status: plan only, written before a renderer or fixture generator is implemented.
All performance and platform results are currently **unverified**. Values below
are proposed acceptance targets and allocation budgets, not measurements.

## Questions the experiment must answer

1. Does a bounded, origin-rebased renderer remain responsive with a million
   source primitives and a much smaller visible set?
2. Does the shell materially change end-to-end interaction time or memory?
3. Are Linux/Vulkan and macOS/Metal correct under large coordinates, layer
   blending, Retina scaling, picking, resize and revision handover?
4. What is the cost of indexing, mmap faults, upload, synchronization and first
   useful presentation, beyond steady-state draw calls?

Use three isolated experiments with identical scene data and recorded input:
minimal winit + wgpu, egui/eframe + the same wgpu renderer, and GPUI with the same
CPU snapshot and visibility pipeline. GPUI's rendering path may require a
different adapter. Record that difference; do not attribute renderer differences
to shell overhead. If an equivalent GPUI batching boundary cannot be achieved
using its supported API, report the integration limitation rather than invent
a directly comparable score. Slint is an optional later experiment.

## Upfront memory estimate

For a first renderer fixture, cap source primitives at `N = 1,000,000`, use one
geometry revision, one visible analysis corner, two preparation workers, and
one active benchmark process. Immutable geometry/index data is shared, not
deep-cloned per worker. Two revisions may coexist only during a bounded handover.
The estimates below intentionally include both.

| Host allocation | Formula / cap | Estimated maximum |
| --- | --- | ---: |
| Canonical geometry and object handles, two revisions | `2 * N * 48 B` | 91.6 MiB |
| Flat hierarchy, two revisions | `2 * (2 * N) * 40 B` | 152.6 MiB |
| Primitive index arrays, two revisions | `2 * N * 8 B` | 15.3 MiB |
| Two worker scratch arenas | `2 * 64 MiB` | 128 MiB |
| Prepared/upload host staging | `3 * 16 MiB` | 48 MiB |
| Labels, glyph metadata and strings | hard byte cap | 64 MiB |
| Shell, allocator overhead, mappings and temporary decode reserve | reserve | 256 MiB |
| **Planned host allocations total** | sum, rounded up | **< 0.8 GiB** |

GPU allocation budgets are separate: resident primitive/cache resources 256 MiB,
render/presentation/ID attachments 160 MiB at up to 3840 x 2160 with no MSAA,
and in-flight upload buffers 48 MiB, totaling at most 464 MiB of application-
tracked GPU allocations. Account for driver and compositor allocations in
observed process/system memory; they are not included in these object formulas.
On unified-memory Macs, do not add overlapping RSS and GPU-accounting numbers
and call the sum physical memory. Record both counters and their definitions.

There are no graph paths, cuts, SAT clauses, timing tags, or netlist clones in
these fixtures. Polygon vertices count toward the primitive budget, and
tessellation has a separate bounded output budget; a single huge polygon cannot
bypass the limit. If the experiment adds timing overlays, first add an estimate
for nodes, arcs, corners, tags and any per-thread state. Bump neither cap nor
concurrency without revising this estimate.

Set a 2 GiB process RSS acceptance ceiling for this standard fixture. Set a
512 MiB application-tracked GPU allocation ceiling and emit high-water counters
for every resource class. Application counters cannot prove driver VRAM use;
collect vendor/platform memory telemetry when available and mark it unavailable
otherwise. Reject over-budget inputs before allocating; at runtime evict caches,
reduce detail or return a structured budget diagnostic.

## Workloads

Generate fixtures deterministically with a recorded seed and bounded output:

| Fixture | Purpose | Proposed cap |
| --- | --- | --- |
| Dense standard-cell-like rectangles, wires, vias across layers | Instancing and near-zoom traffic | 1,000,000 source primitives |
| Sparse view of the same design | Spatial-query scalability | <= 10,000 visible primitives |
| Whole-design overview | Bounded occupancy representation | <= 100,000 displayed aggregate primitives |
| Long wires, overlapping layers and large clipped polygons | Overdraw, tile duplication and mesh limits | 1,000,000 total input vertices/primitives |
| Large positive/negative coordinates and adjacent 1-DBU objects | Rebase/picking precision | Origins near +/- 10^12 DBU, checked arithmetic extremes separately |
| Repeated edits during pan and zoom | Cancellation and cache invalidation | Two live revisions, two workers, two pending viewport jobs |

Record label count independently; default display cap is 2,000 labels. Include
cases with no labels and with the cap reached. Have near-zoom tests that really
draw 100,000 visible primitives; do not use only sparse cases. Add one explicit
all-visible million-primitive stress case with aggregation disabled and treat
its result as characterization, not the initial interactive acceptance gate.
Budget fallback must be exercised and observable.

After synthetic correctness is established, add pinned, legally redistributable
real design snapshots from both sky130 and gf180mcu flows. Use the same revision,
layer visibility, scene crop, label policy and quality settings for every
candidate. Rendering a synthetic million-object fixture does not claim that the
current physical CLI supports a million-cell design; its input cap is separate.

## Measurement protocol

Pin the application commit, lockfile, compiler, fixture hash, OS version,
window-system session, adapter/driver, display refresh rate, physical framebuffer
size, scale factor, power mode, font assets and backend. Test Linux Vulkan on
both Wayland and X11 where supported, and actual Apple Silicon macOS/Metal.
Record Intel Mac status separately if that platform is a requirement.

For each run capture:

- Cold open: process start -> mapped revision -> spatial index ready -> first
  useful visible frame. Report file sizes and index-build time separately.
- Warm interactive work: replay pan, zoom, layer toggle, selection, inspector
  scroll and revision update; report p50/p95/p99 CPU frame time, missed frame
  deadlines, visible primitive count, LOD and uploaded bytes.
- GPU execution time when supported, and complete CPU-to-present behavior.
  CPU command submission is not a presentation timestamp. Use OS presentation
  telemetry or a reproducible input-to-photon measurement for latency; otherwise
  label this metric unavailable.
- Peak RSS, application GPU bytes, driver GPU telemetry when available,
  allocation failures, mmap page faults, cache hit rate and cancellation count.
- Idle CPU/GPU activity for 60 seconds after quiescence, and an idle-vs-active
  energy/power comparison when platform instrumentation is available.

Use release builds, fixed warm-up duration, at least five fresh-process runs and
the same 30-second input trace per workload. Save raw events and summary code.
Separate cold and warm caches. Report variance and all failures. Do not average
different GPUs or operating systems into a single ranking. Compare ordinary
use with capture/instrumentation enabled to quantify measurement overhead.

Proposed first gate on each named reference machine at 1920 x 1080, 60 Hz:

| Metric | Target, subject to measured baseline review |
| --- | --- |
| Warm pan/zoom with <= 100,000 displayed primitives | p95 total frame interval <= 16.7 ms; p99 <= 33.3 ms |
| Input to visibly updated viewport | p95 <= 50 ms, only when actually measured |
| First useful frame for 1,000,000 source primitives | <= 2 s on the pinned local-storage reference configuration |
| Host RSS | < 2 GiB |
| Application-tracked GPU allocation | <= 512 MiB |
| No-change steady state | Event-driven idle; investigate persistent redraw or worker activity |

Repeat at 3840 x 2160 and Retina scale with the same quality settings; initially
characterize those results rather than claiming the 1080p target guarantees 4K.
Choosing a lower LOD to meet a deadline must be reported, not hidden as a speedup.

## Correctness and independent evidence

CPU visibility, clipping and picking are deterministic references. Validate
integer boundary cases with analytic fixtures whose expected object IDs and
pixel-space positions are authored independently of the spatial index. Test
random views against bounded brute-force AABB selection on small fixtures.
Any GPU culling/aggregation added later needs explicit differential tolerances
and the CPU implementation must work first.

Compare raster output using defined edge tolerances, not universal byte-exact
image equality across GPUs. Overlays must respect integer source geometry,
deterministic ordering and revision identity. Test budgets, malformed geometry,
device loss, surface resize, missing fonts and stale jobs. Each platform also
needs manual IME/CJK, keyboard focus, high-DPI multi-monitor movement, trackpad
zoom, accessibility and window lifecycle checks. Mark missing checks unverified.

RTL equivalence, timing accuracy, DRC, LVS and GDS correctness are separate
engineering gates. A fast or attractive GUI is evidence for none of them.

## Resource guards and reporting

Before builds or fixture execution, inspect available host memory (`free -h` on
Linux), keep `CARGO_BUILD_JOBS=2`, `RUST_TEST_THREADS=2`, and run one GUI candidate
at a time. GUI builds must be separately estimated before adding dependencies;
the current small CLI build estimate does not cover a graphics stack.

Use a Linux child cgroup, preferably
`systemd-run --user --scope -p MemoryMax=4G -p MemorySwapMax=1G`, plus a 180-second
external wall-clock guard for one benchmark process and `/usr/bin/time -v` for
peak RSS. Verify that these controls are active; do not silently fall back to an
unguarded run. Do not use a small virtual-address `ulimit` for GPU processes,
whose drivers may reserve large address ranges. For macOS, establish a working
wall-clock watchdog and platform-appropriate process/resource supervision before
running; Linux cgroups are unavailable there. Always enforce the in-process
allocation caps and record macOS `/usr/bin/time -l` in its native units.
Serialize any campaign estimated above 4 GiB behind `.work/scale.lock`.

Save a machine-readable result with `status: verified|failed|unverified`, reason,
commit, dependency versions, fixture hash, platform, backend, resource caps,
metric units and raw-log paths. Missing GPUs, missing macOS access, missing
presentation instrumentation or unfinished adapters are `unverified`, never a
zero-time result or a pass. The shell choice remains provisional until these
results exist.
