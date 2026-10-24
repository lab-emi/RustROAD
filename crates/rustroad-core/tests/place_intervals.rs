//! Public-API placement test: tap-split intervals, cell spacing, determinism.
//! Tiny self-authored problem (< 1 KiB of state); no PDK data.

use rustroad_core::place::{Interval, PinRef, PlaceConfig, PlaceLimits, PlaceProblem, hpwl, place};

fn problem() -> PlaceProblem {
    // A chain of 12 cells between two fixed pins, widths 30 and 10.
    let n = 12usize;
    let mut nets = vec![vec![PinRef::Fixed([0, 50]), PinRef::Cell(0)]];
    for i in 1..n {
        nets.push(vec![PinRef::Cell(i as u32 - 1), PinRef::Cell(i as u32)]);
    }
    nets.push(vec![PinRef::Cell(n as u32 - 1), PinRef::Fixed([290, 150])]);
    PlaceProblem {
        weights: Vec::new(),
        widths: (0..n).map(|i| if i % 3 == 0 { 30 } else { 10 }).collect(),
        row_height: 100,
        site: 10,
        gap: 10,
        nets,
        // Two rows, each split by taps into [0,90) [100,190) [200,290).
        intervals: (0..2)
            .flat_map(|r| {
                [(0, 90), (100, 190), (200, 290)].map(|(lx, ux)| Interval { y: r * 100, lx, ux })
            })
            .collect(),
    }
}

#[test]
fn cells_land_inside_tap_intervals_with_spacing_and_repeatably() {
    let p = problem();
    let r = place(&p, &PlaceConfig::default(), &PlaceLimits::default()).unwrap();
    assert_eq!(r.positions.len(), p.widths.len());
    let mut by_interval: Vec<Vec<(i64, i64)>> = vec![Vec::new(); p.intervals.len()];
    for (c, pos) in r.positions.iter().enumerate() {
        let k = p
            .intervals
            .iter()
            .position(|v| v.y == pos[1] && v.lx <= pos[0] && pos[0] + p.widths[c] <= v.ux)
            .expect("every cell lies inside one free interval");
        assert_eq!((pos[0] - p.intervals[k].lx) % p.site, 0, "site grid");
        by_interval[k].push((pos[0], pos[0] + p.widths[c]));
    }
    for cells in &mut by_interval {
        cells.sort_unstable();
        for w in cells.windows(2) {
            assert!(w[1].0 >= w[0].1 + p.gap, "spacing kept: {w:?}");
        }
    }
    assert_eq!(r.hpwl, hpwl(&p, &r.positions));
    let again = place(&p, &PlaceConfig::default(), &PlaceLimits::default()).unwrap();
    assert_eq!(r.positions, again.positions);
}
