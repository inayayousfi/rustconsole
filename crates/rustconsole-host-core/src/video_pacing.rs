use std::fmt;
use std::time::{Duration, Instant};

pub struct VideoFramePacer {
    frame_period: Duration,
    next_tick: Instant,
    tick: u64,
    frame_divisor: u8,
}

impl VideoFramePacer {
    #[must_use]
    pub fn new(frames_per_second: u16, now: Instant) -> Option<Self> {
        Some(Self {
            frame_period: frame_period(frames_per_second)?,
            next_tick: now,
            tick: 0,
            frame_divisor: 1,
        })
    }

    #[must_use]
    pub const fn frame_period(&self) -> Duration {
        self.frame_period
    }

    #[must_use]
    pub fn wait_duration(&self, now: Instant) -> Duration {
        self.next_tick.saturating_duration_since(now)
    }

    #[must_use]
    pub fn should_encode(&self) -> bool {
        self.tick.is_multiple_of(u64::from(self.frame_divisor))
    }

    pub fn set_frame_divisor(&mut self, divisor: u8) -> Result<(), InvalidFrameDivisor> {
        if !matches!(divisor, 1 | 2) {
            return Err(InvalidFrameDivisor(divisor));
        }
        self.frame_divisor = divisor;
        Ok(())
    }

    pub fn complete_tick(&mut self, started: Instant, completed: Instant) {
        self.tick = self.tick.wrapping_add(1);
        // Rebase after an overrun instead of issuing catch-up captures.
        self.next_tick = (started + self.frame_period).max(completed);
    }
}

#[must_use]
pub fn frame_period(frames_per_second: u16) -> Option<Duration> {
    (frames_per_second != 0)
        .then(|| Duration::from_nanos(1_000_000_000 / u64::from(frames_per_second)))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidFrameDivisor(u8);

impl fmt::Display for InvalidFrameDivisor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "video frame divisor {} is not one or two",
            self.0
        )
    }
}

impl std::error::Error for InvalidFrameDivisor {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_overrun_rebases_without_skipping_an_extra_slot() {
        let started = Instant::now();
        let mut pacer = VideoFramePacer::new(100, started).unwrap();
        pacer.complete_tick(started, started + Duration::from_millis(11));
        assert_eq!(pacer.wait_duration(started), Duration::from_millis(11));
    }

    #[test]
    fn large_stall_does_not_create_catch_up_bursts() {
        let started = Instant::now();
        let mut pacer = VideoFramePacer::new(100, started).unwrap();
        let resumed = started + Duration::from_millis(60);
        pacer.complete_tick(started, resumed);
        assert_eq!(pacer.wait_duration(resumed), Duration::ZERO);
        pacer.complete_tick(resumed, resumed + Duration::from_millis(2));
        assert_eq!(pacer.wait_duration(resumed), Duration::from_millis(10));
    }

    #[test]
    fn divisor_selects_frames_without_changing_the_capture_clock() {
        let started = Instant::now();
        let mut pacer = VideoFramePacer::new(120, started).unwrap();
        pacer.set_frame_divisor(2).unwrap();
        assert!(pacer.should_encode());
        pacer.complete_tick(started, started);
        assert!(!pacer.should_encode());
        pacer.complete_tick(started, started);
        assert!(pacer.should_encode());
        assert!(pacer.set_frame_divisor(3).is_err());
    }
}
