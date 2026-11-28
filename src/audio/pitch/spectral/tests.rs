use super::*;

#[test]
fn the_window_pair_overlap_adds_to_one_at_the_hop() {
    for mode in PitchMode::ALL {
        for rate in [48_000, 96_000, 192_000] {
            let windows = windows_for(mode, rate);
            let Spec { na, ns, hop, .. } = windows.spec;
            let mut sum = vec![0.0f64; ns + hop];
            // A frame's synthesis window covers its samples na − ns … na − 1.
            for frame in 0..ns / hop + 1 {
                for j in 0..ns {
                    let product = windows.wa[na - ns + j] as f64 * windows.ws[j] as f64;
                    if let Some(slot) = sum.get_mut(frame * hop + j) {
                        *slot += product;
                    }
                }
            }
            for (i, value) in sum[ns - hop..ns].iter().enumerate() {
                assert!(
                    (value - 1.0).abs() < 1e-5,
                    "{mode:?} @{rate}: wa·ws sums to {value} at {i}"
                );
            }
        }
    }
}
