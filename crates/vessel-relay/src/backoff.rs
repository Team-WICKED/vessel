//! Reconnect delays: 0.5 s doubling to 30 s, each drawn between half and
//! all of the current step, so nodes that lost the same relay do not all
//! come back at once (the shape of `vessel-client`'s `follow_resuming`).

use std::time::Duration;

const FIRST: Duration = Duration::from_millis(500);
const MAX: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub(crate) struct Backoff {
    step: Duration,
}

impl Backoff {
    pub(crate) fn new() -> Self {
        Self { step: FIRST }
    }

    /// Back to the first step, after a connection that worked.
    pub(crate) fn reset(&mut self) {
        self.step = FIRST;
    }

    /// Wait the longest step next (a refusal will not fix itself soon).
    pub(crate) fn exhaust(&mut self) {
        self.step = MAX;
    }

    /// The next delay.
    pub(crate) fn next_delay(&mut self) -> Duration {
        let step = self.step;
        self.step = step.saturating_mul(2).min(MAX);
        let millis = u64::try_from(step.as_millis()).unwrap_or(u64::MAX);
        let half = millis >> 1;
        let mut random = [0_u8; 8];
        let jitter = match getrandom::fill(&mut random) {
            Ok(()) => u64::from_le_bytes(random)
                .checked_rem(half.saturating_add(1))
                .unwrap_or(0),
            Err(_unavailable) => half,
        };
        Duration::from_millis(half.saturating_add(jitter))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_within_their_jitter_up_to_the_cap() {
        let mut backoff = Backoff::new();
        for expected in [500, 1000, 2000, 4000, 8000, 16_000, 30_000, 30_000] {
            let delay = backoff.next_delay().as_millis();
            assert!(
                (expected >> 1..=expected).contains(&delay),
                "{delay} not within {expected}"
            );
        }
        backoff.reset();
        assert!(backoff.next_delay() <= FIRST);
        backoff.exhaust();
        assert!(backoff.next_delay() >= Duration::from_secs(15));
    }
}
