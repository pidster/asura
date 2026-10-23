//! The shared pacing sequence (owner pattern of 2026-10-19, PACE-1).
//!
//! Delays of 1, 1, 2, 3, 5, 8 s and so on, each the sum of the two before,
//! at most 60 s apart. Pacing only: it never ends or refuses work. The type
//! is pure arithmetic and makes no OS call; each user owns its own wake and
//! reset rules.

use std::time::Duration;

/// Fibonacci pacing from [`Fibonacci::START`] up to [`Fibonacci::CAP`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fibonacci {
    previous: Duration,
    current: Duration,
}

impl Fibonacci {
    /// The first delay.
    pub const START: Duration = Duration::from_secs(1);
    /// The longest delay.
    pub const CAP: Duration = Duration::from_secs(60);

    /// A sequence at its start: the next delay is [`Self::START`].
    pub const fn new() -> Self {
        Self {
            previous: Duration::ZERO,
            current: Self::START,
        }
    }

    /// The next delay; advances the sequence.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = self.previous.saturating_add(delay).min(Self::CAP);
        self.previous = delay;
        delay
    }

    /// Restarts the sequence: the next delay is [`Self::START`].
    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

impl Default for Fibonacci {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PACE-1: 1, 1, 2, 3, 5 ... s, stopping at 60 s; `reset` restarts it.
    #[test]
    fn delays_grow_to_the_cap_and_reset_restarts() {
        let mut pacing = Fibonacci::new();
        let delays: Vec<u64> = (0..12).map(|_| pacing.next_delay().as_secs()).collect();
        assert_eq!(delays, [1, 1, 2, 3, 5, 8, 13, 21, 34, 55, 60, 60]);
        pacing.reset();
        assert_eq!(pacing, Fibonacci::default());
        let delays: Vec<u64> = (0..3).map(|_| pacing.next_delay().as_secs()).collect();
        assert_eq!(delays, [1, 1, 2]);
    }
}
