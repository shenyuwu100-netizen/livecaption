//! Capture-loop liveness, independent of audio volume and speech recognition.
use std::time::{Duration, Instant};

pub const STALLED_AFTER: Duration = Duration::from_secs(10);
pub const START_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub health: &'static str,
    pub heartbeat_age_ms: Option<u64>,
    pub reason: &'static str,
}

#[derive(Default)]
enum Phase {
    #[default]
    Off,
    Starting,
    Healthy,
    Error,
    NotApplicable,
}

#[derive(Default)]
pub struct Tracker {
    revision: Option<u64>,
    phase: Phase,
    started_at: Option<Instant>,
    last_tick: Option<Instant>,
    reason: &'static str,
}

impl Tracker {
    pub fn start(&mut self, revision: u64, desktop: bool, now: Instant) {
        *self = Self {
            revision: Some(revision),
            phase: if desktop {
                Phase::Starting
            } else {
                Phase::NotApplicable
            },
            started_at: desktop.then_some(now),
            ..Self::default()
        };
    }

    pub fn stop(&mut self) {
        *self = Self::default();
    }

    fn accepts(&self, revision: u64, cancelled: bool) -> bool {
        !cancelled
            && self.revision == Some(revision)
            && matches!(self.phase, Phase::Starting | Phase::Healthy | Phase::Error)
    }

    pub fn tick(&mut self, revision: u64, cancelled: bool, now: Instant) -> bool {
        if !self.accepts(revision, cancelled) {
            return false;
        }
        self.phase = Phase::Healthy;
        self.last_tick = Some(now);
        self.reason = "";
        true
    }

    pub fn fail(&mut self, revision: u64, cancelled: bool, reason: &'static str) -> bool {
        if !self.accepts(revision, cancelled) {
            return false;
        }
        self.phase = Phase::Error;
        self.reason = reason;
        true
    }

    pub fn snapshot(&self, now: Instant) -> Snapshot {
        let heartbeat_age = self.last_tick.map(|at| now.saturating_duration_since(at));
        let (health, reason) = match self.phase {
            Phase::Off => ("off", ""),
            Phase::NotApplicable => ("not_applicable", ""),
            Phase::Error => ("error", self.reason),
            Phase::Starting => {
                if self
                    .started_at
                    .is_some_and(|at| now.saturating_duration_since(at) >= START_TIMEOUT)
                {
                    ("stalled", "startup_timeout")
                } else {
                    ("starting", "")
                }
            }
            Phase::Healthy => {
                if heartbeat_age.is_some_and(|age| age >= STALLED_AFTER) {
                    ("stalled", "heartbeat_timeout")
                } else {
                    ("healthy", "")
                }
            }
        };
        Snapshot {
            health,
            heartbeat_age_ms: heartbeat_age.map(|age| age.as_millis().min(u64::MAX as u128) as u64),
            reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_tracker_is_off_without_a_heartbeat() {
        assert_eq!(
            Tracker::default().snapshot(Instant::now()),
            Snapshot {
                health: "off",
                heartbeat_age_ms: None,
                reason: "",
            }
        );
    }

    #[test]
    fn desktop_start_waits_for_its_own_capture_tick() {
        let now = Instant::now();
        let mut tracker = Tracker::default();
        tracker.start(1, true, now);
        assert_eq!(tracker.snapshot(now).health, "starting");
        assert_eq!(tracker.snapshot(now).heartbeat_age_ms, None);
    }

    #[test]
    fn capture_tick_makes_the_source_healthy_and_ages_independently() {
        let now = Instant::now();
        let mut tracker = Tracker::default();
        tracker.start(1, true, now);
        assert!(tracker.tick(1, false, now));
        let snapshot = tracker.snapshot(now + Duration::from_millis(2500));
        assert_eq!(snapshot.health, "healthy");
        assert_eq!(snapshot.heartbeat_age_ms, Some(2500));
    }

    #[test]
    fn prolonged_silence_is_healthy_when_capture_keeps_ticking() {
        let now = Instant::now();
        let mut tracker = Tracker::default();
        tracker.start(1, true, now);
        // A tick represents a completed capture iteration, including zero frames.
        for seconds in 0..60 {
            let at = now + Duration::from_secs(seconds);
            assert!(tracker.tick(1, false, at));
            assert_eq!(tracker.snapshot(at).health, "healthy");
        }
    }

    #[test]
    fn missing_capture_ticks_stall_even_if_the_reporter_is_alive() {
        let now = Instant::now();
        let mut tracker = Tracker::default();
        tracker.start(1, true, now);
        tracker.tick(1, false, now);
        assert_eq!(
            tracker
                .snapshot(now + STALLED_AFTER - Duration::from_millis(1))
                .health,
            "healthy"
        );
        let snapshot = tracker.snapshot(now + STALLED_AFTER);
        assert_eq!(snapshot.health, "stalled");
        assert_eq!(snapshot.reason, "heartbeat_timeout");
    }

    #[test]
    fn startup_has_a_bounded_grace_period() {
        let now = Instant::now();
        let mut tracker = Tracker::default();
        tracker.start(1, true, now);
        assert_eq!(
            tracker
                .snapshot(now + START_TIMEOUT - Duration::from_millis(1))
                .health,
            "starting"
        );
        let snapshot = tracker.snapshot(now + START_TIMEOUT);
        assert_eq!(snapshot.health, "stalled");
        assert_eq!(snapshot.reason, "startup_timeout");
    }

    #[test]
    fn explicit_capture_failure_stays_error_until_capture_progresses_again() {
        let now = Instant::now();
        let mut tracker = Tracker::default();
        tracker.start(1, true, now);
        tracker.tick(1, false, now);
        assert!(tracker.fail(1, false, "capture_failed"));
        let snapshot = tracker.snapshot(now + START_TIMEOUT);
        assert_eq!(snapshot.health, "error");
        assert_eq!(snapshot.reason, "capture_failed");
        assert!(tracker.tick(1, false, now + START_TIMEOUT));
        assert_eq!(tracker.snapshot(now + START_TIMEOUT).reason, "");
    }

    #[test]
    fn stale_revision_cannot_change_the_new_capture_session() {
        let now = Instant::now();
        let mut tracker = Tracker::default();
        tracker.start(1, true, now);
        tracker.start(2, true, now);
        assert!(!tracker.tick(1, false, now));
        assert!(!tracker.fail(1, false, "worker_ended"));
        assert_eq!(tracker.snapshot(now).health, "starting");
    }

    #[test]
    fn cancellation_fences_updates_even_when_a_revision_is_reused() {
        let now = Instant::now();
        let mut tracker = Tracker::default();
        tracker.start(1, true, now);
        assert!(!tracker.tick(1, true, now));
        assert!(!tracker.fail(1, true, "worker_ended"));
        assert_eq!(tracker.snapshot(now).health, "starting");
    }

    #[test]
    fn stop_is_sticky_against_late_worker_updates() {
        let now = Instant::now();
        let mut tracker = Tracker::default();
        tracker.start(1, true, now);
        tracker.tick(1, false, now);
        tracker.stop();
        assert!(!tracker.tick(1, false, now + Duration::from_secs(1)));
        assert!(!tracker.fail(1, false, "worker_ended"));
        assert_eq!(tracker.snapshot(now).health, "off");
        assert_eq!(tracker.snapshot(now).heartbeat_age_ms, None);
    }

    #[test]
    fn manual_ipad_source_never_looks_like_a_stalled_desktop_capture() {
        let now = Instant::now();
        let mut tracker = Tracker::default();
        tracker.start(1, false, now);
        assert!(!tracker.tick(1, false, now));
        assert!(!tracker.fail(1, false, "capture_failed"));
        let snapshot = tracker.snapshot(now + Duration::from_secs(120));
        assert_eq!(snapshot.health, "not_applicable");
        assert_eq!(snapshot.heartbeat_age_ms, None);
    }

    #[test]
    fn unexpected_worker_exit_has_a_distinct_reason() {
        let now = Instant::now();
        let mut tracker = Tracker::default();
        tracker.start(1, true, now);
        assert!(tracker.fail(1, false, "worker_ended"));
        assert_eq!(tracker.snapshot(now).reason, "worker_ended");
    }
}

#[cfg(test)]
mod recognition_tests {
    use super::*;
    #[test]
    fn silence_and_one_failure_do_not_trigger_takeover() {
        let mut tracker = Recognition::default();
        assert_eq!(tracker.health(), "off");
        tracker.start(7);
        assert_eq!(tracker.health(), "healthy");
        tracker.result(7, false, false);
        assert_eq!(tracker.health(), "healthy");
        tracker.result(7, false, false);
        assert_eq!(tracker.health(), "error");
        tracker.result(7, false, true);
        assert_eq!(tracker.health(), "healthy");
    }
    #[test]
    fn stale_and_cancelled_results_cannot_revive_stopped_recognition() {
        let mut tracker = Recognition::default();
        tracker.start(8);
        tracker.result(7, false, false);
        tracker.result(8, true, false);
        assert_eq!(tracker.health(), "healthy");
        tracker.stop();
        tracker.result(8, false, false);
        tracker.result(8, false, false);
        assert_eq!(tracker.health(), "off");
    }
}

/// Consecutive *real* recognition failures, never lack of speech.
#[derive(Default)]
pub struct Recognition {
    revision: Option<u64>,
    failures: u8,
}
impl Recognition {
    pub fn start(&mut self, revision: u64) {
        self.revision = Some(revision);
        self.failures = 0;
    }
    pub fn stop(&mut self) {
        *self = Self::default();
    }
    pub fn result(&mut self, revision: u64, cancelled: bool, success: bool) {
        if cancelled || self.revision != Some(revision) {
            return;
        }
        self.failures = if success {
            0
        } else {
            self.failures.saturating_add(1)
        };
    }
    pub fn health(&self) -> &'static str {
        if self.revision.is_none() {
            "off"
        } else if self.failures >= 2 {
            "error"
        } else {
            "healthy"
        }
    }
}
