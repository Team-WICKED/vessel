//! Tests with access to the crate's internals: the published vectors run
//! against the same `snow` configuration the channel uses, and tampering
//! tests seal records that break the chunk rules yet authenticate.
//!
//! Every test waits on events (a record arriving, a handshake ending), not
//! on the clock; the deadline tests run on paused time.

#![expect(
    clippy::arithmetic_side_effects,
    reason = "tests compute sizes and offsets from small constants; an overflow would panic the \
              test, which is the right outcome"
)]

mod carriers;
mod fuzz;
mod handshake;
mod keys;
mod proof;
mod records;
mod support;
mod tamper;
mod transport;
mod vectors;
