//! Display staleness counted in missed service heartbeats (owner, 2026-10-16).
//!
//! The service sends a heartbeat on each one-second long-poll cycle. A
//! projection is stale after [`STALE_AFTER_MISSED`] consecutive missed
//! heartbeats. Staleness changes the display only; it ends no work, and the
//! wake-up it needs is display pacing.
use std::time::{Duration, Instant};

/// The service heartbeat cadence these counts are measured in.
pub(super) const INTERVAL: Duration = Duration::from_secs(1);
/// Consecutive missed heartbeats that mark a projection stale.
pub(super) const STALE_AFTER_MISSED: u32 = 5;
/// Heartbeat intervals a `git_changed` display grace lasts.
pub(super) const GIT_GRACE_INTERVALS: u32 = 2;

/// Heartbeats missed since the last received one.
pub(super) fn missed(seen: Instant, now: Instant) -> u32 {
    let elapsed = now.saturating_duration_since(seen).as_millis();
    u32::try_from(elapsed / INTERVAL.as_millis()).unwrap_or(u32::MAX)
}
/// True once [`STALE_AFTER_MISSED`] consecutive heartbeats were missed.
pub(super) fn stale(seen: Instant, now: Instant) -> bool {
    missed(seen, now) >= STALE_AFTER_MISSED
}
/// When the display must next re-check staleness (pacing, ends nothing).
pub(super) fn stale_at(seen: Instant) -> Instant {
    seen + INTERVAL * STALE_AFTER_MISSED
}
/// The end of a `git_changed` grace that started at `event`.
pub(super) fn git_grace_end(event: Instant) -> Instant {
    event + INTERVAL * GIT_GRACE_INTERVALS
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn staleness_counts_missed_heartbeats_not_a_deadline() {
        let seen = Instant::now();
        for count in 0..STALE_AFTER_MISSED {
            let now = seen + INTERVAL * count + INTERVAL / 2;
            assert_eq!(missed(seen, now), count);
            assert!(!stale(seen, now));
        }
        assert!(stale(seen, seen + INTERVAL * STALE_AFTER_MISSED));
        assert_eq!(stale_at(seen), seen + INTERVAL * STALE_AFTER_MISSED);
        assert_eq!(git_grace_end(seen), seen + INTERVAL * GIT_GRACE_INTERVALS);
        // A received heartbeat resets the count.
        let next = seen + INTERVAL * 4;
        assert_eq!(missed(next, next), 0);
    }
}
