use super::{QuantumInfo, PIPEWIRE_LOW_QUANTUM};

#[test]
fn an_equal_period_and_quantum_fit() {
    let info = QuantumInfo::assess(true, 64, 48_000, 64);
    assert!(info.low_latency);
    assert!(info.fits);
}

#[test]
fn an_alsa_period_that_divides_the_quantum_fits() {
    let info = QuantumInfo::assess(true, PIPEWIRE_LOW_QUANTUM, 48_000, 32);
    assert!(info.fits);
    assert!(info.note.contains("fits 2"));
}

#[test]
fn a_coarser_alsa_period_is_reported_as_a_collision() {
    let info = QuantumInfo::assess(true, 64, 48_000, 128);
    assert!(!info.fits);
    assert!(info.note.contains("larger than the PipeWire quantum"));
}

#[test]
fn a_period_that_does_not_divide_the_quantum_is_reported() {
    let info = QuantumInfo::assess(true, 64, 48_000, 48);
    assert!(!info.fits);
    assert!(info.note.contains("does not divide"));
}

#[test]
fn a_large_quantum_is_marked_as_shared_not_low_latency() {
    let info = QuantumInfo::assess(true, 1024, 48_000, 64);
    assert!(!info.low_latency);
    assert!(info.note.contains("Desktop PipeWire graph"));
    assert!(info.note.contains("not the OUT CLOCK"));
    assert!(info.note.contains("shared"));
}

#[test]
fn an_unreadable_quantum_says_so_instead_of_pretending() {
    let info = QuantumInfo::assess(false, 0, 0, 128);
    assert!(!info.available);
    assert!(!info.fits);
    assert!(info.note.contains("not readable"));
}
