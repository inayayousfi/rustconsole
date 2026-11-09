//! Decides what one host video stream sends next. The platform loop reads encoded frames, calls
//! the transport, and records diagnostics; this type owns every rule between those steps.

use crate::video_resend::RecentVideoChunks;
use crate::video_stream::{HostEncodedVideoFrame, HostVideoStreamPolicy};
use crate::{BitrateChange, VideoBitrateChangeCause, VideoDeliveryReport, VideoPathReport};
use rustconsole_protocol::latency::{LatencyBudgetFailure, LatencyControlStatus};
use rustconsole_protocol::wire::VideoReceiverReport;
use rustconsole_session::video_datagram::{
    MissingVideoChunks, VideoDatagramError, assembly_deadline,
};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutgoingKind {
    /// A chunk the player asked for again, or the announcement of the newest frame.
    Repair,
    /// A chunk of the frame being sent.
    Frame,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Outgoing<T> {
    pub datagram: T,
    pub kind: OutgoingKind,
}

pub struct VideoSender<T> {
    policy: HostVideoStreamPolicy,
    datagram_version: u32,
    resend: bool,
    frame_period: Duration,
    frame: VecDeque<T>,
    frame_started: Instant,
    repairs: VecDeque<T>,
    recent: RecentVideoChunks<T>,
    bitrate_change_sequence: u64,
    bitrate_change_cause: VideoBitrateChangeCause,
}

impl<T: Clone + AsRef<[u8]> + From<Vec<u8>>> VideoSender<T> {
    /// `resend` tells whether both ends negotiated chunk repair.
    #[must_use]
    pub fn new(
        policy: HostVideoStreamPolicy,
        frame_period: Duration,
        datagram_version: u32,
        resend: bool,
        now: Instant,
    ) -> Self {
        Self {
            policy,
            datagram_version,
            resend,
            frame_period,
            frame: VecDeque::new(),
            frame_started: now,
            repairs: VecDeque::new(),
            recent: RecentVideoChunks::new(now),
            bitrate_change_sequence: 0,
            bitrate_change_cause: VideoBitrateChangeCause::Startup,
        }
    }

    #[must_use]
    pub const fn policy(&self) -> &HostVideoStreamPolicy {
        &self.policy
    }

    /// Counts bitrate changes so diagnostics can tell one change from the next.
    #[must_use]
    pub const fn bitrate_change_sequence(&self) -> u64 {
        self.bitrate_change_sequence
    }

    #[must_use]
    pub const fn bitrate_change_cause(&self) -> VideoBitrateChangeCause {
        self.bitrate_change_cause
    }

    #[must_use]
    pub fn latency_status(&self) -> Option<LatencyControlStatus> {
        self.policy.latency_status()
    }

    pub fn require_keyframe(&mut self) {
        self.policy.require_keyframe();
    }

    /// Tells whether the encoder should be asked for a keyframe now.
    pub fn keyframe_request_due(&mut self, now: Instant) -> bool {
        self.policy.keyframe_request_due(now)
    }

    /// The encoder worker discarded an event, so the player is missing a frame.
    pub fn observe_worker_queue_drop(&mut self) {
        self.policy.observe_worker_queue_drop();
    }

    pub fn observe_resend_request(&mut self, request: &MissingVideoChunks, now: Instant) {
        self.repairs.extend(self.recent.take(
            request.sequence,
            &request.chunk_indexes,
            self.policy.target_bits_per_second(),
            now,
        ));
    }

    /// Applies one player report. The returned change is for the encoder.
    pub fn observe_receiver_report(
        &mut self,
        path: VideoPathReport,
        report: VideoReceiverReport,
        now: Instant,
    ) -> Result<Option<BitrateChange>, LatencyBudgetFailure> {
        let change = self.policy.observe_receiver_with_latency(
            path,
            VideoDeliveryReport {
                received_chunks: report.received_chunks,
                completed_payload_bytes: report.completed_payload_bytes,
                measurement_interval_micros: report.measurement_interval_micros,
                lost_chunks: report.lost_chunks,
                late_chunks: report.late_chunks,
                assembly_overflows: report.assembly_overflows,
                incomplete_frames: report.incomplete_frames,
            },
            report.latency,
            now,
        )?;
        Ok(self.record(change))
    }

    /// Tells whether the sender is ready for the next encoded frame.
    #[must_use]
    pub fn wants_frame(&self) -> bool {
        self.frame.is_empty()
    }

    /// Abandons the frame being sent when it can no longer arrive in time. The returned change is
    /// for the encoder. `true` reports that a frame was abandoned.
    pub fn expire_late_frame(
        &mut self,
        round_trip_time: Duration,
        now: Instant,
    ) -> (bool, Option<BitrateChange>) {
        let deadline = assembly_deadline(self.frame_period, round_trip_time);
        if self.frame.is_empty() || now.saturating_duration_since(self.frame_started) < deadline {
            return (false, None);
        }
        self.frame.clear();
        let change = self.policy.observe_send_deadline_expired();
        (true, self.record(change))
    }

    /// Tells whether an encoded frame can be sent. A frame that depends on one the player never
    /// received is refused until a keyframe arrives.
    pub fn accept_frame(&mut self, sequence: u64, keyframe: bool) -> bool {
        self.policy.accept_encoded_frame(sequence, keyframe)
    }

    /// Splits an accepted frame into chunks and starts sending it.
    pub fn queue_frame(
        &mut self,
        frame: HostEncodedVideoFrame,
        maximum_datagram_size: usize,
        now: Instant,
    ) -> Result<(), VideoDatagramError> {
        let sequence = frame.sequence;
        let chunks: Vec<T> = self
            .policy
            .packetize_encoded_frame(frame, maximum_datagram_size, self.datagram_version)?
            .into_iter()
            .map(T::from)
            .collect();
        self.recent.remember(sequence, chunks.clone(), now);
        self.frame = chunks.into();
        self.frame_started = now;
        Ok(())
    }

    /// Returns the next datagram to send. Repairs go first, because the player holds newer frames
    /// until they arrive.
    pub fn next(&mut self, now: Instant) -> Option<Outgoing<T>> {
        if self.resend {
            self.repairs.extend(self.recent.take_announcement(now));
        }
        if let Some(datagram) = self.repairs.front() {
            return Some(Outgoing {
                datagram: datagram.clone(),
                kind: OutgoingKind::Repair,
            });
        }
        self.frame.front().map(|datagram| Outgoing {
            datagram: datagram.clone(),
            kind: OutgoingKind::Frame,
        })
    }

    /// Records that the datagram `next` returned has left. `true` reports that it was the last
    /// chunk of its frame.
    pub fn sent(&mut self, kind: OutgoingKind) -> bool {
        match kind {
            OutgoingKind::Repair => {
                self.repairs.pop_front();
                false
            }
            OutgoingKind::Frame => {
                self.frame.pop_front();
                self.frame.is_empty()
            }
        }
    }

    /// Records that the transport refused the datagram as too large for the path, which drops
    /// the rest of its kind.
    pub fn refused_as_too_large(&mut self, kind: OutgoingKind) {
        match kind {
            OutgoingKind::Repair => self.repairs.clear(),
            OutgoingKind::Frame => {
                self.frame.clear();
                self.policy.require_keyframe();
            }
        }
    }

    fn record(&mut self, change: Option<BitrateChange>) -> Option<BitrateChange> {
        if let Some(change) = change {
            self.bitrate_change_sequence = self.bitrate_change_sequence.saturating_add(1);
            self.bitrate_change_cause = change.cause;
        }
        change
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustconsole_session::video_datagram::VIDEO_DATAGRAM_VERSION;

    const PERIOD: Duration = Duration::from_nanos(1_000_000_000 / 120);
    const RTT: Duration = Duration::from_millis(10);

    fn sender(now: Instant) -> VideoSender<Vec<u8>> {
        VideoSender::new(
            HostVideoStreamPolicy::new(20_000_000),
            PERIOD,
            VIDEO_DATAGRAM_VERSION,
            true,
            now,
        )
    }

    fn frame(sequence: u64, keyframe: bool) -> HostEncodedVideoFrame {
        HostEncodedVideoFrame {
            sequence,
            captured_at_micros: 0,
            encoded_at_micros: 0,
            packetized_at_micros: 0,
            input_sequence: 0,
            keyframe,
            payload: vec![7; 3_000],
        }
    }

    fn send_all(sender: &mut VideoSender<Vec<u8>>, now: Instant) -> Vec<OutgoingKind> {
        let mut kinds = Vec::new();
        while let Some(outgoing) = sender.next(now) {
            kinds.push(outgoing.kind);
            sender.sent(outgoing.kind);
        }
        kinds
    }

    #[test]
    fn a_frame_is_sent_chunk_by_chunk_and_reports_its_last_chunk() {
        let now = Instant::now();
        let mut sender = sender(now);
        assert!(sender.accept_frame(0, true));
        sender.queue_frame(frame(0, true), 1_200, now).unwrap();
        assert!(!sender.wants_frame());

        let mut finished = Vec::new();
        while let Some(outgoing) = sender.next(now) {
            assert_eq!(outgoing.kind, OutgoingKind::Frame);
            finished.push(sender.sent(outgoing.kind));
        }
        assert_eq!(finished, [false, false, true]);
        assert!(sender.wants_frame());
    }

    #[test]
    fn requested_chunks_go_out_before_the_rest_of_the_current_frame() {
        let now = Instant::now();
        let mut sender = sender(now);
        sender.accept_frame(0, true);
        sender.queue_frame(frame(0, true), 1_200, now).unwrap();
        send_all(&mut sender, now);
        sender.accept_frame(1, false);
        sender.queue_frame(frame(1, false), 1_200, now).unwrap();

        let later = now + Duration::from_millis(5);
        sender.observe_resend_request(
            &MissingVideoChunks {
                sequence: 0,
                chunk_indexes: vec![1],
            },
            later,
        );
        let kinds = send_all(&mut sender, later);
        assert_eq!(kinds[0], OutgoingKind::Repair);
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == OutgoingKind::Frame)
                .count(),
            3
        );
    }

    #[test]
    fn a_frame_too_late_to_arrive_is_abandoned_and_needs_a_keyframe() {
        let now = Instant::now();
        let mut sender = sender(now);
        sender.accept_frame(0, true);
        sender.queue_frame(frame(0, true), 1_200, now).unwrap();

        assert!(!sender.expire_late_frame(RTT, now).0);
        let late = now + Duration::from_millis(200);
        let (abandoned, _) = sender.expire_late_frame(RTT, late);

        assert!(abandoned);
        assert!(sender.wants_frame());
        assert!(!sender.accept_frame(1, false));
        assert!(sender.accept_frame(2, true));
    }

    #[test]
    fn the_newest_frame_is_announced_again_when_nothing_follows_it() {
        let now = Instant::now();
        let mut sender = sender(now);
        sender.accept_frame(0, true);
        sender.queue_frame(frame(0, true), 1_200, now).unwrap();
        send_all(&mut sender, now);
        assert_eq!(sender.next(now), None);

        let later = now + Duration::from_millis(30);
        assert_eq!(
            sender.next(later).map(|outgoing| outgoing.kind),
            Some(OutgoingKind::Repair)
        );
    }
}
