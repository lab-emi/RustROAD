# GUI and layout renderer decision

Research date: 2026-10-23. Status: proposed architecture; no GUI implementation,
GPU benchmark, or macOS execution has been completed in this repository.

## Decision

Start with a **custom wgpu layout renderer inside an egui/eframe shell**. Keep
the renderer's scene preparation independent of both libraries. Use native
Metal through wgpu on macOS and Vulkan on Linux. Treat GPUI as a serious second
shell candidate, subject to the same workload benchmark. There is currently no
evidence that any of these frameworks is the fastest for RustROAD's workload.

The reason for this starting point is control over the dominant work: visible
geometry selection, level of detail, GPU buffer residency, and draw batching.
The shell provides menus, inspectors, searchable lists, keyboard commands, and
accessibility. It must not create one widget or retained UI entity per cell,
wire segment, via, or polygon.

wgpu documents native Metal and Vulkan backends. egui-wgpu exposes preparation
callbacks, retained GPU resources, and custom commands in the rendering pass;
eframe exposes device/adapter/surface configuration. This gives an explicit
integration boundary for the canvas. These are capability statements, not
measured RustROAD performance claims. Sources: [wgpu platform support](https://github.com/gfx-rs/wgpu#supported-platforms),
[egui-wgpu callback API](https://docs.rs/egui-wgpu/latest/egui_wgpu/trait.CallbackTrait.html),
[eframe native options](https://docs.rs/eframe/latest/eframe/struct.NativeOptions.html).

## Candidate comparison

| Candidate | Verified capability | RustROAD assessment |
| --- | --- | --- |
| GPUI | Hybrid retained/immediate UI, imperative elements, macOS Metal rendering, documented macOS/Linux support. The published 0.2.2 documentation says it is pre-1.0 and breaking changes occur. | Good candidate for a polished desktop shell. Its canvas API provides custom drawing, but that alone does not establish a stable arbitrary-wgpu-buffer integration. Prototype the actual batching boundary before adopting it. |
| wgpu + winit | wgpu supplies portable native graphics; winit supplies windows and input events, not widgets or rendering. | Maximum ownership of presentation and event-loop scheduling. More work for text editing, focus, accessibility, inspectors, and desktop integration. Useful as a minimal renderer benchmark control. |
| egui/eframe + custom wgpu | Cross-platform native shell, documented custom wgpu callbacks, configurable graphics setup. | Recommended first experiment: quickest direct route to a custom renderer while retaining useful developer-facing controls. Virtualize tables; do not submit millions of egui shapes. |
| Slint | Desktop Linux/macOS support; several renderer choices including Metal/Vulkan paths; WGPU interoperability has version-specific unstable feature flags. | A viable product UI option, but does not remove the need for the custom layout renderer. Additional interface and license choices make it less attractive for the initial renderer experiment. |

GPUI evidence: [published API overview](https://docs.rs/gpui/0.2.2/gpui/),
[canvas API](https://docs.rs/gpui/0.2.2/gpui/fn.canvas.html).
winit evidence: [window/event/rendering responsibilities](https://docs.rs/winit/0.30.13/winit/).
egui evidence: [supported native platforms](https://github.com/emilk/egui),
[eframe API](https://docs.rs/eframe/latest/eframe/).
Slint evidence: [desktop platforms](https://docs.slint.dev/latest/docs/slint/guide/platforms/desktop/general/),
[renderers](https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backends_and_renderers/),
[WGPU backend selection](https://docs.slint.dev/latest/docs/rust/slint/struct.BackendSelector).

The version numbers above identify pages observed during research. They are not
an instruction to mix independent latest crate versions. Before implementation,
select a compatible egui/egui-wgpu/eframe/wgpu dependency set, pin it and the Rust
toolchain in the lockfile, and record exact versions in every result. Keep any
GPUI experiment independently pinned; do not depend on a moving Git branch.

## Data and crate boundaries

The following are proposed boundaries, not existing crates:

```text
immutable workspace revision (i64 DBU, typed object handles)
    -> renderer-independent display snapshot / spatial index
    -> bounded visible primitive batches / local-coordinate upload buffers
    -> graphics renderer (wgpu)
    -> shell integration (egui/eframe first; GPUI experiment separately)

RustSys / RustSTA / physical engines -> committed workspace revisions
```

Core and workspace crates stay free of GUI dependencies. UI state holds handles,
viewport state and selections, never ownership of the mutable physical graph.
Use an immutable revision handle shared across preparation workers. A short
shared workspace lock selects a consistent committed revision; subsequent
rendering reads its immutable tables. GUI edits invoke the same revision-checked
mutation API as the CLI, including `--base-rev`; conflicts must be visible.
There is no GUI-required daemon, hidden HTTP server, or blocking stdin prompt.

A display snapshot separates instances, rectangular shapes, route segments,
polygons, text and overlays into typed contiguous buffers. Use a per-design
interner for labels. Store stable object IDs alongside draw instances, so
selection resolves to the source object without a parallel copied netlist.
Keep at most two display revisions live during handover and account for both.

## Exact geometry and display precision

All authoritative placement, geometry, bounding boxes, hit-test coordinates,
DRC coordinates and exported data remain `i64` database units. Camera origin is
also an integer DBU point. Rendering is a derived approximation:

1. Subtract the camera or tile origin using checked `i128` intermediates.
2. Clip/cull against an integer viewport conservatively expanded for stroke
   width. Use a finite `f64` scale for screen conversion and reject invalid or
   unrepresentable transforms.
3. Convert only the small viewport-local results to `f32` GPU coordinates. Split
   oversized primitives or rebase again when a documented pixel-error bound
   would otherwise be exceeded.
4. Perform exact geometric selection against original integer geometry. A GPU
   ID buffer may nominate candidates; it cannot decide a DRC or geometry edit.

Never cast an absolute whole-chip DBU coordinate directly to `f32`. Test adjacent
DBUs around large positive and negative origins, boundary-touching geometry and
zoom extremes. Floating-point rasterization must never rewrite the database.

## Bounded canvas preparation

Use a flat immutable spatial hierarchy with one canonical primitive record and
bounded index storage. A binary hierarchy has fewer than `2 * N` nodes. Every
query enforces node-visit and output caps. A giant wire or polygon must not be
copied into every tile it crosses; clip it during bounded visible preparation,
or put it in a bounded large-object list. Iterative construction and traversal
must check the size/work budget before allocation.

Screen tiles organize cache and raster work. They are not an instruction to
duplicate the full source geometry. Cache keys include revision, layer mask,
level of detail, scale bucket, tile origin and style generation. Maintain a
byte-limited LRU for geometry and textures; evict stale revisions promptly.

At close zoom, draw exact visible rectangles, wires and bounded polygon meshes.
At far zoom, use aggregated occupancy/density per layer instead of individually
drawing subpixel geometry. Preserve salient selected objects, macros, ports,
clock routes and violations as separate bounded overlays. Mark aggregation in
the UI; a heatmap is not a claim that every object is shown individually.
Precompute or incrementally update a bounded multiresolution representation,
so every overview frame does not scan the whole design.

Build batches by layer/style and preserve a deterministic transparency order.
Instance common rectangle/via templates. Tessellation has per-polygon and total
vertex/index caps; budget exhaustion yields a visible diagnostic and a bounded
outline/occupancy fallback. It never silently substitutes valid geometry in an
export. Allocate no per-object GPU buffer or bind group. Query the adapter's
actual limits and split buffers accordingly; a successful small allocation does
not prove that a larger one will fit. See [wgpu limits](https://docs.rs/wgpu/latest/wgpu/struct.Limits.html).

Label shaping and drawing happen only for visible, sufficiently large objects,
with a glyph-cache byte cap and a per-frame label count cap. Inspector tables
virtualize their rows. Schematic tracing and timing-path highlighting use
bounded results supplied by engines; never enumerate every graph path or cone
for the sake of visualization.

## Frame scheduling and interaction

The UI thread handles input and submits ready batches. Bounded worker tasks
prepare snapshots and spatial queries. Coalesce obsolete camera requests and
cancel work when the revision or viewport generation changes. Bound pending
work and upload queues. Camera motion should continue with the last complete
snapshot while the new revision is being prepared.

Avoid continuous work when idle. winit documents event-driven waiting for
applications that do not need constant redraws; request frames for input,
changed overlays, uploads and animations. CPU frame time, GPU time and actual
presentation/input latency are different measurements and must be reported
separately. Source: [winit event loop guidance](https://docs.rs/winit/0.30.13/winit/).

## Graphics versus EDA compute

wgpu is the **display** interface here. It does not replace the engineering
constitution's numerical compute path. Placement, routing, extraction and timing
first require deterministic pure-Rust `cpu_ref` implementations. Later Linux
acceleration uses native CUDA; macOS acceleration uses native Metal/Accelerate
with explicit ownership and synchronization. GPU culling or aggregation, if
added, also requires a CPU reference and differential coverage before adoption.

The initial boundary between EDA compute and drawing is an immutable CPU-visible
snapshot. Account for readback, copies, fences and synchronization in end-to-end
benchmarks. A future shared Metal resource path is a separate measured
optimization, not an assumed zero-copy property of Apple unified memory. Never
let an unfinished compute buffer become visible to the renderer.

Timing and violation overlays include revision, corner, wire model and
unconstrained-endpoint scope. A missing extractor or checker displays
`unverified`/`unchecked`; the GUI must not turn absent data into a zero.

## Licensing and adoption gate

GPUI's own package is Apache-2.0; do not infer the license of every Zed component
from it. wgpu and egui use permissive Apache-2.0/MIT licensing, and winit's
repository is Apache-2.0 with component-specific details. Slint offers GPLv3,
royalty-free and commercial alternatives; it is not simply an MIT/Apache-only
dependency. The royalty-free option has distribution conditions including
attribution. A dependency decision must audit the exact pinned dependency
graph, assets and font licenses. Sources: [GPUI license](https://github.com/zed-industries/zed/blob/main/crates/gpui/LICENSE-APACHE),
[wgpu license](https://github.com/gfx-rs/wgpu),
[egui license and fonts](https://github.com/emilk/egui),
[winit license](https://github.com/rust-windowing/winit),
[Slint license options](https://github.com/slint-ui/slint/blob/master/LICENSE.md).

Adopt a shell only after the [benchmark plan](gui-benchmark-plan.md) has usable
results on real Linux and macOS hardware. A Linux compile, headless render or
framework platform claim does not establish macOS presentation latency, IME,
Retina scaling, trackpad behavior, accessibility or power use. If GPUI wins the
same bounded workload and integration criteria, the separated snapshot design
allows a shell change without rewriting physical algorithms.
