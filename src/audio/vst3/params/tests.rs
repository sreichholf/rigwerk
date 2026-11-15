use std::sync::Arc;

use ::vst3::ComRef;
use ::vst3::Steinberg::Vst::{IParamValueQueueTrait, IParameterChangesTrait};
use ::vst3::Steinberg::{kInvalidArgument, kResultOk};

use super::{ParamValueQueue, ParameterChanges};
use crate::audio::plugins::ParamIds;

#[test]
fn parameter_changes_keep_offset_order_and_replace_same_offset() {
    let queue = ParamValueQueue::new(4);
    assert_eq!(queue.add(10, 0.1), 0);
    assert_eq!(queue.add(2, 0.2), 0);
    assert_eq!(queue.add(6, 0.3), 1);
    // The same offset replaces the value in place.
    assert_eq!(queue.add(6, 0.4), 1);
    assert_eq!(queue.points(), vec![(2, 0.2), (6, 0.4), (10, 0.1)]);
    assert_eq!(queue.last(), Some(0.1));
    // Full: the newest point still goes in and the last one makes room.
    queue.add(12, 0.5);
    queue.add(14, 0.6);
    assert_eq!(
        queue.points(),
        vec![(2, 0.2), (6, 0.4), (10, 0.1), (14, 0.6)]
    );
    queue.add(4, 0.7);
    assert_eq!(
        queue.points(),
        vec![(2, 0.2), (4, 0.7), (6, 0.4), (10, 0.1)]
    );

    let mut offset = -1;
    let mut value = 0.0;
    // SAFETY: writable out pointers.
    unsafe {
        assert_eq!(queue.getPointCount(), 4);
        assert_eq!(queue.getPoint(1, &mut offset, &mut value), kResultOk);
        assert_eq!(queue.getPoint(4, &mut offset, &mut value), kInvalidArgument);
    }
    assert_eq!((offset, value), (4, 0.7));
}

#[test]
fn parameter_changes_claim_one_queue_per_parameter_and_clear() {
    let ids = Arc::new(ParamIds::new(vec![7, 3, 9]));
    let changes = ParameterChanges::new(ids, 4);
    assert!(changes.is_empty());
    changes.push(2, 0, 0.5);
    changes.push(0, 0, 0.25);
    changes.push(2, 8, 0.75);
    // SAFETY: the list is live; the ids are readable.
    unsafe {
        assert_eq!(changes.getParameterCount(), 2);
        let first = changes.getParameterData(0);
        assert!(!first.is_null());
        assert_eq!(ComRef::from_raw(first).unwrap().getParameterId(), 9);
        assert!(changes.getParameterData(2).is_null());
        // The plugin's own output: a known id gets its queue, an unknown one
        // the scratch queue, which is never listed.
        let mut index = -1;
        let known = changes.addParameterData(&3, &mut index);
        assert_eq!(index, 2);
        assert_eq!(ComRef::from_raw(known).unwrap().getParameterId(), 3);
        let unknown = changes.addParameterData(&42, &mut index);
        assert!(!unknown.is_null());
        assert_eq!(index, 3);
        assert_eq!(changes.getParameterCount(), 3);
    }
    assert_eq!(changes.points(2), vec![(0, 0.5), (8, 0.75)]);
    let mut last = Vec::new();
    changes.for_each_last(|index, value| last.push((index, value)));
    // The queue of id 3 has no points, so it reports nothing.
    assert_eq!(last, vec![(2, 0.75), (0, 0.25)]);

    changes.clear();
    assert!(changes.is_empty());
    assert!(changes.points(2).is_empty());
    changes.push(1, 0, 1.0);
    assert_eq!(changes.points(1), vec![(0, 1.0)]);
}
