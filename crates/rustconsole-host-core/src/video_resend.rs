use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Frames kept for repair. A player gives up on a frame well before this many frames pass.
const RECENT_FRAMES: usize = 16;
/// Resends may use this share of the target, so repairs cannot flood a link that is already full.
const RESEND_TARGET_DIVISOR: u64 = 4;
/// A low target still needs room to repair one frame, because the encoder has a smallest frame.
const MINIMUM_RESEND_BITS_PER_SECOND: u64 = 2_000_000;
/// Credit gathers for this share of a second, enough to repair one short outage at once.
const RESEND_BURST_DIVISOR: u64 = 4;

/// A player cannot ask for a frame it never heard of, and at low frame rates the next frame is far
/// away. A copy of the last chunk after this delay, which outlasts a short outage, announces the
/// frame. A newer frame announces it sooner, so the copy is only sent at low frame rates.
const FRAME_ANNOUNCEMENT_DELAY: Duration = Duration::from_millis(20);

/// Recently sent video datagrams, kept so that a player can ask for lost ones again.
pub struct RecentVideoChunks<T> {
    frames: VecDeque<(u64, Vec<T>)>,
    credit_bits: u64,
    updated: Instant,
    announcement: Option<(Instant, T)>,
}

impl<T: Clone + AsRef<[u8]>> RecentVideoChunks<T> {
    #[must_use]
    pub const fn new(now: Instant) -> Self {
        Self {
            frames: VecDeque::new(),
            credit_bits: 0,
            updated: now,
            announcement: None,
        }
    }

    pub fn remember(&mut self, sequence: u64, chunks: Vec<T>, now: Instant) {
        self.announcement = chunks
            .last()
            .map(|chunk| (now + FRAME_ANNOUNCEMENT_DELAY, chunk.clone()));
        if self.frames.len() == RECENT_FRAMES {
            self.frames.pop_front();
        }
        self.frames.push_back((sequence, chunks));
    }

    /// Returns the copy of the newest frame's last chunk once it is due.
    pub fn take_announcement(&mut self, now: Instant) -> Option<T> {
        if self.announcement.as_ref()?.0 > now {
            return None;
        }
        self.announcement.take().map(|(_, chunk)| chunk)
    }

    /// Returns the requested chunks that are still held and fit the resend allowance. No indexes
    /// means the whole frame.
    pub fn take(
        &mut self,
        sequence: u64,
        chunk_indexes: &[u32],
        target_bits_per_second: u64,
        now: Instant,
    ) -> Vec<T> {
        let rate =
            (target_bits_per_second / RESEND_TARGET_DIVISOR).max(MINIMUM_RESEND_BITS_PER_SECOND);
        let elapsed = now.saturating_duration_since(self.updated);
        self.updated = now;
        let earned = u128::from(rate) * elapsed.as_nanos() / 1_000_000_000;
        self.credit_bits = self
            .credit_bits
            .saturating_add(u64::try_from(earned).unwrap_or(u64::MAX))
            .min(rate / RESEND_BURST_DIVISOR);

        let Some((_, chunks)) = self.frames.iter().find(|(held, _)| *held == sequence) else {
            return Vec::new();
        };
        let requested: Vec<&T> = if chunk_indexes.is_empty() {
            chunks.iter().collect()
        } else {
            chunk_indexes
                .iter()
                .filter_map(|index| chunks.get(usize::try_from(*index).ok()?))
                .collect()
        };
        let mut allowed = Vec::with_capacity(requested.len());
        for chunk in requested {
            let bits = u64::try_from(chunk.as_ref().len())
                .unwrap_or(u64::MAX)
                .saturating_mul(8);
            if bits > self.credit_bits {
                break;
            }
            self.credit_bits -= bits;
            allowed.push(chunk.clone());
        }
        allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const TARGET: u64 = 40_000_000;

    fn store(now: Instant) -> RecentVideoChunks<Vec<u8>> {
        let mut store = RecentVideoChunks::new(now);
        store.remember(5, vec![vec![1; 1000], vec![2; 1000], vec![3; 1000]], now);
        store
    }

    #[test]
    fn requested_chunks_come_back_and_an_empty_request_means_the_whole_frame() {
        let now = Instant::now();
        let mut store = store(now);
        let later = now + Duration::from_millis(100);

        assert_eq!(store.take(5, &[1], TARGET, later), [vec![2; 1000]]);
        assert_eq!(store.take(5, &[], TARGET, later).len(), 3);
    }

    #[test]
    fn unknown_frames_and_chunks_are_ignored() {
        let now = Instant::now();
        let mut store = store(now);
        let later = now + Duration::from_millis(100);

        assert!(store.take(9, &[0], TARGET, later).is_empty());
        assert!(store.take(5, &[7], TARGET, later).is_empty());
    }

    #[test]
    fn a_slow_frame_is_announced_once_after_the_delay() {
        let now = Instant::now();
        let mut store = store(now);

        assert_eq!(store.take_announcement(now), None);
        let due = now + FRAME_ANNOUNCEMENT_DELAY;
        assert_eq!(store.take_announcement(due), Some(vec![3; 1000]));
        assert_eq!(store.take_announcement(due), None);
    }

    #[test]
    fn a_newer_frame_replaces_the_announcement_before_it_is_due() {
        let now = Instant::now();
        let mut store = store(now);
        let next = now + Duration::from_millis(8);
        store.remember(6, vec![vec![9; 1000]], next);

        assert_eq!(
            store.take_announcement(now + FRAME_ANNOUNCEMENT_DELAY),
            None
        );
        assert_eq!(
            store.take_announcement(next + FRAME_ANNOUNCEMENT_DELAY),
            Some(vec![9; 1000])
        );
    }

    #[test]
    fn old_frames_are_forgotten() {
        let now = Instant::now();
        let mut store = store(now);
        for sequence in 6..6 + RECENT_FRAMES as u64 {
            store.remember(sequence, vec![vec![0; 1000]], now);
        }

        assert!(
            store
                .take(5, &[0], TARGET, now + Duration::from_secs(1))
                .is_empty()
        );
    }

    #[test]
    fn resends_stay_within_a_quarter_of_the_target() {
        let now = Instant::now();
        let mut store = RecentVideoChunks::new(now);
        store.remember(1, vec![vec![0_u8; 1000]; 2000], now);

        let mut sent_bits = 0;
        for millisecond in 1..=1000 {
            let at = now + Duration::from_millis(millisecond);
            sent_bits += store.take(1, &[], TARGET, at).len() as u64 * 8000;
        }

        assert!(sent_bits <= TARGET / 4, "sent {sent_bits}");
        assert!(sent_bits >= TARGET / 5, "sent {sent_bits}");
    }
}
