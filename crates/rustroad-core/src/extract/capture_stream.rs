//! Bounded opt-in static/query capture, using the unchanged serial oracle once.
use super::capture::{Work, align, area, boxput, put, serial_source_pins, span, u32put};
use super::{
    ExtractInput, ExtractLimits, PreparedCrossings, budget, crossings, prepare_crossings_admitted,
};
use crate::{Orientation, Rect, Result};
use serde::Serialize;

const HEADER: usize = 4096;
pub const STATIC_BYTES: usize = 32 << 20;
pub const TAPE_BYTES: usize = 512 << 20;
pub const MAX_QUERIES: usize = 12_000_000;
pub const TILE_QUERIES: usize = 65_536;
pub const MAX_LIVE: u64 = 64 << 20;
const METADATA_BYTES: u64 = 8 << 20;
const MAX_I: usize = 100_000;
const MAX_T: usize = 1024;
const MAX_R: usize = 16_384;
const MAX_B: usize = 1_000_000;
const MAX_E: usize = 4_000_000;
const MAX_V: usize = 1_000_000;

/// Actual input-cache extents, admitted from complete bounded file inventories.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct PhaseInputs {
    pub source_bytes_per_copy: u64,
    pub producer_elf_bytes: u64,
    pub binding_file_bytes: u64,
}

/// Complete source geometry inventory; no per-query or per-instance transcript.
#[derive(Clone, Debug, Serialize)]
pub struct GeometryCounts {
    pub instances: usize,
    pub templates: usize,
    pub rectangles: usize,
    pub virtual_shapes: usize,
    pub bins: usize,
    pub entries: usize,
    pub queries: usize,
    pub virtual_front: usize,
    pub prefix_count: usize,
    pub grid_origin_bins: [i64; 2],
    pub grid_width: usize,
    pub grid_height: usize,
    pub coarse_bin_dbu: i64,
    pub layer_count: usize,
    pub routing_layers: Vec<u32>,
    pub static_bytes: usize,
    pub query_payload_bytes: usize,
    pub tape_bytes: usize,
}

/// One owner implements the bounded disk sink; errors stop before later stages.
pub trait CaptureSink {
    fn static_view(&mut self, data: &[u8], counts: &GeometryCounts) -> Result<()>;
    fn query_chunk(&mut self, first: usize, records: &[u8]) -> Result<()>;
    fn queries_complete(&mut self, total: usize) -> Result<()>;
    fn legacy_words(&mut self, words: &[u64]) -> Result<()>;
}

/// The conservative boundary estimate retains every original prepared owner.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct PhaseMemory {
    pub original_full_estimate_bytes: u64,
    pub original_working_bytes: u64,
    pub original_held_bytes: u64,
    pub original_geometry_bytes: u64,
    pub proved_late_coupling_reserve_bytes: u64,
    pub retained_parent_bound_bytes: u64,
    pub live_staging_bound_bytes: u64,
    pub source_cache_bytes: u64,
    pub producer_elf_cache_bytes: u64,
    pub binding_cache_bytes: u64,
    pub output_cache_bytes: u64,
    pub opaque_controller_kernel_allocator_bytes: u64,
    pub total_boundary_bound_bytes: u64,
    pub original_limit_bytes: u64,
}

/// Completed native result and separate extra-control counters.
#[derive(Debug, Serialize)]
pub struct StreamCounts {
    pub geometry: GeometryCounts,
    pub legacy_work_before: usize,
    pub legacy_work_after: usize,
    pub passive_strips_before: usize,
    pub passive_strips_after: usize,
    pub original_bitset_words: usize,
    pub active_bits: usize,
    pub control_reserved: usize,
    pub control_completed: usize,
    pub bit_validation_work: usize,
    pub core_operation_cap: usize,
    pub effective_legacy_cap: usize,
    pub staging_capacity_bytes: u64,
    pub prefix_memory: PhaseMemory,
    pub memory: PhaseMemory,
    pub serial_body_sha256: String,
    pub serial_function_sha256: String,
}

fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b)
        .ok_or_else(|| budget("stream phase arithmetic"))
}
fn phase_memory(
    memory: super::ExtractMemory,
    limits: &ExtractLimits,
    context: PhaseInputs,
    live: u64,
    static_bytes: usize,
    tape_bytes: usize,
) -> Result<PhaseMemory> {
    if context.source_bytes_per_copy > 128 << 20
        || context.producer_elf_bytes > 128 << 20
        || context.binding_file_bytes > 8 << 20
        || live > MAX_LIVE
    {
        return Err(budget("stream phase input/staging bounds"));
    }
    // prepare_crossings returns before the normal continuation creates its
    // coupling BTreeMap or final coupling Vec. No other original term is freed.
    let late = (limits.max_couplings as u64)
        .checked_mul(128)
        .filter(|&n| n <= memory.working_bytes)
        .ok_or_else(|| budget("stream late-reserve proof"))?;
    let retained = memory
        .estimate_bytes
        .checked_sub(late)
        .ok_or_else(|| budget("stream retained-parent proof"))?;
    let source_cache = context
        .source_bytes_per_copy
        .checked_mul(2)
        .ok_or_else(|| budget("stream source caches"))?;
    let pages = |n: u64| {
        n.checked_add(4095)
            .map(|v| v / 4096 * 4096)
            .ok_or_else(|| budget("stream cache pages"))
    };
    let output_cache = add(
        add(pages(static_bytes as u64)?, pages(tape_bytes as u64)?)?,
        1 << 20,
    )?;
    let opaque = 128 << 20;
    let total = [
        live,
        source_cache,
        context.producer_elf_bytes,
        context.binding_file_bytes,
        output_cache,
        opaque,
    ]
    .into_iter()
    .try_fold(retained, add)?;
    if total > memory.limit_bytes {
        return Err(budget("stream original memory cap cannot admit boundary"));
    }
    Ok(PhaseMemory {
        original_full_estimate_bytes: memory.estimate_bytes,
        original_working_bytes: memory.working_bytes,
        original_held_bytes: memory.held_bytes,
        original_geometry_bytes: memory.geometry_bytes,
        proved_late_coupling_reserve_bytes: late,
        retained_parent_bound_bytes: retained,
        live_staging_bound_bytes: live,
        source_cache_bytes: source_cache,
        producer_elf_cache_bytes: context.producer_elf_bytes,
        binding_cache_bytes: context.binding_file_bytes,
        output_cache_bytes: output_cache,
        opaque_controller_kernel_allocator_bytes: opaque,
        total_boundary_bound_bytes: total,
        original_limit_bytes: memory.limit_bytes,
    })
}

/// Lower a bounded stream, then call the original serial function exactly once.
///
/// Existing extraction APIs/defaults and their numerical continuation are unused
/// here. The caller supplies an already existing one-thread pool and admitted
/// cache extents; all other producer state stays under the original memory cap.
pub fn capture_cpu_ref(
    input: &ExtractInput<'_>,
    limits: &ExtractLimits,
    phase_inputs: PhaseInputs,
    sink: &mut impl CaptureSink,
) -> Result<StreamCounts> {
    if limits.query_workers != 1
        || limits.crossing_workers != 1
        || limits.max_work > 100_000_000_000
        || limits.memory_mib > 3584
        || rayon::current_thread_index().is_none()
        || rayon::current_num_threads() != 1
        || input.layers.is_empty()
        || input.layers.len() > 32
    {
        return Err(budget("stream requires original one-worker path"));
    }
    let mut prefix_memory = None;
    let mut prepared = prepare_crossings_admitted(input, limits, |memory| {
        // Source/ELF/binding caches already exist here. Admit them before
        // build_trees or any prepared geometry/index allocation. The only
        // credit is the unchanged continuation's absent late coupling reserve.
        prefix_memory = Some(phase_memory(
            memory,
            limits,
            phase_inputs,
            METADATA_BYTES,
            0,
            0,
        )?);
        Ok(())
    })?;
    let prefix_memory = prefix_memory.ok_or_else(|| budget("stream prefix admission"))?;
    let PreparedCrossings {
        ref mut stats,
        memory,
        ref shapes,
        ref mut bins,
        ref routing_layers,
        ..
    } = prepared;
    let source = shapes
        .source
        .ok_or_else(|| budget("stream shared geometry required"))?;
    let geometry = source.geometry;
    let instances = geometry.capture_instances();
    let templates = geometry.capture_templates();
    let (ni, nt, nv) = (instances.len(), templates.len(), shapes.virtual_count());
    if ni > MAX_I
        || nt > MAX_T
        || nv > MAX_V
        || input.layers.is_empty()
        || input.layers.len() > 32
        || shapes.capture_prefix().len() != ni + 1
    {
        return Err(budget(&format!(
            "stream admission I={ni},T={nt},V={nv}; R/B/E/Q unchecked; legacy not run"
        )));
    }
    let before = stats.work;
    let strips_before = stats.passive_strips;
    let mut work = Work {
        used: 0,
        cap: limits.max_work.saturating_sub(before),
    };
    work.charge(
        include_str!("../extract.rs")
            .len()
            .checked_mul(2)
            .ok_or_else(|| budget("stream source work"))?,
    )?;
    let (body_pin, function_pin) = serial_source_pins()?;
    let mut nr = 0usize;
    for template in templates {
        work.charge(1)?;
        nr = nr
            .checked_add(template.capture_len())
            .ok_or_else(|| budget("stream rectangle count overflow"))?;
        if nr > MAX_R {
            return Err(budget(&format!(
                "stream admission I={ni},T={nt},V={nv},R_lower_bound={nr}; B/E/Q unchecked; legacy not run"
            )));
        }
    }
    let mut nq = 0usize;
    for _ in crossings::queries(shapes, routing_layers).take(MAX_QUERIES + 1) {
        work.charge(1)?;
        nq += 1;
    }
    if nq > MAX_QUERIES {
        return Err(budget(&format!(
            "stream admission I={ni},T={nt},R={nr},V={nv},Q_lower_bound={nq}; B/E unchecked; legacy not run"
        )));
    }
    let mut world: Option<Rect> = None;
    for i in 0..ni {
        work.charge(1)?;
        if let Some(r) = geometry.bounds(i)? {
            world = Some(world.map_or(r, |b| Rect {
                lx: b.lx.min(r.lx),
                ly: b.ly.min(r.ly),
                ux: b.ux.max(r.ux),
                uy: b.uy.max(r.uy),
            }));
        }
    }
    let world = world.ok_or_else(|| budget("stream empty shared geometry"))?;
    let range = span(world, bins.size);
    let nb = area(range)?;
    let nx = usize::try_from(i128::from(range[2]) - i128::from(range[0]) + 1)
        .map_err(|_| budget("stream width"))?;
    let ny = usize::try_from(i128::from(range[3]) - i128::from(range[1]) + 1)
        .map_err(|_| budget("stream height"))?;
    if nb > MAX_B {
        return Err(budget(&format!(
            "stream admission I={ni},T={nt},R={nr},V={nv},B={nb},Q={nq}; E unchecked; legacy not run"
        )));
    }
    let mut ne = 0usize;
    for i in 0..ni {
        work.charge(1)?;
        if let Some(r) = geometry.bounds(i)? {
            ne = ne
                .checked_add(area(span(r, bins.size))?)
                .ok_or_else(|| budget("stream CSR count overflow"))?;
            if ne > MAX_E {
                return Err(budget(&format!(
                    "stream admission I={ni},T={nt},R={nr},V={nv},B={nb},E_lower_bound={ne},Q={nq}; legacy not run"
                )));
            }
        }
    }
    let counts = [nb + 1, ne, ni, nt + 1, nr];
    let widths = [4usize, 4, 96, 4, 48];
    let mut starts = [0usize; 5];
    let mut end = HEADER;
    for (k, start) in starts.iter_mut().enumerate() {
        *start = align(end)?;
        end = start
            .checked_add(
                counts[k]
                    .checked_mul(widths[k])
                    .ok_or_else(|| budget("stream section size"))?,
            )
            .ok_or_else(|| budget("stream section end"))?;
    }
    let payload_bytes = nq
        .checked_mul(40)
        .ok_or_else(|| budget("stream query bytes"))?;
    let tape_bytes = payload_bytes
        .checked_add(2 * HEADER)
        .filter(|&n| n <= TAPE_BYTES)
        .ok_or_else(|| budget("stream tape cap"))?;
    if end > STATIC_BYTES {
        return Err(budget("stream static cap"));
    }
    let query_bytes = nq
        .min(TILE_QUERIES)
        .checked_mul(40)
        .ok_or_else(|| budget("stream tile bytes"))?;
    let live = (end as u64)
        .checked_add((nb as u64) * 4)
        .and_then(|v| v.checked_add(query_bytes as u64))
        .and_then(|v| v.checked_add(METADATA_BYTES))
        .ok_or_else(|| budget("stream staging arithmetic"))?;
    let phase = phase_memory(memory, limits, phase_inputs, live, end, tape_bytes).map_err(|_|budget(&format!("stream phase admission I={ni},T={nt},R={nr},V={nv},B={nb},E={ne},Q={nq}; original_limit={}; legacy not run",memory.limit_bytes)))?;
    let reserve = [
        end.checked_mul(2),
        Some(nb * 2),
        Some(ni * 2),
        Some(ne * 2),
        Some(nt),
        Some(nr),
        Some(nq),
        Some(payload_bytes),
        Some(query_bytes),
        Some(nv.div_ceil(64) + 1),
        Some(4096),
    ]
    .into_iter()
    .try_fold(work.used, |sum, term| sum.checked_add(term?))
    .ok_or_else(|| budget("stream control reservation"))?;
    let legacy_cap = limits
        .max_work
        .checked_sub(reserve)
        .filter(|&v| before <= v)
        .ok_or_else(|| budget("stream aggregate work admission"))?;
    work.cap = reserve;
    let mut packet = Vec::new();
    packet
        .try_reserve_exact(end)
        .map_err(|_| budget("capture packet allocation"))?;
    work.charge(end)?;
    packet.resize(end, 0);
    let mut cursor = Vec::new();
    cursor
        .try_reserve_exact(nb)
        .map_err(|_| budget("capture CSR allocation"))?;
    work.charge(nb)?;
    cursor.resize(nb, 0u32);
    let capacity = (packet.capacity() as u64)
        .saturating_add((cursor.capacity() as u64) * 4)
        .saturating_add(query_bytes as u64)
        .saturating_add(METADATA_BYTES);
    if capacity > MAX_LIVE {
        return Err(budget("capture actual staging capacities"));
    }
    let cell = |x: i64, y: i64| -> Result<usize> {
        let x = i128::from(x) - i128::from(range[0]);
        let y = i128::from(y) - i128::from(range[1]);
        if x < 0 || y < 0 || x >= nx as i128 || y >= ny as i128 {
            return Err(budget("capture CSR cell"));
        }
        usize::try_from(y * nx as i128 + x).map_err(|_| budget("capture CSR cell"))
    };
    for i in 0..ni {
        work.charge(1)?;
        if let Some(r) = geometry.bounds(i)? {
            let b = span(r, bins.size);
            for x in b[0]..=b[2] {
                for y in b[1]..=b[3] {
                    work.charge(1)?;
                    let k = cell(x, y)?;
                    cursor[k] = cursor[k]
                        .checked_add(1)
                        .ok_or_else(|| budget("capture CSR count"))?;
                }
            }
        }
    }
    let mut total = 0usize;
    for (k, c) in cursor.iter_mut().enumerate() {
        work.charge(1)?;
        let count = *c as usize;
        *c = u32::try_from(total).map_err(|_| budget("capture CSR offset"))?;
        u32put(&mut packet, starts[0] + k * 4, total, &mut work)?;
        total += count;
    }
    if total != ne {
        return Err(budget("capture CSR total"));
    }
    u32put(&mut packet, starts[0] + nb * 4, total, &mut work)?;
    let zero = Rect {
        lx: 0,
        ly: 0,
        ux: 0,
        uy: 0,
    };
    for (i, instance) in instances.iter().enumerate() {
        work.charge(1)?;
        let at = starts[2] + i * 96;
        let template = templates
            .get(instance.template as usize)
            .ok_or_else(|| budget("capture template id"))?;
        let bound = geometry.bounds(i)?.unwrap_or(zero);
        if bound.valid() {
            let b = span(bound, bins.size);
            for x in b[0]..=b[2] {
                for y in b[1]..=b[3] {
                    work.charge(1)?;
                    let k = cell(x, y)?;
                    let position = cursor[k] as usize;
                    if position >= ne {
                        return Err(budget("capture CSR write extent"));
                    }
                    u32put(&mut packet, starts[1] + position * 4, i, &mut work)?;
                    cursor[k] += 1;
                }
            }
        }
        put(&mut packet, at, &instance.at[0].to_le_bytes(), &mut work)?;
        put(
            &mut packet,
            at + 8,
            &instance.at[1].to_le_bytes(),
            &mut work,
        )?;
        put(
            &mut packet,
            at + 16,
            &template.capture_height().to_le_bytes(),
            &mut work,
        )?;
        u32put(&mut packet, at + 24, instance.template as usize, &mut work)?;
        u32put(
            &mut packet,
            at + 28,
            shapes.capture_prefix()[i] as usize,
            &mut work,
        )?;
        boxput(&mut packet, at + 32, bound, &mut work)?;
        u32put(
            &mut packet,
            at + 64,
            match instance.orientation {
                Orientation::N => 0,
                Orientation::FS => 1,
            },
            &mut work,
        )?;
    }
    let mut ordinal_base = 0usize;
    for (t, template) in templates.iter().enumerate() {
        work.charge(1)?;
        u32put(&mut packet, starts[3] + t * 4, ordinal_base, &mut work)?;
        for ordinal in 0..template.capture_len() {
            work.charge(1)?;
            let r = template
                .capture_shape(ordinal)
                .ok_or_else(|| budget("capture template ordinal"))?;
            let at = starts[4] + ordinal_base * 48;
            boxput(&mut packet, at, r.rect, &mut work)?;
            u32put(&mut packet, at + 32, r.layer as usize, &mut work)?;
            u32put(&mut packet, at + 36, ordinal, &mut work)?;
            ordinal_base += 1;
        }
    }
    u32put(&mut packet, starts[3] + nt * 4, ordinal_base, &mut work)?;
    put(&mut packet, 0, b"RG1STA02", &mut work)?;
    u32put(&mut packet, 8, 2, &mut work)?;
    u32put(&mut packet, 12, HEADER, &mut work)?;
    put(&mut packet, 16, &(end as u64).to_le_bytes(), &mut work)?;
    u32put(&mut packet, 24, input.layers.len(), &mut work)?;
    u32put(&mut packet, 28, nv, &mut work)?;
    put(&mut packet, 32, &range[0].to_le_bytes(), &mut work)?;
    put(&mut packet, 40, &range[1].to_le_bytes(), &mut work)?;
    put(&mut packet, 48, &bins.size.to_le_bytes(), &mut work)?;
    u32put(&mut packet, 56, nx, &mut work)?;
    u32put(&mut packet, 60, ny, &mut work)?;
    for (k, (&start, &count)) in starts.iter().zip(&counts).enumerate() {
        put(
            &mut packet,
            64 + k * 16,
            &(start as u64).to_le_bytes(),
            &mut work,
        )?;
        put(
            &mut packet,
            72 + k * 16,
            &(count as u64).to_le_bytes(),
            &mut work,
        )?;
    }
    let geometry_counts = GeometryCounts {
        instances: ni,
        templates: nt,
        rectangles: nr,
        virtual_shapes: nv,
        bins: nb,
        entries: ne,
        queries: nq,
        virtual_front: shapes.virtual_offset(),
        prefix_count: shapes.capture_prefix().len(),
        grid_origin_bins: [range[0], range[1]],
        grid_width: nx,
        grid_height: ny,
        coarse_bin_dbu: bins.size,
        layer_count: input.layers.len(),
        routing_layers: routing_layers.clone(),
        static_bytes: end,
        query_payload_bytes: payload_bytes,
        tape_bytes,
    };
    let mut query = Vec::new();
    query
        .try_reserve_exact(query_bytes)
        .map_err(|_| budget("stream tile allocation"))?;
    let capacity = capacity
        .checked_add(query.capacity() as u64)
        .and_then(|n| n.checked_sub(query_bytes as u64))
        .filter(|&n| n <= MAX_LIVE)
        .ok_or_else(|| budget("stream actual tile capacity"))?;
    let actual_phase = phase_memory(memory, limits, phase_inputs, capacity, end, tape_bytes)?;
    sink.static_view(&packet, &geometry_counts)?;
    drop(cursor);
    drop(packet);
    work.charge(query_bytes)?;
    query.resize(query_bytes, 0u8);
    let mut first = 0usize;
    let mut count = 0usize;
    let mut seen = 0usize;
    for (i, q) in crossings::queries(shapes, routing_layers).enumerate() {
        work.charge(1)?;
        if i >= nq {
            return Err(budget("stream query inventory changed"));
        }
        boxput(&mut query, count * 40, q.rect, &mut work)?;
        u32put(&mut query, count * 40 + 32, q.layer as usize, &mut work)?;
        count += 1;
        seen += 1;
        if count == TILE_QUERIES {
            sink.query_chunk(first, &query[..count * 40])?;
            first += count;
            count = 0;
        }
    }
    if count > 0 {
        sink.query_chunk(first, &query[..count * 40])?;
    }
    if seen != nq {
        return Err(budget("stream query count changed"));
    }
    sink.queries_complete(seen)?;
    drop(query);
    if let Some(index) = &mut bins.instances {
        index.restrict_work(legacy_cap);
    }
    let effective_limits = ExtractLimits {
        max_work: legacy_cap,
        ..limits.clone()
    };
    let (bits, parallel) = crossings::run(
        shapes,
        bins,
        routing_layers,
        &effective_limits,
        &memory,
        stats,
    )?;
    if parallel.is_some() || bits.len() != nv.div_ceil(64) {
        return Err(budget("capture actual serial result"));
    }
    work.charge(bits.len() + 1)?;
    let active = bits.iter().map(|w| w.count_ones() as usize).sum::<usize>();
    if stats.passive_strips.checked_sub(strips_before) != active.checked_mul(2)
        || (!nv.is_multiple_of(64) && bits.last().is_some_and(|&v| v >> (nv % 64) != 0))
        || stats
            .work
            .checked_add(work.used)
            .is_none_or(|v| v > limits.max_work)
    {
        return Err(budget("capture original bitset/work invariant"));
    }
    if stats.work > legacy_cap {
        return Err(budget("stream effective legacy cap"));
    }
    sink.legacy_words(&bits)?;
    let _ = phase; // pre-allocation proof; the actual-capacity proof is retained.
    Ok(StreamCounts {
        geometry: geometry_counts,
        legacy_work_before: before,
        legacy_work_after: stats.work,
        passive_strips_before: strips_before,
        passive_strips_after: stats.passive_strips,
        original_bitset_words: bits.len(),
        active_bits: active,
        control_reserved: reserve,
        control_completed: work.used,
        bit_validation_work: nv.div_ceil(64) + 1,
        core_operation_cap: limits.max_work,
        effective_legacy_cap: legacy_cap,
        staging_capacity_bytes: capacity,
        prefix_memory,
        memory: actual_phase,
        serial_body_sha256: body_pin,
        serial_function_sha256: function_pin,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (super::super::ExtractMemory, ExtractLimits, PhaseInputs) {
        let limits = ExtractLimits {
            max_couplings: 10,
            ..Default::default()
        };
        let memory = super::super::ExtractMemory {
            held_bytes: 1000,
            working_bytes: 10_000,
            geometry_bytes: 2000,
            estimate_bytes: 13_000,
            limit_bytes: 512 << 20,
            instance_query_bin_dbu: 16,
        };
        (
            memory,
            limits,
            PhaseInputs {
                source_bytes_per_copy: 8192,
                producer_elf_bytes: 4096,
                binding_file_bytes: 4096,
            },
        )
    }

    #[test]
    fn boundary_keeps_every_parent_except_the_proved_late_reserve() {
        let (memory, limits, context) = fixture();
        let got = phase_memory(memory, &limits, context, 1 << 20, 4097, 8193).unwrap();
        assert_eq!(got.proved_late_coupling_reserve_bytes, 1280);
        assert_eq!(got.retained_parent_bound_bytes, 11_720);
        assert_eq!(got.original_held_bytes, memory.held_bytes);
        assert_eq!(got.original_geometry_bytes, memory.geometry_bytes);
        assert_eq!(got.output_cache_bytes, 8192 + 12_288 + (1 << 20));
        assert_eq!(got.source_cache_bytes, 16_384);
        let expected =
            11_720 + (1 << 20) + 16_384 + 4096 + 4096 + got.output_cache_bytes + (128 << 20);
        assert_eq!(got.total_boundary_bound_bytes, expected);
    }

    #[test]
    fn phase_cap_is_inclusive_and_one_byte_below_rejects() {
        let (mut memory, limits, context) = fixture();
        let bound = phase_memory(memory, &limits, context, 4096, 4096, 8192)
            .unwrap()
            .total_boundary_bound_bytes;
        memory.limit_bytes = bound;
        assert!(phase_memory(memory, &limits, context, 4096, 4096, 8192).is_ok());
        memory.limit_bytes -= 1;
        assert!(phase_memory(memory, &limits, context, 4096, 4096, 8192).is_err());
    }

    #[test]
    fn late_credit_cannot_exceed_the_original_working_reserve() {
        let (memory, mut limits, context) = fixture();
        limits.max_couplings = 79;
        assert!(phase_memory(memory, &limits, context, 4096, 4096, 8192).is_err());
        limits.max_couplings = usize::MAX;
        assert!(phase_memory(memory, &limits, context, 4096, 4096, 8192).is_err());
    }

    #[test]
    fn live_and_each_cache_have_independent_hard_admission() {
        let (memory, limits, context) = fixture();
        assert!(phase_memory(memory, &limits, context, MAX_LIVE + 1, 4096, 8192).is_err());
        for oversized in [
            PhaseInputs {
                source_bytes_per_copy: (128 << 20) + 1,
                ..context
            },
            PhaseInputs {
                producer_elf_bytes: (128 << 20) + 1,
                ..context
            },
            PhaseInputs {
                binding_file_bytes: (8 << 20) + 1,
                ..context
            },
        ] {
            assert!(phase_memory(memory, &limits, oversized, 4096, 4096, 8192).is_err());
        }
    }
}
