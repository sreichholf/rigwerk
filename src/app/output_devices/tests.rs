use super::device_channel_count;
use crate::audio::MAX_OUTPUT_CHANNELS;

#[test]
fn channel_count_prefers_the_stream_then_the_scan() {
    assert_eq!(device_channel_count(6, 2), 6);
    assert_eq!(device_channel_count(0, 4), 4);
    assert_eq!(device_channel_count(0, 0), 2);
    assert_eq!(device_channel_count(1, 0), 1);
    assert_eq!(device_channel_count(32, 2), MAX_OUTPUT_CHANNELS);
    assert_eq!(device_channel_count(0, 16), MAX_OUTPUT_CHANNELS);
}
