use super::contain;

#[test]
fn a_body_that_returns_is_not_a_failure() {
    let mut ran = false;

    assert!(!contain(|| ran = true));
    assert!(ran);
}

#[test]
fn a_panicking_body_is_caught() {
    assert!(contain(|| panic!("a plugin failed inside the callback")));
}

/// The guard only reports: the caller's next statements still run, which is
/// the silence-and-flag path every callback takes.
#[test]
fn the_caller_continues_after_a_caught_panic() {
    let mut silenced = false;

    if contain(|| panic!("a plugin failed inside the callback")) {
        silenced = true;
    }

    assert!(silenced);
}
