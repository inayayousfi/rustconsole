use std::fmt;
use std::time::{Duration, Instant};

/// Encoded data may run this far ahead of the target before frames are skipped. It covers a
/// keyframe at normal bitrates, so skipping happens only when the encoder cannot reach the target.
const FRAME_BUDGET_ALLOWANCE_DIVISOR: u64 = 10;

/// An encoder has a smallest possible frame, so its output stops following a target below
/// that size. Sending fewer frames is then the only way to stay within the target.
struct FrameBudget {
    target_bits_per_second: u64,
    debt_bits: u64,
    updated: Instant,
}

impl FrameBudget {
    fn repay(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.updated);
        self.updated = now;
        let repaid = u128::from(self.target_bits_per_second) * elapsed.as_nanos() / 1_000_000_000;
        self.debt_bits = self
            .debt_bits
            .saturating_sub(u64::try_from(repaid).unwrap_or(u64::MAX));
    }
}

pub struct VideoFramePacer {
    frame_period: Duration,
    next_tick: Instant,
    tick: u64,
    frame_divisor: u8,
    budget: FrameBudget,
}

impl VideoFramePacer {
    #[must_use]
    pub fn new(frames_per_second: u16, target_bits_per_second: u64, now: Instant) -> Option<Self> {
        Some(Self {
            frame_period: frame_period(frames_per_second)?,
            next_tick: now,
            tick: 0,
            frame_divisor: 1,
            budget: FrameBudget {
                target_bits_per_second,
                debt_bits: 0,
                updated: now,
            },
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

    pub fn should_encode(&mut self, now: Instant) -> bool {
        self.budget.repay(now);
        self.tick.is_multiple_of(u64::from(self.frame_divisor))
            && self.budget.debt_bits
                <= self.budget.target_bits_per_second / FRAME_BUDGET_ALLOWANCE_DIVISOR
    }

    pub fn set_target_bitrate(&mut self, bits_per_second: u64, now: Instant) {
        self.budget.repay(now);
        self.budget.target_bits_per_second = bits_per_second;
    }

    /// Debt stops at one second of the target, which bounds the pause after a large keyframe.
    pub fn frame_encoded(&mut self, bytes: usize) {
        let bits = u64::try_from(bytes).unwrap_or(u64::MAX).saturating_mul(8);
        self.budget.debt_bits = self
            .budget
            .debt_bits
            .saturating_add(bits)
            .min(self.budget.target_bits_per_second);
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

/// Sending rate of a spread frame, in percent of the target bitrate.
const SPREAD_RATE_PERCENT: u64 = 150;
/// Longest spread of one frame. A keyframe is many times the usual size, and spreading it
/// at the usual rate would outlast its send deadline.
const LONGEST_FRAME_SPREAD: Duration = Duration::from_millis(20);
/// A timer tick can run late. Sending may catch up by this much instead of falling behind.
const SEND_CATCH_UP: Duration = Duration::from_millis(2);
/// A clean link pays no delay for spreading, so loss turns it on for this long.
const SPREAD_AFTER_LOSS: Duration = Duration::from_secs(30);

/// Spreads the datagrams of one frame over time once the link has lost data. A frame sent
/// as one burst overflows a small buffer on a narrow link, and a short radio gap swallows
/// most of it.
#[derive(Debug, Default)]
pub struct VideoSendPacer {
    spread_until: Option<Instant>,
    next_send: Option<Instant>,
    nanos_per_byte: u64,
}

impl VideoSendPacer {
    pub fn observe_loss(&mut self, now: Instant) {
        self.spread_until = Some(now + SPREAD_AFTER_LOSS);
    }

    pub fn begin_frame(&mut self, bytes: usize, target_bits_per_second: u64, now: Instant) {
        self.next_send = None;
        self.nanos_per_byte = 0;
        if bytes == 0 || self.spread_until.is_none_or(|until| now >= until) {
            return;
        }
        let rate = u128::from(target_bits_per_second.max(1)) * u128::from(SPREAD_RATE_PERCENT);
        let spread =
            (bytes as u128 * 8 * 100 * 1_000_000_000 / rate).min(LONGEST_FRAME_SPREAD.as_nanos());
        self.nanos_per_byte = u64::try_from(spread / bytes as u128).unwrap_or(u64::MAX);
    }

    #[must_use]
    pub fn ready(&self, now: Instant) -> bool {
        self.next_send.is_none_or(|next| now >= next)
    }

    pub fn sent(&mut self, bytes: usize, now: Instant) {
        if self.nanos_per_byte == 0 {
            return;
        }
        let earliest = now.checked_sub(SEND_CATCH_UP).unwrap_or(now);
        let start = self.next_send.map_or(now, |next| next.max(earliest));
        let wait = self.nanos_per_byte.saturating_mul(bytes as u64);
        self.next_send = Some(start + Duration::from_nanos(wait));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send_duration(pacer: &mut VideoSendPacer, datagrams: usize, start: Instant) -> Duration {
        let mut now = start;
        for _ in 0..datagrams {
            while !pacer.ready(now) {
                now += Duration::from_micros(100);
            }
            pacer.sent(1200, now);
        }
        now - start
    }

    #[test]
    fn a_clean_link_sends_a_frame_at_once() {
        let now = Instant::now();
        let mut pacer = VideoSendPacer::default();
        pacer.begin_frame(24_000, 5_000_000, now);
        assert_eq!(send_duration(&mut pacer, 20, now), Duration::ZERO);
    }

    #[test]
    fn after_loss_a_frame_leaves_at_one_and_a_half_times_the_target() {
        let now = Instant::now();
        let mut pacer = VideoSendPacer::default();
        pacer.observe_loss(now);
        pacer.begin_frame(12_000, 8_000_000, now);
        let spread = send_duration(&mut pacer, 10, now);
        assert!(spread >= Duration::from_millis(7) && spread <= Duration::from_millis(8));
    }

    #[test]
    fn a_keyframe_stays_within_the_longest_spread() {
        let now = Instant::now();
        let mut pacer = VideoSendPacer::default();
        pacer.observe_loss(now);
        pacer.begin_frame(240_000, 2_000_000, now);
        assert!(send_duration(&mut pacer, 200, now) <= LONGEST_FRAME_SPREAD);
    }

    #[test]
    fn spreading_stops_once_the_link_stays_clean() {
        let now = Instant::now();
        let mut pacer = VideoSendPacer::default();
        pacer.observe_loss(now);
        let later = now + SPREAD_AFTER_LOSS;
        pacer.begin_frame(24_000, 5_000_000, later);
        assert_eq!(send_duration(&mut pacer, 20, later), Duration::ZERO);
    }

    #[test]
    fn small_overrun_rebases_without_skipping_an_extra_slot() {
        let started = Instant::now();
        let mut pacer = VideoFramePacer::new(100, 50_000_000, started).unwrap();
        pacer.complete_tick(started, started + Duration::from_millis(11));
        assert_eq!(pacer.wait_duration(started), Duration::from_millis(11));
    }

    #[test]
    fn large_stall_does_not_create_catch_up_bursts() {
        let started = Instant::now();
        let mut pacer = VideoFramePacer::new(100, 50_000_000, started).unwrap();
        let resumed = started + Duration::from_millis(60);
        pacer.complete_tick(started, resumed);
        assert_eq!(pacer.wait_duration(resumed), Duration::ZERO);
        pacer.complete_tick(resumed, resumed + Duration::from_millis(2));
        assert_eq!(pacer.wait_duration(resumed), Duration::from_millis(10));
    }

    #[test]
    fn divisor_selects_frames_without_changing_the_capture_clock() {
        let started = Instant::now();
        let mut pacer = VideoFramePacer::new(120, 50_000_000, started).unwrap();
        pacer.set_frame_divisor(2).unwrap();
        assert!(pacer.should_encode(started));
        pacer.complete_tick(started, started);
        assert!(!pacer.should_encode(started));
        pacer.complete_tick(started, started);
        assert!(pacer.should_encode(started));
        assert!(pacer.set_frame_divisor(3).is_err());
    }

    /// Runs one second at 120 ticks and returns the frames the pacer allowed.
    fn frames_in_one_second(pacer: &mut VideoFramePacer, started: Instant, bytes: usize) -> u32 {
        let mut frames = 0;
        for tick in 1..=120 {
            let now = started + Duration::from_nanos(tick * 8_333_333);
            if pacer.should_encode(now) {
                pacer.frame_encoded(bytes);
                frames += 1;
            }
            pacer.complete_tick(now, now);
        }
        frames
    }

    #[test]
    fn an_encoder_within_its_target_never_loses_a_frame() {
        let started = Instant::now();
        let mut pacer = VideoFramePacer::new(120, 45_000_000, started).unwrap();
        assert_eq!(frames_in_one_second(&mut pacer, started, 44_000), 120);
    }

    #[test]
    fn a_keyframe_at_a_normal_bitrate_does_not_lose_a_frame() {
        let started = Instant::now();
        let mut pacer = VideoFramePacer::new(120, 45_000_000, started).unwrap();
        pacer.frame_encoded(400_000);
        assert_eq!(frames_in_one_second(&mut pacer, started, 44_000), 120);
    }

    #[test]
    fn frames_above_the_target_thin_out_to_the_target_rate() {
        let started = Instant::now();
        let mut pacer = VideoFramePacer::new(120, 4_000_000, started).unwrap();
        let frames = frames_in_one_second(&mut pacer, started, 18_000);
        assert!((27..=31).contains(&frames), "{frames} frames");
    }

    #[test]
    fn a_huge_frame_pauses_the_stream_for_at_most_one_second() {
        let started = Instant::now();
        let mut pacer = VideoFramePacer::new(120, 1_000_000, started).unwrap();
        pacer.frame_encoded(10_000_000);
        assert!(!pacer.should_encode(started));
        assert!(pacer.should_encode(started + Duration::from_secs(1)));
    }
}
