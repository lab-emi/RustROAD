//! Maximal collinear wire runs for export (DEF NETS paths, GDS rectangles).
//!
//! The router stores one record per lattice edge. Every record is a DEF path
//! with the layer default width and a half-width extension at both ends, so
//! collinear same-net same-layer records that touch or overlap describe one
//! continuous rectangle. This module merges them into one run per maximal
//! interval of a track line and splits a run only at an electrical junction
//! that falls strictly inside it:
//!
//! * a via landing on that layer (lower or upper metal of the via kind),
//! * a pin-access point on that layer,
//! * a point where a non-collinear (perpendicular) same-net wire on the same
//!   layer touches or crosses the run.
//!
//! The union of the returned runs' rectangles equals the union of the input
//! rectangles exactly (each run is a union of input intervals, and consecutive
//! pieces of a split run share their split point). Every via, access point and
//! branch therefore sits at a path end, which keeps endpoint-based reference
//! readers electrically correct. Geometry is integer DBU throughout.
//!
//! Work is bounded: grouping is O(n log n) in the wire count and the
//! perpendicular-touch test per (net, layer) is charged against `max_work`.
//! Line and junction vectors are retained for one net at a time. Nets are
//! independent, so `collinear_runs` merges them on the rayon pool and, when
//! any net fails or the summed work exceeds the budget, repeats the merge in
//! net order to report the same error as the sequential pass.
use crate::routing::{Access, Via, Wire};
use crate::{Error, Result};
use rayon::prelude::*;
use std::borrow::Cow;

fn invalid(s: &str) -> Error {
    Error::Invalid(format!("WIRE_RUNS: {s}"))
}

/// Merge `wires` into maximal runs split at junctions (see module docs).
/// `via_layers[kind] = (lower, upper)` routing-layer indices of each via kind.
/// Output is sorted by (net, layer, horizontal first, cross, lo); every run
/// starts at its lower coordinate.
pub fn collinear_runs(
    wires: &[&Wire],
    vias: &[&Via],
    via_layers: &[(u32, u32)],
    access: &[&Access],
    max_work: usize,
) -> Result<Vec<Wire>> {
    // Most callers already emit net order. Borrow those slices; otherwise
    // sort only references, retaining geometry in its original storage.
    let wires = net_order(wires, |w| w.net);
    let vias = net_order(vias, |v| v.net);
    let access = net_order(access, |a| a.net);
    // Preserve validation even for a via whose net has no wire records.
    for v in vias.iter() {
        if v.kind as usize >= via_layers.len() {
            return Err(invalid("via kind outside the via table"));
        }
    }
    // Per-net slices: (wires, vias, access) ranges in net order.
    let (mut wi, mut vi, mut ai) = (0, 0, 0);
    let mut ranges = Vec::new();
    while wi < wires.len() {
        let net = wires[wi].net;
        let end = wi + wires[wi..].partition_point(|w| w.net == net);
        while vi < vias.len() && vias[vi].net < net {
            vi += 1;
        }
        let vend = vi + vias[vi..].partition_point(|v| v.net == net);
        while ai < access.len() && access[ai].net < net {
            ai += 1;
        }
        let aend = ai + access[ai..].partition_point(|a| a.net == net);
        ranges.push([wi, end, vi, vend, ai, aend]);
        (wi, vi, ai) = (end, vend, aend);
    }
    let run = |r: &[usize; 6], work: &mut usize| {
        net_runs(
            &wires[r[0]..r[1]],
            &vias[r[2]..r[3]],
            via_layers,
            &access[r[4]..r[5]],
            work,
            max_work,
            max_work,
        )
    };
    if rayon::current_num_threads() > 1 && ranges.len() > 1 {
        // Each net from zero work; the sequential pass charges one shared
        // counter, so it succeeds exactly when every net does and the sum
        // stays within the budget (every check is `work <= max_work`).
        let parts: Vec<Result<(Vec<Wire>, usize)>> = ranges
            .par_iter()
            .map(|r| {
                let mut work = 0;
                run(r, &mut work).map(|out| (out, work))
            })
            .collect();
        let mut total = 0usize;
        let within = parts.iter().all(|p| {
            p.as_ref().is_ok_and(|(_, w)| {
                total = total.saturating_add(*w);
                total <= max_work
            })
        });
        if within {
            let mut out = Vec::with_capacity(
                parts
                    .iter()
                    .map(|p| p.as_ref().map_or(0, |(o, _)| o.len()))
                    .sum(),
            );
            for (part, _) in parts.into_iter().flatten() {
                out.extend(part);
            }
            return Ok(out);
        }
    }
    let mut work = 0;
    let mut out = Vec::new();
    for r in &ranges {
        out.extend(run(r, &mut work)?);
    }
    Ok(out)
}

fn net_order<'a, T>(values: &'a [&'a T], net: impl Fn(&T) -> u32) -> Cow<'a, [&'a T]> {
    if values.windows(2).all(|pair| net(pair[0]) <= net(pair[1])) {
        Cow::Borrowed(values)
    } else {
        let mut sorted = values.to_vec();
        sorted.sort_unstable_by_key(|value| net(value));
        Cow::Owned(sorted)
    }
}

/// One merged interval of a track line: `(net, layer, vertical, cross, lo, hi)`.
/// Sorted, the order is net, layer, horizontal (`false`) lines first, cross
/// coordinate, then interval start.
type Segment = (u32, u32, bool, i64, i64, i64);
/// A junction point of one net and layer, `(net, layer, axis, other)`.
type Junction = (u32, u32, i64, i64);

/// Only this net's segment and junction vectors remain live beside the output.
pub(crate) fn net_runs(
    wires: &[&Wire],
    vias: &[&Via],
    via_layers: &[(u32, u32)],
    access: &[&Access],
    work: &mut usize,
    max_work: usize,
    max_output: usize,
) -> Result<Vec<Wire>> {
    let mut segments: Vec<Segment> = Vec::with_capacity(wires.len());
    for w in wires {
        let (vertical, cross, a, b) = if w.start[0] == w.end[0] && w.start[1] != w.end[1] {
            (true, w.start[0], w.start[1], w.end[1])
        } else if w.start[1] == w.end[1] && w.start[0] != w.end[0] {
            (false, w.start[1], w.start[0], w.end[0])
        } else {
            return Err(invalid("non-Manhattan or zero-length wire"));
        };
        segments.push((w.net, w.layer, vertical, cross, a.min(b), a.max(b)));
    }
    // Union of closed intervals per line; touching intervals merge.
    segments.sort_unstable();
    let mut merged: Vec<Segment> = Vec::with_capacity(segments.len());
    for s in segments {
        match merged.last_mut() {
            Some(m) if (m.0, m.1, m.2, m.3) == (s.0, s.1, s.2, s.3) && s.4 <= m.5 => {
                m.5 = m.5.max(s.5)
            }
            _ => merged.push(s),
        }
    }
    // Junction points per (net, layer), keyed (x, y) for vertical lines.
    let mut by_x: Vec<Junction> = Vec::with_capacity(2 * vias.len() + access.len());
    for v in vias {
        let &(lower, upper) = via_layers
            .get(v.kind as usize)
            .ok_or_else(|| invalid("via kind outside the via table"))?;
        for layer in [lower, upper] {
            by_x.push((v.net, layer, v.at[0], v.at[1]));
        }
    }
    for a in access {
        by_x.push((a.net, a.layer, a.at[0], a.at[1]));
    }
    // Perpendicular touches: a horizontal and a vertical interval of the same
    // net and layer that intersect (closed) make a junction at the crossing.
    let mut i = 0;
    while i < merged.len() {
        let (net, layer) = (merged[i].0, merged[i].1);
        let end = i + merged[i..].partition_point(|m| (m.0, m.1) == (net, layer));
        let split = i + merged[i..end].partition_point(|m| !m.2);
        let (horizontal, vertical) = (&merged[i..split], &merged[split..end]);
        *work = work
            .checked_add(horizontal.len().saturating_mul(vertical.len()))
            .ok_or_else(|| Error::Budget("wire-run junction work".into()))?;
        if *work > max_work {
            return Err(Error::Budget("wire-run junction work".into()));
        }
        for &(_, _, _, y, x0, x1) in horizontal {
            for &(_, _, _, x, y0, y1) in vertical {
                if x0 <= x && x <= x1 && y0 <= y && y <= y1 {
                    by_x.push((net, layer, x, y));
                }
            }
        }
        i = end;
    }
    by_x.sort_unstable();
    by_x.dedup();
    // Both axis orders, so each line takes a range query: (x, y) for
    // vertical lines and (y, x) for horizontal ones.
    let mut by_y: Vec<Junction> = by_x.iter().map(|&(n, l, x, y)| (n, l, y, x)).collect();
    by_y.sort_unstable();
    let mut out = Vec::new();
    for &(net, layer, vertical, cross, lo, hi) in &merged {
        let mut from = lo;
        let mut emit = |to| -> Result<()> {
            if out.len() >= max_output {
                return Err(Error::Budget("wire-run output record limit".into()));
            }
            *work = work
                .checked_add(1)
                .filter(|&n| n <= max_work)
                .ok_or_else(|| Error::Budget("wire-run junction work".into()))?;
            let (start, end) = if vertical {
                ([cross, from], [cross, to])
            } else {
                ([from, cross], [to, cross])
            };
            out.push(Wire {
                net,
                layer,
                start,
                end,
            });
            from = to;
            Ok(())
        };
        // Iterate interior junctions directly; charge before each output.
        if hi - lo >= 2 {
            let points = if vertical { &by_x } else { &by_y };
            let first = points.partition_point(|&p| p < (net, layer, cross, lo + 1));
            let last = points.partition_point(|&p| p <= (net, layer, cross, hi - 1));
            for point in &points[first..last] {
                emit(point.3)?;
            }
        }
        emit(hi)?;
    }
    Ok(out)
}

#[cfg(test)]
use std::collections::{BTreeMap, BTreeSet};
/// `(net, layer, vertical, cross)` identifies one track line of one net.
#[cfg(test)]
type Line = (u32, u32, bool, i64);
/// Union of closed intervals; touching intervals merge.
#[cfg(test)]
fn union(mut v: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    v.sort_unstable();
    let mut out: Vec<(i64, i64)> = Vec::with_capacity(v.len());
    for (lo, hi) in v {
        match out.last_mut() {
            Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
            _ => out.push((lo, hi)),
        }
    }
    out
}
/// The original map/set implementation, kept as the test oracle.
#[cfg(test)]
pub(crate) fn net_runs_reference(
    wires: &[&Wire],
    vias: &[&Via],
    via_layers: &[(u32, u32)],
    access: &[&Access],
    work: &mut usize,
    max_work: usize,
    max_output: usize,
) -> Result<Vec<Wire>> {
    let mut lines = BTreeMap::<Line, Vec<(i64, i64)>>::new();
    for w in wires {
        let (vertical, cross, a, b) = if w.start[0] == w.end[0] && w.start[1] != w.end[1] {
            (true, w.start[0], w.start[1], w.end[1])
        } else if w.start[1] == w.end[1] && w.start[0] != w.end[0] {
            (false, w.start[1], w.start[0], w.end[0])
        } else {
            return Err(invalid("non-Manhattan or zero-length wire"));
        };
        lines
            .entry((w.net, w.layer, vertical, cross))
            .or_default()
            .push((a.min(b), a.max(b)));
    }
    let merged: BTreeMap<Line, Vec<(i64, i64)>> =
        lines.into_iter().map(|(k, v)| (k, union(v))).collect();
    // Junction points per (net, layer).
    let mut points = BTreeMap::<(u32, u32), BTreeSet<[i64; 2]>>::new();
    for v in vias {
        let &(lower, upper) = via_layers
            .get(v.kind as usize)
            .ok_or_else(|| invalid("via kind outside the via table"))?;
        for layer in [lower, upper] {
            points.entry((v.net, layer)).or_default().insert(v.at);
        }
    }
    for a in access {
        points.entry((a.net, a.layer)).or_default().insert(a.at);
    }
    // Perpendicular touches: a horizontal and a vertical interval of the same
    // net and layer that intersect (closed) make a junction at the crossing.
    let mut by_layer = BTreeMap::<(u32, u32), (Vec<(i64, i64, i64)>, Vec<(i64, i64, i64)>)>::new();
    for (&(net, layer, vertical, cross), intervals) in &merged {
        let entry = by_layer.entry((net, layer)).or_default();
        for &(lo, hi) in intervals {
            if vertical {
                entry.1.push((cross, lo, hi));
            } else {
                entry.0.push((cross, lo, hi));
            }
        }
    }
    for (key, (horizontal, vertical)) in &by_layer {
        *work = work
            .checked_add(horizontal.len().saturating_mul(vertical.len()))
            .ok_or_else(|| Error::Budget("wire-run junction work".into()))?;
        if *work > max_work {
            return Err(Error::Budget("wire-run junction work".into()));
        }
        for &(y, x0, x1) in horizontal {
            for &(x, y0, y1) in vertical {
                if x0 <= x && x <= x1 && y0 <= y && y <= y1 {
                    points.entry(*key).or_default().insert([x, y]);
                }
            }
        }
    }
    // Both axis orders, so each line takes a range query: (x, y) for
    // vertical lines and (y, x) for horizontal ones.
    let transposed: BTreeMap<(u32, u32), BTreeSet<[i64; 2]>> = points
        .iter()
        .map(|(k, set)| (*k, set.iter().map(|p| [p[1], p[0]]).collect()))
        .collect();
    let empty = BTreeSet::new();
    let mut out = Vec::new();
    // BTreeMap order puts horizontal (`false`) lines first within a layer.
    for (&(net, layer, vertical, cross), intervals) in &merged {
        let junctions = if vertical {
            points.get(&(net, layer))
        } else {
            transposed.get(&(net, layer))
        }
        .unwrap_or(&empty);
        for &(lo, hi) in intervals {
            let mut from = lo;
            let mut emit = |to| -> Result<()> {
                if out.len() >= max_output {
                    return Err(Error::Budget("wire-run output record limit".into()));
                }
                *work = work
                    .checked_add(1)
                    .filter(|&n| n <= max_work)
                    .ok_or_else(|| Error::Budget("wire-run junction work".into()))?;
                let (start, end) = if vertical {
                    ([cross, from], [cross, to])
                } else {
                    ([from, cross], [to, cross])
                };
                out.push(Wire {
                    net,
                    layer,
                    start,
                    end,
                });
                from = to;
                Ok(())
            };
            // Iterate interior junctions directly; charge before each output.
            if hi - lo >= 2 {
                for point in junctions.range([cross, lo + 1]..=[cross, hi - 1]) {
                    emit(point[1])?;
                }
            }
            emit(hi)?;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(net: u32, layer: u32, s: [i64; 2], e: [i64; 2]) -> Wire {
        Wire {
            net,
            layer,
            start: s,
            end: e,
        }
    }
    /// Exact union area of wire rectangles (half width `h`) by coordinate
    /// compression; independent of the merge code.
    fn union_area(wires: &[Wire], layer: u32, net: u32, h: i64) -> i128 {
        let rects: Vec<[i64; 4]> = wires
            .iter()
            .filter(|x| x.layer == layer && x.net == net)
            .map(|x| {
                [
                    x.start[0].min(x.end[0]) - h,
                    x.start[1].min(x.end[1]) - h,
                    x.start[0].max(x.end[0]) + h,
                    x.start[1].max(x.end[1]) + h,
                ]
            })
            .collect();
        let mut xs: Vec<i64> = rects.iter().flat_map(|r| [r[0], r[2]]).collect();
        let mut ys: Vec<i64> = rects.iter().flat_map(|r| [r[1], r[3]]).collect();
        xs.sort_unstable();
        xs.dedup();
        ys.sort_unstable();
        ys.dedup();
        let mut area = 0i128;
        for i in 0..xs.len().saturating_sub(1) {
            for j in 0..ys.len().saturating_sub(1) {
                let (cx, cy) = (xs[i], ys[j]);
                if rects
                    .iter()
                    .any(|r| r[0] <= cx && cx < r[2] && r[1] <= cy && cy < r[3])
                {
                    area += i128::from(xs[i + 1] - xs[i]) * i128::from(ys[j + 1] - ys[j]);
                }
            }
        }
        area
    }
    /// Every covered cell of one union is covered in the other (same set).
    fn same_union(a: &[Wire], b: &[Wire], h: i64) -> bool {
        let mut keys: BTreeSet<(u32, u32)> = a.iter().map(|x| (x.net, x.layer)).collect();
        keys.extend(b.iter().map(|x| (x.net, x.layer)));
        keys.into_iter().all(|(n, l)| {
            let both: Vec<Wire> = a.iter().chain(b).cloned().collect();
            let ua = union_area(a, l, n, h);
            ua == union_area(b, l, n, h) && ua == union_area(&both, l, n, h)
        })
    }

    #[test]
    fn interleaved_nets_keep_sorted_geometry_and_share_the_work_limit() {
        let wires = [
            w(9, 0, [10, 0], [20, 0]),
            w(2, 0, [0, 0], [10, 0]),
            w(9, 0, [0, 0], [10, 0]),
        ];
        let via = Via {
            net: 9,
            kind: 0,
            at: [10, 0],
        };
        let extra = Via {
            net: 4,
            kind: 0,
            at: [10, 0],
        };
        let result = collinear_runs(
            &wires.iter().collect::<Vec<_>>(),
            &[&via, &extra],
            &[(0, 1)],
            &[],
            3,
        )
        .unwrap();
        assert_eq!(
            result,
            [
                w(2, 0, [0, 0], [10, 0]),
                w(9, 0, [0, 0], [10, 0]),
                w(9, 0, [10, 0], [20, 0])
            ]
        );
        assert!(
            collinear_runs(
                &wires.iter().collect::<Vec<_>>(),
                &[&via],
                &[(0, 1)],
                &[],
                2
            )
            .is_err()
        );
        assert!(
            net_runs(
                &wires[0..1].iter().collect::<Vec<_>>(),
                &[],
                &[],
                &[],
                &mut 0,
                100,
                0,
            )
            .is_err()
        );
        let invalid = Via {
            net: 4,
            kind: 5,
            at: [0, 0],
        };
        assert!(collinear_runs(&[], &[&invalid], &[(0, 1)], &[], 100).is_err());
    }

    #[test]
    #[ignore = "standalone scale fixture; run under an OS memory and wall-clock guard"]
    fn many_net_normalization_scale_fixture() {
        let count = 100_001u32;
        let wires: Vec<_> = (0..count)
            .flat_map(|net| {
                [
                    w(net, 0, [0, 0], [5, 0]),
                    w(net, 0, [5, 0], [10, 0]),
                    w(net, 0, [10, 0], [20, 0]),
                ]
            })
            .collect();
        let vias: Vec<_> = (0..count)
            .map(|net| Via {
                net,
                kind: 0,
                at: [10, 0],
            })
            .collect();
        let access: Vec<_> = (0..count)
            .map(|net| Access {
                pin: net,
                net,
                layer: 0,
                at: [0, 0],
            })
            .collect();
        let result = collinear_runs(
            &wires.iter().collect::<Vec<_>>(),
            &vias.iter().collect::<Vec<_>>(),
            &[(0, 1)],
            &access.iter().collect::<Vec<_>>(),
            count as usize * 4,
        )
        .unwrap();
        assert_eq!(result.len(), count as usize * 2);
        for (net, pair) in result.chunks_exact(2).enumerate() {
            assert_eq!(
                pair,
                [
                    w(net as u32, 0, [0, 0], [10, 0]),
                    w(net as u32, 0, [10, 0], [20, 0])
                ]
            );
        }
    }

    /// The vector implementation reproduces the map/set oracle exactly:
    /// output records, their order, the work charged and every error.
    #[test]
    fn sorted_vectors_match_the_map_oracle() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = move |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        for case in 0..400 {
            let nets = 1 + next(3) as u32;
            let mut wires = Vec::new();
            for _ in 0..next(40) {
                let (x, y) = (next(12) as i64 * 10, next(12) as i64 * 10);
                let len = 10 * (1 + next(4) as i64);
                let (net, layer) = (next(nets as u64) as u32, next(3) as u32);
                let end = match next(9) {
                    0..=3 => [x + len, y],
                    4..=7 => [x, y + len],
                    _ => [x + len, y + len], // non-Manhattan: an error case
                };
                if case % 7 != 0 && end[0] != x && end[1] != y {
                    continue;
                }
                let (start, end) = if next(2) == 0 {
                    ([x, y], end)
                } else {
                    (end, [x, y])
                };
                wires.push(w(net, layer, start, end));
            }
            let vias: Vec<Via> = (0..next(8))
                .map(|_| Via {
                    net: next(nets as u64) as u32,
                    kind: next(if case % 11 == 0 { 3 } else { 2 }) as u32,
                    at: [next(12) as i64 * 10, next(12) as i64 * 10],
                })
                .collect();
            let access: Vec<Access> = (0..next(5))
                .map(|i| Access {
                    pin: i as u32,
                    net: next(nets as u64) as u32,
                    layer: next(3) as u32,
                    at: [next(12) as i64 * 10, next(12) as i64 * 10],
                })
                .collect();
            let layers = [(0, 1), (1, 2)];
            let (wr, vr, ar): (Vec<&Wire>, Vec<&Via>, Vec<&Access>) = (
                wires.iter().collect(),
                vias.iter().collect(),
                access.iter().collect(),
            );
            for (max_work, max_output) in [
                (usize::MAX, usize::MAX),
                (next(60) as usize, 1000),
                (1000, next(6) as usize),
            ] {
                let (mut a, mut b) = (next(5) as usize, 0);
                b += a;
                let new = net_runs(&wr, &vr, &layers, &ar, &mut a, max_work, max_output);
                let old = net_runs_reference(&wr, &vr, &layers, &ar, &mut b, max_work, max_output);
                match (&new, &old) {
                    (Ok(x), Ok(y)) => assert_eq!(x, y, "case {case}"),
                    (Err(x), Err(y)) => assert_eq!(x.to_string(), y.to_string(), "case {case}"),
                    _ => panic!("case {case}: {new:?} vs {old:?}"),
                }
                assert_eq!(a, b, "case {case} work");
            }
            // The pooled multi-net merge equals the sequential pass.
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(3)
                .build()
                .unwrap();
            for max_work in [usize::MAX, next(80) as usize] {
                let pooled = pool.install(|| collinear_runs(&wr, &vr, &layers, &ar, max_work));
                let single = rayon::ThreadPoolBuilder::new()
                    .num_threads(1)
                    .build()
                    .unwrap()
                    .install(|| collinear_runs(&wr, &vr, &layers, &ar, max_work));
                match (&pooled, &single) {
                    (Ok(x), Ok(y)) => assert_eq!(x, y, "case {case}"),
                    (Err(x), Err(y)) => assert_eq!(x.to_string(), y.to_string(), "case {case}"),
                    _ => panic!("case {case}: {pooled:?} vs {single:?}"),
                }
            }
        }
    }

    #[test]
    fn chain_of_lattice_edges_becomes_one_run() {
        let edges = [
            w(0, 1, [0, 0], [10, 0]),
            w(0, 1, [20, 0], [10, 0]),
            w(0, 1, [20, 0], [50, 0]),
        ];
        let refs: Vec<&Wire> = edges.iter().collect();
        let runs = collinear_runs(&refs, &[], &[], &[], 1000).unwrap();
        assert_eq!(runs, vec![w(0, 1, [0, 0], [50, 0])]);
        assert!(same_union(&edges, &runs, 5));
    }

    #[test]
    fn runs_split_at_vias_access_points_and_branches_only() {
        let edges = [
            // Horizontal track y=0 from 0 to 100 in 10 edges.
            w(3, 1, [0, 0], [10, 0]),
            w(3, 1, [10, 0], [20, 0]),
            w(3, 1, [20, 0], [30, 0]),
            w(3, 1, [30, 0], [40, 0]),
            w(3, 1, [40, 0], [50, 0]),
            w(3, 1, [50, 0], [60, 0]),
            w(3, 1, [60, 0], [70, 0]),
            w(3, 1, [70, 0], [80, 0]),
            w(3, 1, [80, 0], [90, 0]),
            w(3, 1, [90, 0], [100, 0]),
            // T branch at x=40 going up (same layer).
            w(3, 1, [40, 0], [40, 20]),
            w(3, 1, [40, 20], [40, 30]),
            // Same xy on another layer must not split layer 1.
            w(3, 2, [70, -10], [70, 10]),
            // Other net on the same line: kept separate.
            w(4, 1, [200, 0], [210, 0]),
        ];
        let vias = [Via {
            net: 3,
            kind: 0,
            at: [20, 0],
        }];
        let access = [Access {
            pin: 9,
            net: 3,
            layer: 1,
            at: [90, 0],
        }];
        let refs: Vec<&Wire> = edges.iter().collect();
        let runs = collinear_runs(
            &refs,
            &vias.iter().collect::<Vec<_>>(),
            &[(0, 1)],
            &access.iter().collect::<Vec<_>>(),
            1000,
        )
        .unwrap();
        assert_eq!(
            runs,
            vec![
                w(3, 1, [0, 0], [20, 0]),
                w(3, 1, [20, 0], [40, 0]),
                w(3, 1, [40, 0], [90, 0]),
                w(3, 1, [90, 0], [100, 0]),
                w(3, 1, [40, 0], [40, 30]),
                w(3, 2, [70, -10], [70, 10]),
                w(4, 1, [200, 0], [210, 0]),
            ]
        );
        assert!(same_union(&edges, &runs, 5));
    }

    #[test]
    fn overlapping_stubs_and_crossings_keep_identical_metal() {
        let edges = [
            w(1, 0, [0, 0], [40, 0]),
            // Min-area stub overlapping the track from an interior anchor.
            w(1, 0, [20, 0], [65, 0]),
            // Crossing on the same layer (both lines cross at (30, 0)).
            w(1, 0, [30, -20], [30, 20]),
            // A collinear piece separated by a gap stays separate.
            w(1, 0, [90, 0], [100, 0]),
        ];
        let refs: Vec<&Wire> = edges.iter().collect();
        let runs = collinear_runs(&refs, &[], &[], &[], 1000).unwrap();
        assert_eq!(
            runs,
            vec![
                w(1, 0, [0, 0], [30, 0]),
                w(1, 0, [30, 0], [65, 0]),
                w(1, 0, [90, 0], [100, 0]),
                w(1, 0, [30, -20], [30, 0]),
                w(1, 0, [30, 0], [30, 20]),
            ]
        );
        assert!(same_union(&edges, &runs, 3));
    }

    #[test]
    fn pseudo_random_nets_preserve_the_union() {
        // Deterministic LCG; small lattice so unions are cheap to compare.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = |m: i64| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) % m as u64) as i64
        };
        for _ in 0..50 {
            let mut edges = Vec::new();
            for _ in 0..40 {
                let (x, y) = (next(8) * 10, next(8) * 10);
                let net = next(2) as u32;
                let layer = next(2) as u32;
                if next(2) == 0 {
                    edges.push(w(net, layer, [x, y], [x + 10 * (1 + next(3)), y]));
                } else {
                    edges.push(w(net, layer, [x, y], [x, y + 10 * (1 + next(3))]));
                }
            }
            let vias: Vec<Via> = (0..5)
                .map(|_| Via {
                    net: next(2) as u32,
                    kind: 0,
                    at: [next(8) * 10, next(8) * 10],
                })
                .collect();
            let refs: Vec<&Wire> = edges.iter().collect();
            let runs = collinear_runs(
                &refs,
                &vias.iter().collect::<Vec<_>>(),
                &[(0, 1)],
                &[],
                1_000_000,
            )
            .unwrap();
            // A via inside one random edge splits it, so the count can grow;
            // the metal union never changes.
            assert!(same_union(&edges, &runs, 2));
            // Idempotent: merging the result again changes nothing.
            let again = collinear_runs(
                &runs.iter().collect::<Vec<_>>(),
                &vias.iter().collect::<Vec<_>>(),
                &[(0, 1)],
                &[],
                1_000_000,
            )
            .unwrap();
            assert_eq!(again, runs);
            // No via point lies strictly inside a run on its landing layers.
            for v in &vias {
                for r in runs.iter().filter(|r| r.net == v.net) {
                    let inside = if r.start[0] == r.end[0] {
                        v.at[0] == r.start[0] && r.start[1] < v.at[1] && v.at[1] < r.end[1]
                    } else {
                        v.at[1] == r.start[1] && r.start[0] < v.at[0] && v.at[0] < r.end[0]
                    };
                    assert!(!inside, "via {v:?} inside run {r:?}");
                }
            }
        }
    }

    #[test]
    fn bad_input_and_budget_fail_explicitly() {
        let diagonal = [w(0, 0, [0, 0], [10, 10])];
        assert!(
            collinear_runs(&diagonal.iter().collect::<Vec<_>>(), &[], &[], &[], 10)
                .unwrap_err()
                .to_string()
                .contains("WIRE_RUNS")
        );
        let point = [w(0, 0, [5, 5], [5, 5])];
        assert!(collinear_runs(&point.iter().collect::<Vec<_>>(), &[], &[], &[], 10).is_err());
        let via = [Via {
            net: 0,
            kind: 3,
            at: [0, 0],
        }];
        assert!(collinear_runs(&[], &via.iter().collect::<Vec<_>>(), &[(0, 1)], &[], 10).is_err());
        let grid: Vec<Wire> = (0..20)
            .flat_map(|i| {
                [
                    w(0, 0, [0, i * 10], [200, i * 10]),
                    w(0, 0, [i * 10, 0], [i * 10, 200]),
                ]
            })
            .collect();
        let error =
            collinear_runs(&grid.iter().collect::<Vec<_>>(), &[], &[], &[], 100).unwrap_err();
        assert!(error.to_string().starts_with("BUDGET_EXCEEDED"), "{error}");
    }
}
