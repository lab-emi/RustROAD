//! Opt-in boundary capture. No numerical extraction or component execution.
use super::{ExtractInput, ExtractLimits, PreparedCrossings, budget, crossings, prepare_crossings};
use crate::{Orientation, Rect, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

const HEADER: usize = 4096;
const MAX_LIVE: u64 = 64 << 20;
const MAX_Q: usize = 65_536;
const MAX_I: usize = 100_000;
const MAX_T: usize = 1024;
const MAX_R: usize = 16_384;
const MAX_B: usize = 1_000_000;
const MAX_E: usize = 4_000_000;
const MAX_V: usize = 1_000_000;

/// Metadata only; packet and original bitset are written separately.
#[derive(Debug, Serialize)]
pub struct CaptureCounts {
    pub instances: usize,
    pub templates: usize,
    pub template_rectangles: usize,
    pub virtual_shapes: usize,
    pub virtual_front: usize,
    pub prefix_count: usize,
    pub queries: usize,
    pub grid_origin_bins: [i64; 2],
    pub grid_width: usize,
    pub grid_height: usize,
    pub coarse_bin_dbu: i64,
    pub grid_cells: usize,
    pub csr_entries: usize,
    pub layer_count: usize,
    pub routing_layers: Vec<u32>,
    pub packet_bytes: usize,
    pub original_bitset_words: usize,
    pub active_bits: usize,
    pub legacy_work_before: usize,
    pub legacy_work_after: usize,
    pub passive_strips_before: usize,
    pub passive_strips_after: usize,
    pub control_reserved: usize,
    pub control_completed: usize,
    pub bit_validation_work: usize,
    pub core_operation_cap: usize,
    pub effective_legacy_cap: usize,
    pub staging_capacity_bytes: u64,
    pub conservative_engine_plus_staging_bytes: u64,
    pub memory_limit_bytes: u64,
    pub serial_body_sha256: String,
    pub serial_function_sha256: String,
}

/// The words come directly from the one real legacy call, never reconstruction.
#[derive(Debug)]
pub struct CaptureData {
    pub packet: Vec<u8>,
    pub original_bits: Vec<u64>,
    pub counts: CaptureCounts,
}

pub(super) struct Work {
    pub(super) used: usize,
    pub(super) cap: usize,
}
impl Work {
    pub(super) fn charge(&mut self, n: usize) -> Result<()> {
        self.used = self
            .used
            .checked_add(n)
            .filter(|&v| v <= self.cap)
            .ok_or_else(|| budget("capture control work"))?;
        Ok(())
    }
}

pub(super) fn align(n: usize) -> Result<usize> {
    n.checked_add(63)
        .map(|v| v & !63)
        .ok_or_else(|| budget("capture alignment"))
}
pub(super) fn span(r: Rect, bin: i64) -> [i64; 4] {
    [
        r.lx.div_euclid(bin),
        r.ly.div_euclid(bin),
        r.ux.div_euclid(bin),
        r.uy.div_euclid(bin),
    ]
}
pub(super) fn area(r: [i64; 4]) -> Result<usize> {
    usize::try_from(
        (i128::from(r[2]) - i128::from(r[0]) + 1)
            .checked_mul(i128::from(r[3]) - i128::from(r[1]) + 1)
            .ok_or_else(|| budget("capture span"))?,
    )
    .map_err(|_| budget("capture span"))
}
pub(super) fn put(data: &mut [u8], at: usize, value: &[u8], work: &mut Work) -> Result<()> {
    work.charge(value.len())?;
    let end = at
        .checked_add(value.len())
        .ok_or_else(|| budget("capture field"))?;
    data.get_mut(at..end)
        .ok_or_else(|| budget("capture field"))?
        .copy_from_slice(value);
    Ok(())
}
pub(super) fn u32put(data: &mut [u8], at: usize, value: usize, work: &mut Work) -> Result<()> {
    let v = u32::try_from(value).map_err(|_| budget("capture u32"))?;
    put(data, at, &v.to_le_bytes(), work)
}
pub(super) fn boxput(data: &mut [u8], at: usize, r: Rect, work: &mut Work) -> Result<()> {
    for (k, value) in [r.lx, r.ly, r.ux, r.uy].into_iter().enumerate() {
        put(data, at + k * 8, &value.to_le_bytes(), work)?;
    }
    Ok(())
}

/// Hash the exact source fragments embedded into this experimental binary.
pub fn serial_source_pins() -> Result<(String, String)> {
    let text = include_str!("../extract.rs");
    let start = text
        .find("fn virtual_crossings(")
        .ok_or_else(|| budget("capture source start"))?;
    let rest = &text[start..];
    let end = rest
        .find("\nfn virtual_strip(")
        .ok_or_else(|| budget("capture source end"))?;
    let function = &rest[..end];
    let body = function[function
        .find('{')
        .ok_or_else(|| budget("capture source body"))?..]
        .trim_end();
    let hash = |bytes: &[u8]| format!("{:x}", Sha256::digest(bytes));
    let a = hash(body.as_bytes());
    let b = hash(function.as_bytes());
    if a != "194d67d08dfc489884d9a55c27e414b1dcbcec06513eefb392515f931dd4f1cc"
        || b != "55f627475f3b550b1742577aee707b6bdf7d5dcd031dcf5e67a9b60f644e9906"
    {
        return Err(budget("capture original serial source changed"));
    }
    Ok((a, b))
}

/// Capture only one bounded real serial boundary, within the original limits.
///
/// Caller must establish an existing one-thread pool. Unsupported real query
/// volumes fail; this entry does not implement a streaming worker.
pub fn capture_cpu_ref(input: &ExtractInput<'_>, limits: &ExtractLimits) -> Result<CaptureData> {
    if limits.query_workers != 1
        || limits.crossing_workers != 1
        || limits.max_work > 100_000_000_000
        || limits.memory_mib > 3584
        || rayon::current_thread_index().is_none()
        || rayon::current_num_threads() != 1
    {
        return Err(budget("capture requires original bounded one-worker path"));
    }
    let mut prepared = prepare_crossings(input, limits)?;
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
        .ok_or_else(|| budget("capture requires shared instance geometry"))?;
    let geometry = source.geometry;
    let instances = geometry.capture_instances();
    let templates = geometry.capture_templates();
    let ni = instances.len();
    let nt = templates.len();
    let nv = shapes.virtual_count();
    if ni > MAX_I || nt > MAX_T || nv > MAX_V || input.layers.len() > 32 || input.layers.is_empty()
    {
        return Err(budget("capture instance/template/virtual/layer count"));
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
            .ok_or_else(|| budget("capture source work"))?,
    )?;
    let (body_pin, function_pin) = serial_source_pins()?;
    let mut nr = 0usize;
    for t in templates {
        work.charge(1)?;
        nr = nr
            .checked_add(t.capture_len())
            .filter(|&v| v <= MAX_R)
            .ok_or_else(|| budget("capture rectangle count"))?;
    }
    let mut nq = 0usize;
    for _ in crossings::queries(shapes, routing_layers).take(MAX_Q + 1) {
        work.charge(1)?;
        nq += 1;
    }
    if nq > MAX_Q {
        return Err(budget("capture query count; full stream unsupported"));
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
    let world = world.ok_or_else(|| budget("capture empty shared geometry"))?;
    let range = span(world, bins.size);
    let nb = area(range)?;
    let nx = usize::try_from(i128::from(range[2]) - i128::from(range[0]) + 1)
        .map_err(|_| budget("capture grid width"))?;
    let ny = usize::try_from(i128::from(range[3]) - i128::from(range[1]) + 1)
        .map_err(|_| budget("capture grid height"))?;
    if nb > MAX_B {
        return Err(budget("capture dense grid unsupported"));
    }
    let mut ne = 0usize;
    for i in 0..ni {
        work.charge(1)?;
        if let Some(r) = geometry.bounds(i)? {
            ne = ne
                .checked_add(area(span(r, bins.size))?)
                .filter(|&v| v <= MAX_E)
                .ok_or_else(|| budget("capture CSR entry count"))?;
        }
    }
    let counts = [nb + 1, ne, ni, nt + 1, nr, nq];
    let widths = [4usize, 4, 96, 4, 48, 40];
    let mut starts = [0usize; 6];
    let mut end = HEADER;
    for (k, start) in starts.iter_mut().enumerate() {
        *start = align(end)?;
        end = (*start)
            .checked_add(
                counts[k]
                    .checked_mul(widths[k])
                    .ok_or_else(|| budget("capture section size"))?,
            )
            .ok_or_else(|| budget("capture section end"))?;
    }
    let expected_staging = (end as u64)
        .checked_add((nb as u64) * 4)
        .and_then(|v| v.checked_add(1 << 20))
        .ok_or_else(|| budget("capture staging estimate"))?;
    let estimated = memory
        .estimate_bytes
        .checked_add(expected_staging)
        .ok_or_else(|| budget("capture memory estimate"))?;
    if expected_staging > MAX_LIVE || estimated > memory.limit_bytes {
        return Err(budget("capture original engine plus staging memory"));
    }
    let reserve = end
        .checked_mul(3)
        .and_then(|v| v.checked_add(ne * 4))
        .and_then(|v| v.checked_add(ni * 10 + nr * 10 + nq * 4 + nb * 4 + 4096))
        .and_then(|v| v.checked_add(nv.div_ceil(64) + 1))
        .and_then(|v| v.checked_add(work.used))
        .ok_or_else(|| budget("capture control reservation"))?;
    let legacy_cap = limits
        .max_work
        .checked_sub(reserve)
        .filter(|&v| before <= v)
        .ok_or_else(|| budget("capture combined original work admission"))?;
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
        .saturating_add(1 << 20);
    if capacity > MAX_LIVE || memory.estimate_bytes.saturating_add(capacity) > memory.limit_bytes {
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
    for (i, q) in crossings::queries(shapes, routing_layers).enumerate() {
        work.charge(1)?;
        if i >= nq {
            return Err(budget("capture query inventory changed"));
        }
        boxput(&mut packet, starts[5] + i * 40, q.rect, &mut work)?;
        u32put(
            &mut packet,
            starts[5] + i * 40 + 32,
            q.layer as usize,
            &mut work,
        )?;
    }
    put(&mut packet, 0, b"RG1CSR01", &mut work)?;
    u32put(&mut packet, 8, 1, &mut work)?;
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
    drop(cursor);
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
    Ok(CaptureData {
        packet,
        original_bits: bits,
        counts: CaptureCounts {
            instances: ni,
            templates: nt,
            template_rectangles: nr,
            virtual_shapes: nv,
            virtual_front: shapes.virtual_offset(),
            prefix_count: shapes.capture_prefix().len(),
            queries: nq,
            grid_origin_bins: [range[0], range[1]],
            grid_width: nx,
            grid_height: ny,
            coarse_bin_dbu: bins.size,
            grid_cells: nb,
            csr_entries: ne,
            layer_count: input.layers.len(),
            routing_layers: routing_layers.clone(),
            packet_bytes: end,
            original_bitset_words: nv.div_ceil(64),
            active_bits: active,
            legacy_work_before: before,
            legacy_work_after: stats.work,
            passive_strips_before: strips_before,
            passive_strips_after: stats.passive_strips,
            control_reserved: reserve,
            control_completed: work.used,
            bit_validation_work: nv.div_ceil(64) + 1,
            core_operation_cap: limits.max_work,
            effective_legacy_cap: legacy_cap,
            staging_capacity_bytes: capacity,
            conservative_engine_plus_staging_bytes: memory.estimate_bytes + capacity,
            memory_limit_bytes: memory.limit_bytes,
            serial_body_sha256: body_pin,
            serial_function_sha256: function_pin,
        },
    })
}
