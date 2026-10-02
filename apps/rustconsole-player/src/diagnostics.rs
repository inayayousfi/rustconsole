use rustconsole_player_core::ClockOffsetEstimate;
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TRACE_QUEUE_CAPACITY: usize = 4_096;
const TRACE_SCHEMA_VERSION: u32 = 2;
const TRACE_CHUNK_MICROS: u64 = 60_000_000;
const TRACE_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_ANOMALY_ARTIFACTS: u64 = 3;
const MAX_ANOMALY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DISTRIBUTION_SAMPLES: usize = 65_536;

#[derive(Default)]
struct BoundedSamples {
    observed: u64,
    discarded: u64,
    values: Vec<u64>,
}

impl BoundedSamples {
    fn observe(&mut self, value: u64) {
        self.observed = self.observed.saturating_add(1);
        if self.values.len() < MAX_DISTRIBUTION_SAMPLES {
            self.values.push(value);
        } else {
            self.discarded = self.discarded.saturating_add(1);
        }
    }

    fn sorted(&self) -> Vec<u64> {
        let mut values = self.values.clone();
        values.sort_unstable();
        values
    }
}

#[derive(Serialize)]
struct TraceEvent {
    record_type: &'static str,
    schema_version: u32,
    elapsed_micros: u64,
    metric: &'static str,
    value: u64,
    unit: &'static str,
    media_sequence: Option<u64>,
    input_sequence: Option<u64>,
    endpoint: &'static str,
    classification: &'static str,
    includes: Option<&'static str>,
}

#[derive(Clone, Copy)]
pub struct VideoFrameTrace {
    pub sequence: u64,
    pub keyframe: bool,
    pub encoded_frame_bytes: u64,
    pub target_bitrate_bits_per_second: u64,
    pub delivered_goodput_bits_per_second: u64,
    pub soft_ceiling_bits_per_second: Option<u64>,
    pub round_trip_time_micros: u64,
    pub received_chunks: u64,
    pub lost_chunks: u64,
    pub late_chunks: u64,
    pub assembly_overflows: u64,
    pub completed_frames: u64,
    pub incomplete_frames: u64,
}

#[derive(Clone, Copy)]
pub struct HostBitrateTrace {
    pub media_sequence: u64,
    pub change_sequence: u64,
    pub cause: u64,
    pub target_bits_per_second: u64,
    pub delivered_goodput_bits_per_second: u64,
    pub soft_ceiling_bits_per_second: u64,
    pub path_round_trip_micros: u64,
    pub path_congestion_window_bytes: u64,
    pub path_lost_packets: u64,
}

#[derive(Serialize)]
struct VideoFrameTraceRecord {
    record_type: &'static str,
    schema_version: u32,
    elapsed_micros: u64,
    sequence: u64,
    keyframe: bool,
    encoded_frame_bytes: u64,
    target_bitrate_bits_per_second: u64,
    delivered_goodput_bits_per_second: u64,
    soft_ceiling_bits_per_second: Option<u64>,
    round_trip_time_micros: u64,
    received_chunks: u64,
    lost_chunks: u64,
    late_chunks: u64,
    assembly_overflows: u64,
    completed_frames: u64,
    incomplete_frames: u64,
}

#[derive(Serialize)]
struct CounterTraceRecord {
    record_type: &'static str,
    schema_version: u32,
    elapsed_micros: u64,
    name: &'static str,
    value: u64,
}

#[derive(Serialize)]
struct BitrateChangeTraceRecord {
    record_type: &'static str,
    schema_version: u32,
    elapsed_micros: u64,
    sequence: u64,
    previous_target_bits_per_second: Option<u64>,
    target_bits_per_second: u64,
    reason: &'static str,
}

#[derive(Serialize)]
struct HostBitrateTraceRecord {
    record_type: &'static str,
    schema_version: u32,
    elapsed_micros: u64,
    media_sequence: u64,
    change_sequence: u64,
    cause: &'static str,
    cause_code: u64,
    target_bits_per_second: u64,
    delivered_goodput_bits_per_second: u64,
    soft_ceiling_bits_per_second: u64,
    path_round_trip_micros: u64,
    path_congestion_window_bytes: u64,
    path_lost_packets: u64,
}

enum WorkerMessage {
    Observation(TraceEvent),
    Counter {
        elapsed_micros: u64,
        name: &'static str,
        value: u64,
    },
    IncrementCounter {
        elapsed_micros: u64,
        name: &'static str,
    },
    AddCounter {
        elapsed_micros: u64,
        name: &'static str,
        value: u64,
    },
    VideoFrame(VideoFrameTraceRecord),
    HostBitrate(HostBitrateTraceRecord),
    ClockOffset(ClockOffsetEstimate),
    Anomaly {
        elapsed_micros: u64,
        kind: &'static str,
        media_sequence: Option<u64>,
        values: Vec<(&'static str, u64)>,
    },
}

#[derive(Serialize)]
struct MetricSummary {
    samples: u64,
    discarded_samples: u64,
    unit: &'static str,
    p50_micros: Option<u64>,
    p90_micros: Option<u64>,
    p95_micros: Option<u64>,
    p99_micros: Option<u64>,
    worst_micros: Option<u64>,
}

#[derive(Default)]
struct MetricAccumulator {
    samples: BoundedSamples,
    unit: Option<&'static str>,
    larger_is_better: bool,
}

#[derive(Serialize)]
struct ClockSummary {
    offset_micros: i64,
    uncertainty_micros: u64,
    method: &'static str,
}

#[derive(Serialize)]
struct DiagnosticSummary {
    session_id: String,
    generated_at_unix_micros: u128,
    duration_micros: u64,
    mode: &'static str,
    trace_queue_capacity: usize,
    trace_queue_rejected_events: u64,
    trace_serialization_failures: u64,
    trace_write_failures: u64,
    trace_byte_cap_discarded_events: u64,
    trace_sampling_discarded_events: u64,
    trace_bytes: u64,
    trace_byte_limit: u64,
    trace_manifest: String,
    trace_chunks: Vec<TraceChunkSummary>,
    aggregation_worker_processing_micros: u64,
    aggregation_worker_events: u64,
    aggregation_worker_worst_event_micros: u64,
    clock: Option<ClockSummary>,
    metrics: BTreeMap<&'static str, MetricSummary>,
    counters: BTreeMap<&'static str, u64>,
    anomaly_artifacts: u64,
    anomaly_artifact_bytes: u64,
    anomaly_artifacts_discarded_by_limit: u64,
    anomaly_artifacts_deduplicated: u64,
    anomaly_serialization_failures: u64,
    anomaly_write_failures: u64,
    unavailable_boundaries: &'static [&'static str],
    notes: &'static [&'static str],
}

#[derive(Clone, Serialize)]
struct TraceChunkSummary {
    index: u64,
    path: String,
    start_elapsed_micros: u64,
    end_elapsed_micros: u64,
    records: u64,
    bytes: u64,
}

#[derive(Serialize)]
struct TraceManifest {
    schema_version: u32,
    session_id: String,
    chunk_duration_micros: u64,
    chunks: Vec<TraceChunkSummary>,
}

struct WorkerOutput {
    summary_path: PathBuf,
}

struct WorkerState {
    session_id: String,
    started: Instant,
    artifact_stem: PathBuf,
    summary_path: PathBuf,
    trace: Option<BufWriter<File>>,
    trace_chunk: Option<TraceChunkSummary>,
    trace_chunks: Vec<TraceChunkSummary>,
    trace_manifest_path: PathBuf,
    trace_last_flushed: Instant,
    trace_bytes: u64,
    trace_serialization_failures: u64,
    trace_write_failures: u64,
    trace_byte_cap_discarded_events: u64,
    trace_sampling_discarded_events: u64,
    trace_metric_last_elapsed: BTreeMap<&'static str, u64>,
    last_video_target_bits_per_second: Option<u64>,
    last_video_sequence: Option<u64>,
    last_host_bitrate_change_sequence: Option<u64>,
    metrics: BTreeMap<&'static str, MetricAccumulator>,
    counters: BTreeMap<&'static str, u64>,
    clock_offset: Option<ClockOffsetEstimate>,
    artifact_count: u64,
    artifact_bytes: u64,
    artifact_discarded_by_limit: u64,
    artifact_deduplicated: u64,
    artifact_serialization_failures: u64,
    artifact_write_failures: u64,
    artifact_kinds: HashSet<&'static str>,
    processing_micros: u64,
    processed_events: u64,
    worst_processing_micros: u64,
    overlay: Arc<Mutex<String>>,
    overlay_updated_at: Instant,
}

pub struct LatencyDiagnostics {
    started: Instant,
    sender: Option<mpsc::SyncSender<WorkerMessage>>,
    queue_rejected_events: Arc<AtomicU64>,
    worker: Option<thread::JoinHandle<io::Result<WorkerOutput>>>,
    overlay: Arc<Mutex<String>>,
}

impl LatencyDiagnostics {
    pub fn disabled() -> Self {
        Self {
            started: Instant::now(),
            sender: None,
            queue_rejected_events: Arc::new(AtomicU64::new(0)),
            worker: None,
            overlay: Arc::new(Mutex::new(String::new())),
        }
    }

    pub fn open(enabled: bool) -> io::Result<Self> {
        if !enabled {
            return Ok(Self::disabled());
        }
        let directory = diagnostic_directory();
        let session_id = format!(
            "{}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_millis(),
            std::process::id()
        );
        let started = Instant::now();
        let overlay = Arc::new(Mutex::new(String::new()));
        let worker_overlay = Arc::clone(&overlay);
        let queue_rejected_events = Arc::new(AtomicU64::new(0));
        let worker_queue_rejected_events = Arc::clone(&queue_rejected_events);
        let (sender, receiver) = mpsc::sync_channel(TRACE_QUEUE_CAPACITY);
        let worker = thread::Builder::new()
            .name("diagnostic-writer".into())
            .spawn(move || {
                run_worker(
                    directory,
                    session_id,
                    started,
                    worker_overlay,
                    worker_queue_rejected_events,
                    receiver,
                )
            })?;
        Ok(Self {
            started,
            sender: Some(sender),
            queue_rejected_events,
            worker: Some(worker),
            overlay,
        })
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.sender.is_some()
    }

    pub fn set_clock_offset(&mut self, estimate: ClockOffsetEstimate) {
        self.submit(WorkerMessage::ClockOffset(estimate));
    }

    pub fn observe(
        &mut self,
        metric: &'static str,
        micros: u64,
        media_sequence: Option<u64>,
        input_sequence: Option<u64>,
        endpoint: &'static str,
    ) {
        self.observe_classified(
            metric,
            micros,
            media_sequence,
            input_sequence,
            endpoint,
            "direct",
            None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn observe_classified(
        &mut self,
        metric: &'static str,
        micros: u64,
        media_sequence: Option<u64>,
        input_sequence: Option<u64>,
        endpoint: &'static str,
        classification: &'static str,
        includes: Option<&'static str>,
    ) {
        self.submit(WorkerMessage::Observation(TraceEvent {
            record_type: "observation",
            schema_version: TRACE_SCHEMA_VERSION,
            elapsed_micros: elapsed_micros(self.started),
            metric,
            value: micros,
            unit: "microseconds",
            media_sequence,
            input_sequence,
            endpoint,
            classification,
            includes,
        }));
    }

    pub fn observe_measurement(
        &mut self,
        metric: &'static str,
        value: u64,
        unit: &'static str,
        media_sequence: Option<u64>,
    ) {
        self.observe_measurement_classified(metric, value, unit, media_sequence, "direct", None);
    }

    pub fn observe_measurement_classified(
        &mut self,
        metric: &'static str,
        value: u64,
        unit: &'static str,
        media_sequence: Option<u64>,
        classification: &'static str,
        includes: Option<&'static str>,
    ) {
        self.submit(WorkerMessage::Observation(TraceEvent {
            record_type: "observation",
            schema_version: TRACE_SCHEMA_VERSION,
            elapsed_micros: elapsed_micros(self.started),
            metric,
            value,
            unit,
            media_sequence,
            input_sequence: None,
            endpoint: "measurement recorded",
            classification,
            includes,
        }));
    }

    pub fn counter(&mut self, name: &'static str, value: u64) {
        self.submit(WorkerMessage::Counter {
            elapsed_micros: elapsed_micros(self.started),
            name,
            value,
        });
    }

    pub fn video_frame(&mut self, sample: VideoFrameTrace) {
        self.submit(WorkerMessage::VideoFrame(VideoFrameTraceRecord {
            record_type: "video_frame",
            schema_version: TRACE_SCHEMA_VERSION,
            elapsed_micros: elapsed_micros(self.started),
            sequence: sample.sequence,
            keyframe: sample.keyframe,
            encoded_frame_bytes: sample.encoded_frame_bytes,
            target_bitrate_bits_per_second: sample.target_bitrate_bits_per_second,
            delivered_goodput_bits_per_second: sample.delivered_goodput_bits_per_second,
            soft_ceiling_bits_per_second: sample.soft_ceiling_bits_per_second,
            round_trip_time_micros: sample.round_trip_time_micros,
            received_chunks: sample.received_chunks,
            lost_chunks: sample.lost_chunks,
            late_chunks: sample.late_chunks,
            assembly_overflows: sample.assembly_overflows,
            completed_frames: sample.completed_frames,
            incomplete_frames: sample.incomplete_frames,
        }));
    }

    pub fn host_bitrate(&mut self, sample: HostBitrateTrace) {
        self.submit(WorkerMessage::HostBitrate(HostBitrateTraceRecord {
            record_type: "host_bitrate_change",
            schema_version: TRACE_SCHEMA_VERSION,
            elapsed_micros: elapsed_micros(self.started),
            media_sequence: sample.media_sequence,
            change_sequence: sample.change_sequence,
            cause: bitrate_change_cause_name(sample.cause),
            cause_code: sample.cause,
            target_bits_per_second: sample.target_bits_per_second,
            delivered_goodput_bits_per_second: sample.delivered_goodput_bits_per_second,
            soft_ceiling_bits_per_second: sample.soft_ceiling_bits_per_second,
            path_round_trip_micros: sample.path_round_trip_micros,
            path_congestion_window_bytes: sample.path_congestion_window_bytes,
            path_lost_packets: sample.path_lost_packets,
        }));
    }

    pub fn increment_counter(&mut self, name: &'static str) {
        self.submit(WorkerMessage::IncrementCounter {
            elapsed_micros: elapsed_micros(self.started),
            name,
        });
    }

    pub fn observe_full_frame_copy(&mut self, duration_micros: u64, bytes: u64, sequence: u64) {
        self.observe(
            "player_full_frame_copy",
            duration_micros,
            Some(sequence),
            None,
            "diagnostic luma readback complete",
        );
        self.increment_counter("player_full_frame_copy_count");
        self.submit(WorkerMessage::AddCounter {
            elapsed_micros: elapsed_micros(self.started),
            name: "player_full_frame_copy_bytes",
            value: bytes,
        });
        self.counter("player_full_frame_copy_last_bytes", bytes);
    }

    pub fn record_anomaly(
        &mut self,
        kind: &'static str,
        media_sequence: Option<u64>,
        values: &[(&'static str, u64)],
    ) {
        self.submit(WorkerMessage::Anomaly {
            elapsed_micros: elapsed_micros(self.started),
            kind,
            media_sequence,
            values: values.to_vec(),
        });
    }

    #[must_use]
    pub fn overlay_text(&self) -> String {
        self.overlay
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn finish(mut self) -> io::Result<Option<PathBuf>> {
        self.sender.take();
        let Some(worker) = self.worker.take() else {
            return Ok(None);
        };
        let output = worker
            .join()
            .map_err(|_| io::Error::other("diagnostic writer panicked"))??;
        Ok(Some(output.summary_path))
    }

    fn submit(&mut self, message: WorkerMessage) {
        let Some(sender) = &self.sender else {
            return;
        };
        if sender.try_send(message).is_err() {
            self.queue_rejected_events.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn run_worker(
    directory: PathBuf,
    session_id: String,
    started: Instant,
    overlay: Arc<Mutex<String>>,
    queue_rejected_events: Arc<AtomicU64>,
    receiver: mpsc::Receiver<WorkerMessage>,
) -> io::Result<WorkerOutput> {
    fs::create_dir_all(&directory)?;
    let artifact_stem = directory.join(format!("session-{session_id}"));
    let summary_path = artifact_stem.with_extension("json");
    let trace_manifest_path =
        PathBuf::from(format!("{}-trace-manifest.json", artifact_stem.display()));
    let mut state = WorkerState {
        session_id,
        started,
        artifact_stem,
        summary_path: summary_path.clone(),
        trace: None,
        trace_chunk: None,
        trace_chunks: Vec::new(),
        trace_manifest_path,
        trace_last_flushed: Instant::now(),
        trace_bytes: 0,
        trace_serialization_failures: 0,
        trace_write_failures: 0,
        trace_byte_cap_discarded_events: 0,
        trace_sampling_discarded_events: 0,
        trace_metric_last_elapsed: BTreeMap::new(),
        last_video_target_bits_per_second: None,
        last_video_sequence: None,
        last_host_bitrate_change_sequence: None,
        metrics: BTreeMap::new(),
        counters: BTreeMap::new(),
        clock_offset: None,
        artifact_count: 0,
        artifact_bytes: 0,
        artifact_discarded_by_limit: 0,
        artifact_deduplicated: 0,
        artifact_serialization_failures: 0,
        artifact_write_failures: 0,
        artifact_kinds: HashSet::new(),
        processing_micros: 0,
        processed_events: 0,
        worst_processing_micros: 0,
        overlay,
        overlay_updated_at: Instant::now() - Duration::from_secs(1),
    };
    loop {
        match receiver.recv_timeout(TRACE_FLUSH_INTERVAL) {
            Ok(message) => {
                let event_started = Instant::now();
                state.process(message);
                let cost = u64::try_from(event_started.elapsed().as_micros()).unwrap_or(u64::MAX);
                state.processing_micros = state.processing_micros.saturating_add(cost);
                state.processed_events = state.processed_events.saturating_add(1);
                state.worst_processing_micros = state.worst_processing_micros.max(cost);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => state.flush_trace(),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if state.trace_last_flushed.elapsed() >= TRACE_FLUSH_INTERVAL {
            state.flush_trace();
        }
    }
    state.finish(queue_rejected_events.load(Ordering::Relaxed))?;
    Ok(WorkerOutput { summary_path })
}

impl WorkerState {
    fn process(&mut self, message: WorkerMessage) {
        match message {
            WorkerMessage::Observation(event) => {
                let accumulator = self.metrics.entry(event.metric).or_default();
                accumulator.unit = Some(event.unit);
                accumulator.larger_is_better = event.metric == "video_luma_psnr";
                accumulator.samples.observe(event.value);
                self.write_trace(&event);
            }
            WorkerMessage::Counter {
                elapsed_micros,
                name,
                value,
            } => self.set_counter(elapsed_micros, name, value),
            WorkerMessage::IncrementCounter {
                elapsed_micros,
                name,
            } => {
                let value = self
                    .counters
                    .get(name)
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(1);
                self.set_counter(elapsed_micros, name, value);
            }
            WorkerMessage::AddCounter {
                elapsed_micros,
                name,
                value,
            } => {
                let value = self
                    .counters
                    .get(name)
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(value);
                self.set_counter(elapsed_micros, name, value);
            }
            WorkerMessage::VideoFrame(record) => {
                let restarted = self
                    .last_video_sequence
                    .is_some_and(|sequence| record.sequence <= sequence);
                self.last_video_sequence = Some(record.sequence);
                let previous = if restarted {
                    self.last_video_target_bits_per_second =
                        Some(record.target_bitrate_bits_per_second);
                    None
                } else {
                    self.last_video_target_bits_per_second
                        .replace(record.target_bitrate_bits_per_second)
                };
                if previous != Some(record.target_bitrate_bits_per_second) {
                    let reason = previous.map_or("startup", |previous| {
                        if record.target_bitrate_bits_per_second < previous {
                            "congestion"
                        } else {
                            "healthy_delivery"
                        }
                    });
                    self.write_trace_record(
                        record.elapsed_micros,
                        &BitrateChangeTraceRecord {
                            record_type: "bitrate_change",
                            schema_version: TRACE_SCHEMA_VERSION,
                            elapsed_micros: record.elapsed_micros,
                            sequence: record.sequence,
                            previous_target_bits_per_second: previous,
                            target_bits_per_second: record.target_bitrate_bits_per_second,
                            reason,
                        },
                    );
                }
                self.write_trace_record(record.elapsed_micros, &record);
            }
            WorkerMessage::HostBitrate(record) => {
                if self.last_host_bitrate_change_sequence != Some(record.change_sequence) {
                    self.last_host_bitrate_change_sequence = Some(record.change_sequence);
                    self.write_trace_record(record.elapsed_micros, &record);
                }
            }
            WorkerMessage::ClockOffset(estimate) => self.clock_offset = Some(estimate),
            WorkerMessage::Anomaly {
                elapsed_micros,
                kind,
                media_sequence,
                values,
            } => self.write_anomaly(elapsed_micros, kind, media_sequence, &values),
        }
        if self.overlay_updated_at.elapsed() >= Duration::from_millis(500) {
            self.refresh_overlay();
            self.overlay_updated_at = Instant::now();
        }
    }

    fn set_counter(&mut self, elapsed: u64, name: &'static str, value: u64) {
        let previous = self.counters.insert(name, value).unwrap_or(0);
        if value != previous {
            self.write_trace_record(
                elapsed,
                &CounterTraceRecord {
                    record_type: "counter",
                    schema_version: TRACE_SCHEMA_VERSION,
                    elapsed_micros: elapsed,
                    name,
                    value,
                },
            );
        }
        if value > previous
            && matches!(
                name,
                "integrity_audio_mismatches"
                    | "integrity_video_mismatches"
                    | "worker_service_audio_mismatches"
                    | "worker_service_video_mismatches"
                    | "audio_decoder_failures"
                    | "video_decoder_failures"
                    | "audio_device_failures"
                    | "audio_device_query_failures"
                    | "audio_device_submission_failures"
                    | "audio_underruns"
                    | "audio_software_capacity_rejections"
                    | "audio_missing_packets"
                    | "audio_expired_packets"
                    | "audio_overflow_packets"
                    | "audio_late_fragments"
                    | "audio_malformed_fragments"
                    | "video_lost_chunks"
                    | "video_late_chunks"
                    | "video_incomplete_frames"
                    | "video_skipped_frames"
                    | "video_assembly_overflows"
                    | "video_render_queue_drops"
                    | "input_reliable_rejected"
                    | "input_pointer_mode_rejections"
            )
        {
            self.write_anomaly(
                elapsed,
                name,
                None,
                &[("previous", previous), ("current", value)],
            );
        }
    }

    fn write_trace(&mut self, event: &TraceEvent) {
        if self
            .trace_metric_last_elapsed
            .get(event.metric)
            .is_some_and(|last| event.elapsed_micros.saturating_sub(*last) < 50_000)
        {
            self.trace_sampling_discarded_events =
                self.trace_sampling_discarded_events.saturating_add(1);
            return;
        }
        self.trace_metric_last_elapsed
            .insert(event.metric, event.elapsed_micros);
        self.write_trace_record(event.elapsed_micros, event);
    }

    fn write_trace_record<T: Serialize>(&mut self, elapsed: u64, record: &T) {
        let mut bytes = Vec::with_capacity(256);
        if serde_json::to_writer(&mut bytes, record).is_err() {
            self.trace_serialization_failures = self.trace_serialization_failures.saturating_add(1);
            return;
        }
        bytes.push(b'\n');
        let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if self.ensure_trace_chunk(elapsed).is_err() {
            self.trace_write_failures = self.trace_write_failures.saturating_add(1);
            return;
        }
        let Some(trace) = self.trace.as_mut() else {
            self.trace_write_failures = self.trace_write_failures.saturating_add(1);
            return;
        };
        if trace.write_all(&bytes).is_err() {
            self.trace_write_failures = self.trace_write_failures.saturating_add(1);
            return;
        }
        self.trace_bytes = self.trace_bytes.saturating_add(length);
        if let Some(chunk) = self.trace_chunk.as_mut() {
            chunk.end_elapsed_micros = elapsed;
            chunk.records = chunk.records.saturating_add(1);
            chunk.bytes = chunk.bytes.saturating_add(length);
        }
    }

    fn ensure_trace_chunk(&mut self, elapsed: u64) -> io::Result<()> {
        let index = elapsed / TRACE_CHUNK_MICROS;
        if let Some(chunk) = self.trace_chunk.as_ref()
            && chunk.index >= index
        {
            return Ok(());
        }
        self.finish_trace_chunk()?;
        let path = PathBuf::from(format!(
            "{}-trace-{index:05}.jsonl",
            self.artifact_stem.display()
        ));
        self.trace = Some(BufWriter::new(File::create(&path)?));
        self.trace_chunk = Some(TraceChunkSummary {
            index,
            path: path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_owned(),
            start_elapsed_micros: elapsed,
            end_elapsed_micros: elapsed,
            records: 0,
            bytes: 0,
        });
        self.trace_last_flushed = Instant::now();
        Ok(())
    }

    fn flush_trace(&mut self) {
        if let Some(trace) = self.trace.as_mut()
            && trace.flush().is_err()
        {
            self.trace_write_failures = self.trace_write_failures.saturating_add(1);
        }
        self.trace_last_flushed = Instant::now();
    }

    fn finish_trace_chunk(&mut self) -> io::Result<()> {
        if let Some(mut trace) = self.trace.take() {
            trace.flush()?;
        }
        if let Some(chunk) = self.trace_chunk.take() {
            self.trace_chunks.push(chunk);
        }
        Ok(())
    }

    fn write_anomaly(
        &mut self,
        elapsed: u64,
        kind: &'static str,
        media_sequence: Option<u64>,
        values: &[(&'static str, u64)],
    ) {
        if !self.artifact_kinds.insert(kind) {
            self.artifact_deduplicated = self.artifact_deduplicated.saturating_add(1);
            return;
        }
        if self.artifact_count >= MAX_ANOMALY_ARTIFACTS || self.artifact_bytes >= MAX_ANOMALY_BYTES
        {
            self.artifact_discarded_by_limit = self.artifact_discarded_by_limit.saturating_add(1);
            return;
        }
        let artifact = json!({
            "kind": kind,
            "media_sequence": media_sequence,
            "elapsed_micros": elapsed,
            "values": values.iter().copied().collect::<BTreeMap<_, _>>(),
            "content": "numeric diagnostics only; no frame or audio payload",
        });
        let bytes = match serde_json::to_vec_pretty(&artifact) {
            Ok(bytes) => bytes,
            Err(_) => {
                self.artifact_serialization_failures =
                    self.artifact_serialization_failures.saturating_add(1);
                return;
            }
        };
        let bytes_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if self.artifact_bytes.saturating_add(bytes_len) > MAX_ANOMALY_BYTES {
            self.artifact_discarded_by_limit = self.artifact_discarded_by_limit.saturating_add(1);
            return;
        }
        let number = self.artifact_count + 1;
        let path = PathBuf::from(format!(
            "{}-anomaly-{number}.anomaly",
            self.artifact_stem.display()
        ));
        if fs::write(&path, bytes).is_ok() {
            self.artifact_count = number;
            self.artifact_bytes = self.artifact_bytes.saturating_add(bytes_len);
        } else {
            let _ = fs::remove_file(path);
            self.artifact_write_failures = self.artifact_write_failures.saturating_add(1);
        }
    }

    fn refresh_overlay(&self) {
        *self
            .overlay
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = diagnostic_overlay_text(
            &self.metrics,
            self.counters
                .get("video_learned_soft_ceiling_bits_per_second")
                .copied(),
            self.clock_offset,
        );
    }

    fn finish(&mut self, queue_rejected_events: u64) -> io::Result<()> {
        self.finish_trace_chunk()?;
        let manifest = TraceManifest {
            schema_version: TRACE_SCHEMA_VERSION,
            session_id: self.session_id.clone(),
            chunk_duration_micros: TRACE_CHUNK_MICROS,
            chunks: self.trace_chunks.clone(),
        };
        let mut manifest_writer = BufWriter::new(File::create(&self.trace_manifest_path)?);
        serde_json::to_writer_pretty(&mut manifest_writer, &manifest)?;
        manifest_writer.flush()?;
        let metrics = self
            .metrics
            .iter()
            .map(|(&name, accumulator)| {
                (
                    name,
                    metric_summary(
                        &accumulator.samples,
                        accumulator.unit.unwrap_or("units"),
                        accumulator.larger_is_better,
                    ),
                )
            })
            .collect();
        let summary = DiagnosticSummary {
            session_id: self.session_id.clone(),
            generated_at_unix_micros: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_micros(),
            duration_micros: u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX),
            mode: "full-diagnostics",
            trace_queue_capacity: TRACE_QUEUE_CAPACITY,
            trace_queue_rejected_events: queue_rejected_events,
            trace_serialization_failures: self.trace_serialization_failures,
            trace_write_failures: self.trace_write_failures,
            trace_byte_cap_discarded_events: self.trace_byte_cap_discarded_events,
            trace_sampling_discarded_events: self.trace_sampling_discarded_events,
            trace_bytes: self.trace_bytes,
            trace_byte_limit: 0,
            trace_manifest: self
                .trace_manifest_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_owned(),
            trace_chunks: self.trace_chunks.clone(),
            aggregation_worker_processing_micros: self.processing_micros,
            aggregation_worker_events: self.processed_events,
            aggregation_worker_worst_event_micros: self.worst_processing_micros,
            clock: self.clock_offset.map(|estimate| ClockSummary {
                offset_micros: estimate.offset_micros,
                uncertainty_micros: estimate.uncertainty_micros,
                method: "minimum-RTT midpoint estimate; cross-machine stages are clock-adjusted",
            }),
            metrics,
            counters: self.counters.clone(),
            anomaly_artifacts: self.artifact_count,
            anomaly_artifact_bytes: self.artifact_bytes,
            anomaly_artifacts_discarded_by_limit: self.artifact_discarded_by_limit,
            anomaly_artifacts_deduplicated: self.artifact_deduplicated,
            anomaly_serialization_failures: self.artifact_serialization_failures,
            anomaly_write_failures: self.artifact_write_failures,
            unavailable_boundaries: &[
                "physical display-panel response after presentation feedback",
                "analog audio after SDL device submission, including DAC and speakers",
                "Windows kernel HID report consumption after user-mode ring publication",
                "target-application input handling after kernel HID consumption",
                "GPU utilization because the active Vulkan and D3D11 paths expose no portable utilization counter",
            ],
            notes: &[
                "Latency values are microseconds unless a metric declares another unit.",
                "Presentation feedback is the latest software-visible compositor or display timing boundary, not panel response.",
                "Audio playback timing ends at SDL queue or callback transfer, not physical sound output.",
                "Input timing ends at Windows user-mode shared-ring publication; later endpoints are unavailable.",
                "Derived and clock-adjusted metrics identify their classification in the event trace.",
                "Network path kind: 0 unknown, 1 IP without Tailscale, 2 Tailscale direct, 3 Tailscale DERP relay, 4 Tailscale peer relay, 5 Tailscale path unknown.",
                "Network player/host link: 0 unknown, 1 Ethernet, 2 Wi-Fi, 3 other or virtual; the host reports its internet route when its peer route is virtual.",
                "Network status is sampled every second; shorter path changes can be missed. The physical internet route is context, not proof of which interface a Tailscale relay used.",
                "The named diagnostic-writer thread owns aggregation, serialization, file output, and anomaly selection.",
            ],
        };
        let file = File::create(&self.summary_path)?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, &summary)?;
        writer.flush()
    }
}

fn diagnostic_metric_group(name: &str, unit: &str) -> (u8, &'static str) {
    if unit == "bits-per-second" || name.contains("bitrate") {
        (0, "Bitrates")
    } else if name.contains("quality") || name.contains("luma") {
        (6, "Video quality")
    } else if unit == "microseconds" {
        if name.starts_with("audio_") {
            (2, "Audio latencies")
        } else if name.starts_with("input_") || name.contains("_input_") {
            (3, "Input latencies")
        } else if name.starts_with("video_")
            || matches!(
                name,
                "decode" | "frame_assembly" | "decode_queue_to_present"
            )
            || name.starts_with("host_capture_")
            || name.starts_with("host_packetization_")
        {
            (1, "Video latencies")
        } else {
            (4, "Other timings")
        }
    } else if name.contains("queue") || name.contains("buffer") {
        (7, "Queues and buffering")
    } else if name.starts_with("network_") || name.contains("_path_") {
        (5, "Network")
    } else if name.contains("cpu_load") {
        (9, "Processing load")
    } else if name.contains("integrity") || name.contains("hash") || name.contains("diagnostic") {
        (10, "Integrity and diagnostics")
    } else if unit == "bytes" {
        (8, "Frame and packet sizes")
    } else {
        (11, "Other measurements")
    }
}

fn diagnostic_overlay_text(
    metrics: &BTreeMap<&'static str, MetricAccumulator>,
    delivery_ceiling: Option<u64>,
    clock: Option<ClockOffsetEstimate>,
) -> String {
    let mut groups = BTreeMap::<(u8, &'static str), Vec<String>>::new();
    if let Some(ceiling) = delivery_ceiling {
        groups
            .entry((0, "Bitrates"))
            .or_default()
            .push(if ceiling == 0 {
                "Delivery ceiling: not established".to_owned()
            } else {
                format!(
                    "Delivery ceiling: {:.2} Mbit/s",
                    ceiling as f64 / 1_000_000.0
                )
            });
    }
    for (name, accumulator) in metrics {
        let values = accumulator.samples.sorted();
        if !values.is_empty() {
            let unit = accumulator.unit.unwrap_or("units");
            groups
                .entry(diagnostic_metric_group(name, unit))
                .or_default()
                .push(format!(
                    "{name}: p50 {} p99 {} {unit}",
                    percentile(&values, 50),
                    percentile(&values, 99),
                ));
        }
    }
    if let Some(clock) = clock {
        groups
            .entry((12, "Clock alignment"))
            .or_default()
            .push(format!(
                "clock: offset {} us +/-{} us",
                clock.offset_micros, clock.uncertainty_micros,
            ));
    }
    let mut sections = vec!["Full diagnostics active".to_owned()];
    for ((_, heading), lines) in groups {
        sections.push(format!("{heading}\n{}", lines.join("\n")));
    }
    sections.join("\n\n")
}

fn metric_summary(
    samples: &BoundedSamples,
    unit: &'static str,
    larger_is_better: bool,
) -> MetricSummary {
    let values = samples.sorted();
    let (p50, p90, p95, p99, worst) = if values.is_empty() {
        (None, None, None, None, None)
    } else {
        if larger_is_better {
            (
                Some(percentile(&values, 50)),
                Some(percentile(&values, 10)),
                Some(percentile(&values, 5)),
                Some(percentile(&values, 1)),
                values.first().copied(),
            )
        } else {
            (
                Some(percentile(&values, 50)),
                Some(percentile(&values, 90)),
                Some(percentile(&values, 95)),
                Some(percentile(&values, 99)),
                values.last().copied(),
            )
        }
    };
    MetricSummary {
        samples: samples.observed,
        discarded_samples: samples.discarded,
        unit,
        p50_micros: p50,
        p90_micros: p90,
        p95_micros: p95,
        p99_micros: p99,
        worst_micros: worst,
    }
}

fn percentile(values: &[u64], percent: usize) -> u64 {
    values[(values.len() * percent).div_ceil(100).saturating_sub(1)]
}

fn diagnostic_directory() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME").map_or_else(
        || {
            std::env::var_os("HOME").map_or_else(
                || PathBuf::from(".rustconsole/diagnostics"),
                |home| PathBuf::from(home).join(".local/state/rustconsole/diagnostics"),
            )
        },
        |state| PathBuf::from(state).join("rustconsole/diagnostics"),
    )
}

fn elapsed_micros(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn bitrate_change_cause_name(cause: u64) -> &'static str {
    use rustconsole_protocol::diagnostics::VideoBitrateChangeCause;
    match cause {
        value if value == VideoBitrateChangeCause::Startup as u64 => "startup",
        value if value == VideoBitrateChangeCause::HealthyDelivery as u64 => "healthy_delivery",
        value if value == VideoBitrateChangeCause::MildDegradation as u64 => "mild_degradation",
        value if value == VideoBitrateChangeCause::SevereReceiverLoss as u64 => {
            "severe_receiver_loss"
        }
        value if value == VideoBitrateChangeCause::SeverePathPressure as u64 => {
            "severe_path_pressure"
        }
        value if value == VideoBitrateChangeCause::SenderCongestion as u64 => "sender_congestion",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_summary_uses_selected_percentiles() {
        let mut samples = BoundedSamples::default();
        for value in 1..=100 {
            samples.observe(value);
        }
        let summary = metric_summary(&samples, "microseconds", false);
        assert_eq!(summary.p50_micros, Some(50));
        assert_eq!(summary.p90_micros, Some(90));
        assert_eq!(summary.p95_micros, Some(95));
        assert_eq!(summary.p99_micros, Some(99));
        assert_eq!(summary.worst_micros, Some(100));
    }

    #[test]
    fn advanced_overlay_groups_metrics_and_preserves_names_values_and_units() {
        let observations = [
            ("video_host_target_bitrate", "bits-per-second", "Bitrates"),
            ("video_render_queue", "microseconds", "Video latencies"),
            ("audio_decode", "microseconds", "Audio latencies"),
            ("input_ack_round_trip", "microseconds", "Input latencies"),
            ("video_host_path_congestion_window", "bytes", "Network"),
            ("video_luma_mean_absolute_error", "ppm", "Video quality"),
            (
                "audio_capture_buffer_frames",
                "frames",
                "Queues and buffering",
            ),
            (
                "video_encoded_frame_size",
                "bytes",
                "Frame and packet sizes",
            ),
            ("player_process_cpu_load", "percent", "Processing load"),
            ("video_bitrate_change_cause_sample", "enum", "Bitrates"),
            ("audio_concealed_packets", "packets", "Other measurements"),
        ];
        let metrics = observations
            .iter()
            .map(|(name, unit, _)| {
                let mut accumulator = MetricAccumulator {
                    unit: Some(*unit),
                    ..Default::default()
                };
                accumulator.samples.observe(17);
                (*name, accumulator)
            })
            .collect();
        let text = diagnostic_overlay_text(
            &metrics,
            Some(4_000_000),
            Some(ClockOffsetEstimate {
                offset_micros: -10,
                uncertainty_micros: 3,
            }),
        );
        assert!(
            text.starts_with("Full diagnostics active\n\nBitrates\nDelivery ceiling: 4.00 Mbit/s")
        );
        for (name, unit, heading) in observations {
            let section = text
                .split("\n\n")
                .find(|section| section.lines().next() == Some(heading))
                .unwrap();
            let measurement = format!("{name}: p50 17 p99 17 {unit}");
            assert!(section.lines().any(|line| line == measurement));
            assert_eq!(text.matches(&measurement).count(), 1);
        }
        assert!(text.contains("Clock alignment\nclock: offset -10 us +/-3 us"));
    }

    #[test]
    fn larger_is_better_summary_reports_low_tail_and_minimum() {
        let mut samples = BoundedSamples::default();
        for value in 1..=100 {
            samples.observe(value);
        }
        let summary = metric_summary(&samples, "millidecibels", true);
        assert_eq!(summary.p50_micros, Some(50));
        assert_eq!(summary.p90_micros, Some(10));
        assert_eq!(summary.p95_micros, Some(5));
        assert_eq!(summary.p99_micros, Some(1));
        assert_eq!(summary.worst_micros, Some(1));
    }

    #[test]
    fn bitrate_change_causes_have_readable_trace_names() {
        use rustconsole_protocol::diagnostics::VideoBitrateChangeCause;

        assert_eq!(
            bitrate_change_cause_name(VideoBitrateChangeCause::MildDegradation as u64),
            "mild_degradation"
        );
        assert_eq!(bitrate_change_cause_name(u64::MAX), "unknown");
    }

    #[test]
    fn disabled_diagnostics_do_not_start_a_worker() {
        let diagnostics = LatencyDiagnostics::disabled();
        assert!(!diagnostics.enabled());
        assert!(diagnostics.worker.is_none());
    }

    #[test]
    fn worker_deduplicates_repeated_faults_without_hiding_distinct_faults() {
        let directory = std::env::temp_dir().join(format!(
            "rustconsole-diagnostic-worker-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let overlay = Arc::new(Mutex::new(String::new()));
        let rejected = Arc::new(AtomicU64::new(0));
        let (sender, receiver) = mpsc::sync_channel(8);
        let worker = thread::spawn({
            let directory = directory.clone();
            move || {
                run_worker(
                    directory,
                    "test".into(),
                    Instant::now(),
                    overlay,
                    rejected,
                    receiver,
                )
            }
        });
        for (kind, value) in [("first", 1), ("first", 2), ("second", 3)] {
            sender
                .send(WorkerMessage::Anomaly {
                    elapsed_micros: value,
                    kind,
                    media_sequence: None,
                    values: vec![("value", value)],
                })
                .unwrap();
        }
        drop(sender);
        worker.join().unwrap().unwrap();

        let summary: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.join("session-test.json")).unwrap())
                .unwrap();
        assert_eq!(summary["anomaly_artifacts"], 2);
        assert_eq!(summary["anomaly_artifacts_deduplicated"], 1);
        assert!(directory.join("session-test-anomaly-1.anomaly").is_file());
        assert!(directory.join("session-test-anomaly-2.anomaly").is_file());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn worker_rotates_readable_unbounded_trace_chunks_by_elapsed_minute() {
        let directory = std::env::temp_dir().join(format!(
            "rustconsole-diagnostic-chunk-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let overlay = Arc::new(Mutex::new(String::new()));
        let rejected = Arc::new(AtomicU64::new(0));
        let (sender, receiver) = mpsc::sync_channel(8);
        let worker = thread::spawn({
            let directory = directory.clone();
            move || {
                run_worker(
                    directory,
                    "chunks".into(),
                    Instant::now(),
                    overlay,
                    rejected,
                    receiver,
                )
            }
        });
        for (elapsed, sequence) in [(1, 1), (TRACE_CHUNK_MICROS + 1, 2)] {
            sender
                .send(WorkerMessage::VideoFrame(VideoFrameTraceRecord {
                    record_type: "video_frame",
                    schema_version: TRACE_SCHEMA_VERSION,
                    elapsed_micros: elapsed,
                    sequence,
                    keyframe: false,
                    encoded_frame_bytes: 1_000,
                    target_bitrate_bits_per_second: 10_000_000,
                    delivered_goodput_bits_per_second: 9_000_000,
                    soft_ceiling_bits_per_second: None,
                    round_trip_time_micros: 8_000,
                    received_chunks: sequence,
                    lost_chunks: 0,
                    late_chunks: 0,
                    assembly_overflows: 0,
                    completed_frames: sequence,
                    incomplete_frames: 0,
                }))
                .unwrap();
        }
        drop(sender);
        worker.join().unwrap().unwrap();

        let manifest: serde_json::Value = serde_json::from_slice(
            &fs::read(directory.join("session-chunks-trace-manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["schema_version"], TRACE_SCHEMA_VERSION);
        assert_eq!(manifest["chunks"].as_array().unwrap().len(), 2);
        for index in 0..=1 {
            let path = directory.join(format!("session-chunks-trace-{index:05}.jsonl"));
            let trace = fs::read_to_string(path).unwrap();
            let record: serde_json::Value = trace
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .find(|record: &serde_json::Value| record["record_type"] == "video_frame")
                .unwrap();
            assert_eq!(record["sequence"], index + 1);
        }
        let summary: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.join("session-chunks.json")).unwrap())
                .unwrap();
        assert_eq!(summary["trace_byte_limit"], 0);
        assert_eq!(summary["trace_byte_cap_discarded_events"], 0);
        fs::remove_dir_all(directory).unwrap();
    }
}
