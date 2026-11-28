use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use super::queue;

fn ignore(_: u32) {}

#[test]
fn values_come_out_in_order_and_a_full_queue_hands_the_value_back() {
    let (mut tx, mut rx) = queue(3, ignore);
    assert_eq!(tx.room(), 4);
    for value in 0..4 {
        assert!(tx.push(value).is_ok());
    }
    assert_eq!(tx.room(), 0);
    assert_eq!(tx.push(9), Err(9));
    assert_eq!(rx.peek(), Some(&0), "peek leaves the value queued");
    assert_eq!(rx.pop(), Some(0));
    assert_eq!(tx.room(), 1);
    assert!(tx.push(4).is_ok());
    let rest: Vec<u32> = std::iter::from_fn(|| rx.pop()).collect();
    assert_eq!(rest, [1, 2, 3, 4]);
    assert_eq!(rx.peek(), None);
    assert_eq!(rx.pop(), None);
}

#[test]
fn the_queue_wraps_around_many_times() {
    let (mut tx, mut rx) = queue(4, ignore);
    for value in 0..1_000u32 {
        assert!(tx.push(value).is_ok());
        assert!(tx.push(value + 1).is_ok());
        assert_eq!(rx.pop(), Some(value));
        assert_eq!(rx.pop(), Some(value + 1));
    }
    assert_eq!(tx.room(), 4);
}

static RELEASED: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn record(value: String) {
    RELEASED.lock().unwrap().push(value);
}

#[test]
fn values_left_when_both_ends_drop_go_to_release() {
    let (mut tx, mut rx) = queue(4, record);
    for name in ["a", "b", "c"] {
        assert!(tx.push(name.to_owned()).is_ok());
    }
    assert_eq!(rx.pop().as_deref(), Some("a"));
    drop(tx);
    assert!(
        RELEASED.lock().unwrap().is_empty(),
        "the consumer still owns them"
    );
    drop(rx);
    assert_eq!(*RELEASED.lock().unwrap(), ["b", "c"]);
}

static DROPS: AtomicUsize = AtomicUsize::new(0);

struct Counted(u64);

impl Drop for Counted {
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn a_producer_and_a_consumer_thread_pass_every_value_once_and_in_order() {
    const COUNT: u64 = 100_000;
    let (mut tx, mut rx) = queue(16, drop::<Box<Counted>>);
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let mut next = 0;
            let mut value = Box::new(Counted(next));
            loop {
                match tx.push(value) {
                    Ok(()) => {
                        next += 1;
                        if next == COUNT {
                            break;
                        }
                        value = Box::new(Counted(next));
                    }
                    Err(back) => {
                        value = back;
                        std::thread::yield_now();
                    }
                }
            }
        });
        let mut expected = 0;
        while expected < COUNT {
            match rx.pop() {
                Some(value) => {
                    assert_eq!(value.0, expected);
                    expected += 1;
                }
                None => std::thread::yield_now(),
            }
        }
    });
    assert_eq!(DROPS.load(Ordering::SeqCst), COUNT as usize);
}
