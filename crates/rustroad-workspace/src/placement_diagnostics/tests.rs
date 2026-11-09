//! Source-authored correctness controls; execution requires separate build authority.
use super::*;

#[test]
fn byte_hold_and_tail_admission_are_atomic() {
    let mut meter = ByteLedger {
        charged: 7,
        held: 5,
        limit: 20,
    };
    assert!(meter.charge(9).is_err());
    assert_eq!(meter.charged, 7, "failed admission must not spend work");
    assert!(meter.admit_remaining(8).is_ok());
    assert!(meter.admit_remaining(9).is_err());
    assert_eq!(meter.final_phase(), 5);
    meter.charge(13).unwrap();
    assert!(meter.charge(1).is_err());
    assert_eq!(meter.charged, 20);
}

#[test]
fn full_publication_schedule_fits_the_released_hold() {
    // One actual source recheck: all five extents <=32MiB, HEAD<=64B.
    // Observation <=60MiB; summary<=64KiB; fixed CLI<=64KiB encoding+write.
    let schedule = 2 * (32 << 20) + 128 + 2 * (60 << 20) + 4 * (64 << 10) + 10 + 2 * (64 << 10);
    assert!(schedule < FINAL_HOLD);
    assert_eq!(FINAL_HOLD, 193 << 20);
}
