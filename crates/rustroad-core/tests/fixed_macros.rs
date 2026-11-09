//! Resource estimate: four cells, four rows, one macro; <64 KiB including
//! serialization. Bounded row and overlap work; no path enumeration.
use rustroad_core::fixed::FixedPlacement;
use rustroad_core::{
    Cell, Design, Limits, Orientation, Rect, Row, Site, place_design_cpu_ref, validate_placement,
    write_def,
};
fn design() -> Design {
    Design {
        name: "hardmacro".into(),
        units_per_micron: 1000,
        die_area: Rect {
            lx: 0,
            ly: 0,
            ux: 20000,
            uy: 10000,
        },
        core_area: Rect {
            lx: 0,
            ly: 0,
            ux: 20000,
            uy: 10000,
        },
        site: Site {
            name: "core".into(),
            width: 100,
            height: 1000,
        },
        rows: (0..4)
            .map(|i| Row {
                x_start: 0,
                x_end: 20000,
                y: i * 1000,
                orientation: Orientation::N,
            })
            .collect(),
        cells: vec![
            Cell {
                name: "ram".into(),
                master: "SRAM".into(),
                width: 5000,
                height: 5000,
                fixed: Some(FixedPlacement {
                    x: 5000,
                    y: 2000,
                    orientation: Orientation::N,
                    halo: 1000,
                }),
            },
            Cell {
                name: "u0".into(),
                master: "BUF".into(),
                width: 10000,
                height: 1000,
                fixed: None,
            },
            Cell {
                name: "u1".into(),
                master: "BUF".into(),
                width: 10000,
                height: 1000,
                fixed: None,
            },
        ],
    }
}
#[test]
fn fixed_macro_survives_pack_and_def() {
    let d = design();
    let limits = Limits::default();
    let p = place_design_cpu_ref(&d, &limits).unwrap();
    assert_eq!((p.cells[0].x, p.cells[0].y), (5000, 2000));
    validate_placement(&d, &p.cells, &limits).unwrap();
    let mut out = Vec::new();
    write_def(&d, &p.cells, &mut out).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out)
            .lines()
            .filter(|l| l.starts_with("ROW "))
            .count(),
        7,
        "DEF must preserve the macro halo row exclusions"
    );
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("- ram SRAM + FIXED ( 5000 2000 ) N")
    );
}
#[test]
fn movement_and_halo_overlap_are_rejected() {
    let d = design();
    let limits = Limits::default();
    let mut p = place_design_cpu_ref(&d, &limits).unwrap();
    p.cells[0].x += 100;
    assert!(validate_placement(&d, &p.cells, &limits).is_err());
    p.cells[0].x -= 100;
    p.cells[1].x = 4500;
    p.cells[1].y = 2000;
    assert!(validate_placement(&d, &p.cells, &limits).is_err());
}
#[test]
fn row_subtraction_has_a_work_limit() {
    assert!(rustroad_core::fixed::row_segments(&design(), 1).is_err());
}

#[test]
fn row_disjoint_from_a_macro_never_grows_toward_its_halo() {
    let mut d = design();
    d.rows = vec![Row {
        x_start: 0,
        x_end: 3000,
        y: 2000,
        orientation: Orientation::N,
    }];
    let rows = rustroad_core::fixed::row_segments(&d, 8).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].x_start, rows[0].x_end, rows[0].y),
        (0, 3000, 2000),
        "a vertically overlapping macro outside the row must leave it unchanged"
    );
}

#[test]
fn macro_power_pin_exclamation_is_a_literal_def_name() {
    assert_eq!(rustroad_core::def_name("VDD!"), "VDD!");
    assert_eq!(rustroad_core::def_name("VDDARRAY!"), "VDDARRAY!");
    assert_eq!(rustroad_core::def_name("VSS!"), "VSS!");
}
