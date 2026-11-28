use super::*;

#[test]
fn any_sequence_of_the_two_windows_overlap_adds_to_one_at_the_hop() {
    // Which window each frame uses: bit `frame` of the pattern.
    let patterns: [u64; 4] = [0, u64::MAX, 0x5555_5555_5555_5555, 0x0f0f_3c3c_00ff_0f0f];
    for mode in PitchMode::ALL {
        for rate in [48_000, 96_000, 192_000] {
            let windows = windows_for(mode, rate);
            let Spec { na, ns, hop, .. } = windows[0].spec;
            for pattern in patterns {
                let mut sum = vec![0.0f64; ns + hop];
                // A frame's synthesis window covers its samples na − ns … na − 1.
                for frame in 0..ns / hop + 1 {
                    let w = &windows[((pattern >> (frame % 64)) & 1) as usize];
                    for j in 0..ns {
                        let product = w.wa[na - ns + j] as f64 * w.ws[j] as f64;
                        if let Some(slot) = sum.get_mut(frame * hop + j) {
                            *slot += product;
                        }
                    }
                }
                for (i, value) in sum[ns - hop..ns].iter().enumerate() {
                    assert!(
                        (value - 1.0).abs() < 1e-5,
                        "{mode:?} @{rate} {pattern:x}: wa·ws sums to {value} at {i}"
                    );
                }
            }
        }
    }
}

#[test]
fn the_short_window_is_zero_before_its_support() {
    for mode in PitchMode::ALL {
        let [long, short] = windows_for(mode, 48_000);
        let Spec { na, .. } = long.spec;
        assert_eq!(long.support, na);
        assert!(short.wa[..na - short.support].iter().all(|&w| w == 0.0));
        assert!(short.wa[na - short.support] > 0.0);
    }
}
