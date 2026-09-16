use std::time::{Duration, Instant};

const KEYFRAME_RETRY_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Default)]
pub struct VideoRecovery {
    last_sequence: Option<u64>,
    waiting_for_keyframe: bool,
    last_request: Option<Instant>,
}

impl VideoRecovery {
    pub fn require_keyframe(&mut self) {
        self.waiting_for_keyframe = true;
    }

    pub fn accept(&mut self, sequence: u64, keyframe: bool) -> bool {
        if self
            .last_sequence
            .is_none_or(|last| last.checked_add(1) != Some(sequence))
        {
            self.waiting_for_keyframe = true;
        }
        self.last_sequence = Some(sequence);
        if keyframe {
            self.waiting_for_keyframe = false;
        }
        !self.waiting_for_keyframe
    }

    pub fn request_due(&mut self, now: Instant) -> bool {
        if !self.waiting_for_keyframe
            || self
                .last_request
                .is_some_and(|last| now.saturating_duration_since(last) < KEYFRAME_RETRY_INTERVAL)
        {
            return false;
        }
        self.last_request = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_dependent_frames_until_a_fresh_keyframe() {
        let mut recovery = VideoRecovery::default();
        assert!(recovery.accept(0, true));
        assert!(recovery.accept(1, false));
        assert!(!recovery.accept(3, false));
        assert!(!recovery.accept(4, false));
        assert!(recovery.accept(7, true));
        assert!(recovery.accept(8, false));
    }

    #[test]
    fn accepting_a_keyframe_cancels_pending_retries() {
        let mut recovery = VideoRecovery::default();
        recovery.accept(0, true);
        recovery.require_keyframe();
        assert!(recovery.accept(2, true));
        assert!(!recovery.request_due(Instant::now()));
    }

    #[test]
    fn loss_requests_immediate_recovery_and_retains_rate_limited_retries() {
        let mut recovery = VideoRecovery::default();
        let now = Instant::now();
        recovery.accept(0, true);
        recovery.require_keyframe();
        assert!(recovery.request_due(now));
        for millis in 1..250 {
            recovery.require_keyframe();
            assert!(!recovery.request_due(now + Duration::from_millis(millis)));
        }
        assert!(!recovery.accept(1, false));
        assert!(recovery.request_due(now + KEYFRAME_RETRY_INTERVAL));
        assert!(recovery.accept(2, true));
        assert!(!recovery.request_due(now + KEYFRAME_RETRY_INTERVAL * 2));
    }
}
