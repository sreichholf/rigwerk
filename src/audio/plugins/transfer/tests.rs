use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use super::{param_transfer, ParamReceiver, ParamSender};

fn drain(receiver: &mut ParamReceiver) -> Vec<(usize, f64, i32)> {
    let mut out = Vec::new();
    receiver.drain(|index, value, offset| out.push((index, value, offset)));
    out
}

#[test]
fn changes_arrive_in_order_with_their_offsets() {
    let (mut sender, mut receiver) = param_transfer(4);
    assert!(sender.push(2, 0.25, 0));
    assert!(sender.push(0, 1.0, 16));
    assert!(sender.push(2, 0.5, 31));
    assert_eq!(
        drain(&mut receiver),
        vec![(2, 0.25, 0), (0, 1.0, 16), (2, 0.5, 31)]
    );
    assert!(drain(&mut receiver).is_empty());
}

#[test]
fn bad_indices_and_values_are_refused() {
    let (mut sender, mut receiver) = param_transfer(3);
    assert!(!sender.push(3, 0.5, 0));
    assert!(!sender.push(usize::MAX, 0.5, 0));
    assert!(!sender.push(0, f64::NAN, 0));
    assert!(!sender.push(0, f64::INFINITY, 0));
    assert!(!sender.push(0, f64::NEG_INFINITY, 0));
    assert!(drain(&mut receiver).is_empty());
}

#[test]
fn a_negative_offset_is_stored_as_zero() {
    let (mut sender, mut receiver) = param_transfer(1);
    assert!(sender.push(0, 0.5, -12));
    assert_eq!(drain(&mut receiver), vec![(0, 0.5, 0)]);
}

#[test]
fn the_ring_holds_twice_the_parameters_rounded_up() {
    for (params, capacity) in [
        (0, 64),
        (1, 64),
        (32, 64),
        (33, 128),
        (40, 128),
        (64, 128),
        (65, 256),
    ] {
        let (sender, receiver) = param_transfer(params);
        assert_eq!(receiver.capacity(), capacity, "{params} parameters");
        assert_eq!(sender.params(), params);
        assert_eq!(receiver.params(), params);
    }
}

#[test]
fn no_parameters_still_works() {
    let (mut sender, mut receiver) = param_transfer(0);
    assert!(!sender.push(0, 0.5, 0));
    assert!(drain(&mut receiver).is_empty());
    receiver.discard();
}

#[test]
fn a_full_ring_overflows_without_losing_a_change() {
    let (mut sender, mut receiver) = param_transfer(4);
    let capacity = receiver.capacity();
    for n in 0..capacity {
        assert!(sender.push(0, n as f64, n as i32 % 8));
    }
    // Ring full: these overflow, and only the newest per parameter is kept.
    assert!(sender.push(1, 0.1, 50));
    assert!(sender.push(3, 0.3, 50));
    assert!(sender.push(1, 0.2, 60));
    let out = drain(&mut receiver);
    assert_eq!(out.len(), capacity + 2);
    for (n, &(index, value, offset)) in out[..capacity].iter().enumerate() {
        assert_eq!((index, value, offset), (0, n as f64, n as i32 % 8));
    }
    // After the ring, at the highest ring offset, in index order.
    assert_eq!(out[capacity..], [(1, 0.2, 7), (3, 0.3, 7)]);
    assert!(drain(&mut receiver).is_empty());
}

#[test]
fn a_pending_parameter_stays_in_order_across_the_overflow() {
    let (mut sender, mut receiver) = param_transfer(2);
    for _ in 0..receiver.capacity() {
        assert!(sender.push(0, 0.0, 0));
    }
    assert!(sender.push(1, 1.0, 0));
    let out = drain(&mut receiver);
    assert_eq!(out.last(), Some(&(1, 1.0, 0)));
    // B goes back to the ring: an overflow value would come at offset 0.
    assert!(sender.push(1, 2.0, 3));
    assert_eq!(drain(&mut receiver), vec![(1, 2.0, 3)]);
    assert!(drain(&mut receiver).is_empty());
}

#[test]
fn discard_empties_the_ring_and_the_overflow() {
    let (mut sender, mut receiver) = param_transfer(3);
    for _ in 0..receiver.capacity() {
        assert!(sender.push(0, 0.5, 0));
    }
    assert!(sender.push(2, 0.25, 0));
    receiver.discard();
    assert!(drain(&mut receiver).is_empty());
    assert!(sender.push(2, 0.75, 1));
    assert_eq!(drain(&mut receiver), vec![(2, 0.75, 1)]);
}

#[test]
fn two_threads_deliver_every_parameter_in_order() {
    const PARAMS: usize = 5;
    const PUSHES: usize = 20_000;
    let (mut sender, mut receiver) = param_transfer(PARAMS);
    let barrier = Arc::new(Barrier::new(2));
    let producer = {
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            for n in 0..PUSHES {
                for index in 0..PARAMS {
                    assert!(sender.push(index, n as f64, (n % 32) as i32));
                }
            }
        })
    };
    barrier.wait();
    let last = (PUSHES - 1) as f64;
    let mut seen = [f64::NEG_INFINITY; PARAMS];
    let started = Instant::now();
    while seen.iter().any(|&value| value != last) {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "stuck at {seen:?}"
        );
        receiver.drain(|index, value, _| {
            assert!(
                value > seen[index],
                "{index}: {value} after {}",
                seen[index]
            );
            seen[index] = value;
        });
        std::thread::yield_now();
    }
    producer.join().unwrap();
    assert!(drain(&mut receiver).is_empty());
}

fn values_of(changes: &[(usize, f64, i32)], index: usize) -> Vec<f64> {
    changes
        .iter()
        .filter(|change| change.0 == index)
        .map(|change| change.1)
        .collect()
}

/// Queues one entry for parameter 0, then drains with `during` running in
/// the first emit, as a producer that pushes while the consumer drains.
fn drain_racing(
    sender: &mut ParamSender,
    receiver: &mut ParamReceiver,
    during: impl FnOnce(&mut ParamSender),
) -> Vec<(usize, f64, i32)> {
    assert!(sender.push(0, -1.0, 0));
    let mut during = Some(during);
    let mut out = Vec::new();
    receiver.drain(|index, value, offset| {
        if let Some(during) = during.take() {
            during(sender);
        }
        out.push((index, value, offset));
    });
    out
}

/// Inside the first emit of a drain: one ring entry for parameter 1 past
/// the drain's head snapshot, the ring filled with parameter 0 (the drain
/// has not published `tail` yet), then `overflow` for parameter 1.
fn race_into_overflow(sender: &mut ParamSender, capacity: usize, overflow: f64) {
    assert!(sender.push(1, 1.0, 2));
    for _ in 0..capacity - 2 {
        assert!(sender.push(0, 0.0, 5));
    }
    assert!(sender.push(1, overflow, 9));
}

#[test]
fn a_value_overflowing_mid_drain_waits_for_the_ring_entries_ahead_of_it() {
    let (mut sender, mut receiver) = param_transfer(2);
    let capacity = receiver.capacity();
    let first = drain_racing(&mut sender, &mut receiver, |sender| {
        race_into_overflow(sender, capacity, 2.0);
    });
    assert_eq!(first, vec![(0, -1.0, 0)]);
    // The ring has room again, but the full slot keeps taking parameter 1.
    assert!(sender.push(1, 3.0, 1));
    let second = drain(&mut receiver);
    assert_eq!(values_of(&second, 1), [1.0, 3.0]);
    // Taken after the ring, at that drain's highest ring offset.
    assert_eq!(second.len(), capacity);
    assert_eq!(second.last(), Some(&(1, 3.0, 5)));
    assert!(drain(&mut receiver).is_empty());
    // Back to the ring once taken.
    assert!(sender.push(1, 4.0, 7));
    assert_eq!(drain(&mut receiver), vec![(1, 4.0, 7)]);
}

#[test]
fn overflow_reaches_every_bitset_word() {
    let (mut sender, mut receiver) = param_transfer(130);
    let capacity = receiver.capacity();
    assert_eq!(capacity, 512);
    for _ in 0..capacity {
        assert!(sender.push(0, 0.0, 3));
    }
    for index in [129, 64, 63] {
        assert!(sender.push(index, index as f64, 0));
    }
    let out = drain(&mut receiver);
    assert_eq!(
        out[capacity..],
        [(63, 63.0, 3), (64, 64.0, 3), (129, 129.0, 3)]
    );
    assert!(drain(&mut receiver).is_empty());
}

#[test]
fn discard_drops_a_deferred_value_with_the_ring_ahead_of_it() {
    let (mut sender, mut receiver) = param_transfer(2);
    let capacity = receiver.capacity();
    let first = drain_racing(&mut sender, &mut receiver, |sender| {
        race_into_overflow(sender, capacity, 2.0);
    });
    assert_eq!(first, vec![(0, -1.0, 0)]);
    receiver.discard();
    assert!(drain(&mut receiver).is_empty());
}

#[test]
fn a_discard_the_producer_races_keeps_the_newest_value() {
    // `discard` is a drain that emits nothing, so a drain whose changes are
    // ignored stands in for it while the producer pushes during it.
    let (mut sender, mut receiver) = param_transfer(2);
    let capacity = receiver.capacity();
    drain_racing(&mut sender, &mut receiver, |sender| {
        race_into_overflow(sender, capacity, 2.0);
    });
    // Parameter 1 lost only a prefix (nothing), and keeps its newest.
    assert_eq!(values_of(&drain(&mut receiver), 1), [1.0, 2.0]);
}

#[test]
fn discards_racing_a_producer_lose_only_old_changes() {
    const PARAMS: usize = 3;
    const PUSHES: usize = 20_000;
    let (mut sender, mut receiver) = param_transfer(PARAMS);
    let barrier = Arc::new(Barrier::new(2));
    let producer = {
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            for n in 0..PUSHES {
                for index in 0..PARAMS {
                    assert!(sender.push(index, n as f64, 0));
                }
            }
        })
    };
    barrier.wait();
    let mut seen = [f64::NEG_INFINITY; PARAMS];
    let mut since_discard = [false; PARAMS];
    let mut take = |receiver: &mut ParamReceiver, since_discard: &mut [bool; PARAMS]| {
        receiver.drain(|index, value, _| {
            assert!(
                value > seen[index],
                "{index}: {value} after {}",
                seen[index]
            );
            seen[index] = value;
            since_discard[index] = true;
        });
    };
    let started = Instant::now();
    let mut round = 0;
    while !producer.is_finished() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "producer stuck"
        );
        round += 1;
        if round % 3 == 0 {
            receiver.discard();
            since_discard = [false; PARAMS];
        } else {
            take(&mut receiver, &mut since_discard);
        }
        std::thread::yield_now();
    }
    producer.join().unwrap();
    take(&mut receiver, &mut since_discard);
    // A parameter that kept anything past the last discard kept its newest.
    let last = (PUSHES - 1) as f64;
    for (index, &kept) in since_discard.iter().enumerate() {
        if kept {
            assert_eq!(seen[index], last, "parameter {index}");
        }
    }
}

#[test]
fn has_queued_until_drained() {
    let (mut sender, mut receiver) = param_transfer(70);
    let capacity = receiver.capacity();
    assert!(!sender.has_queued());
    assert!(receiver.is_empty());

    assert!(sender.push(3, 0.5, 0));
    assert!(sender.has_queued());
    assert!(!receiver.is_empty());
    assert_eq!(drain(&mut receiver).len(), 1);
    assert!(!sender.has_queued());
    assert!(receiver.is_empty());

    // The overflow, in the second bitset word.
    for _ in 0..capacity {
        assert!(sender.push(0, 0.0, 0));
    }
    assert!(sender.push(69, 0.25, 0));
    let out = drain(&mut receiver);
    assert_eq!(out.len(), capacity + 1);
    assert!(!sender.has_queued());
    assert!(receiver.is_empty());

    // A value deferred behind ring entries a drain had not reached.
    let out = drain_racing(&mut sender, &mut receiver, |sender| {
        race_into_overflow(sender, capacity, 2.0)
    });
    assert_eq!(out.len(), 1);
    assert!(sender.has_queued());
    assert!(!receiver.is_empty());
    let out = drain(&mut receiver);
    assert_eq!(out.last(), Some(&(1, 2.0, 5)));
    assert!(!sender.has_queued());
    assert!(receiver.is_empty());

    assert!(sender.push(5, 0.5, 0));
    receiver.discard();
    assert!(!sender.has_queued());
    assert!(receiver.is_empty());
}

#[test]
fn a_value_counts_as_queued_while_a_drain_holds_its_bit() {
    // A drain swaps a word's bits to zero before it takes or defers each
    // value; the sender must not read that gap as drained.
    let (mut sender, mut receiver) = param_transfer(70);
    for _ in 0..receiver.capacity() {
        assert!(sender.push(0, 0.0, 0));
    }
    assert!(sender.push(69, 0.25, 0));
    let bits = receiver.inner.pending[1].swap(0, std::sync::atomic::Ordering::AcqRel);
    assert_ne!(bits, 0);
    assert!(sender.has_queued(), "the slot is still full");
    receiver.inner.pending[1].fetch_or(bits, std::sync::atomic::Ordering::AcqRel);
    assert_eq!(drain(&mut receiver).last(), Some(&(69, 0.25, 0)));
    assert!(!sender.has_queued());
}

/// A value the producer puts in the overflow while a drain runs, behind
/// ring entries that drain did not see, waits for the next drain, and the
/// drain says it deferred one.
#[test]
fn a_drain_reports_an_overflow_value_it_deferred() {
    let (mut sender, mut receiver) = param_transfer(2);
    let capacity = receiver.capacity();
    assert!(sender.push(0, 0.1, 0));
    let mut pushed_inside = false;
    let deferred = receiver.drain(|_, _, _| {
        if !pushed_inside {
            pushed_inside = true;
            // The drain has not published its tail yet: these fill the
            // ring, and the last one goes to the overflow behind them.
            for at in 0..capacity {
                sender.push(0, at as f64, 0);
            }
            sender.push(1, 0.9, 0);
        }
    });
    assert!(
        deferred,
        "the overflow value waits behind the new ring entries"
    );
    let mut later = Vec::new();
    let deferred = receiver.drain(|index, value, _| later.push((index, value)));
    assert!(!deferred);
    assert_eq!(later.last(), Some(&(1, 0.9)));
    assert!(!receiver.drain(|_, _, _| {}), "nothing left to defer");
}
