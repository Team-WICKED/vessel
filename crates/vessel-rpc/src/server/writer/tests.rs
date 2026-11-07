//! Unit tests of the writer's flush policy.

use super::*;

#[test]
fn a_burst_is_flushed_after_a_bounded_number_of_frames_or_bytes() {
    let mut unflushed = Unflushed::default();
    assert!(!unflushed.pending());
    for _ in 1..FLUSH_FRAMES {
        assert!(!unflushed.add(10));
    }
    assert!(unflushed.pending());
    assert!(unflushed.add(10), "the frame limit calls for a flush");
    unflushed.reset();
    assert!(unflushed.add(FLUSH_BYTES), "so does the byte limit");
}
