use rustconsole_player_core::ClockOffsetEstimate;
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TRACE_QUEUE_CAPACITY: usize = 4_096;
const TRACE_BYTE_LIMIT: u64 = 16 * 1024 * 1024;
const RETAINED_SESSIONS: usize = 10;
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

enum WorkerMessage {
    Observation(TraceEvent),
    Counter {
        name: &'static str,
        value: u64,
    },
    IncrementCounter {
        name: &'static str,
    },
    AddCounter {
        name: &'static str,
        value: u64,
    },
    ClockOffset(ClockOffsetEstimate),
    Anomaly {
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

struct WorkerOutput {
    summary_path: PathBuf,
}

struct WorkerState {
    session_id: String,
    started: Instant,
    artifact_stem: PathBuf,
    summary_path: PathBuf,
    trace: BufWriter<File>,
    trace_bytes: u64,
    trace_serialization_failures: u64,
    trace_write_failures: u64,
    trace_byte_cap_discarded_events: u64,
    trace_sampling_discarded_events: u64,
    trace_metric_last_elapsed: BTreeMap<&'static str, u64>,
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
        self.submit(WorkerMessage::Counter { name, value });
    }

    pub fn increment_counter(&mut self, name: &'static str) {
        self.submit(WorkerMessage::IncrementCounter { name });
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
    retain_recent_sessions(&directory)?;
    let artifact_stem = directory.join(format!("session-{session_id}"));
    let summary_path = artifact_stem.with_extension("json");
    let trace_path = artifact_stem.with_extension("jsonl");
    let trace = BufWriter::new(File::create(trace_path)?);
    let mut state = WorkerState {
        session_id,
        started,
        artifact_stem,
        summary_path: summary_path.clone(),
        trace,
        trace_bytes: 0,
        trace_serialization_failures: 0,
        trace_write_failures: 0,
        trace_byte_cap_discarded_events: 0,
        trace_sampling_discarded_events: 0,
        trace_metric_last_elapsed: BTreeMap::new(),
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
    while let Ok(message) = receiver.recv() {
        let event_started = Instant::now();
        state.process(message);
        let cost = u64::try_from(event_started.elapsed().as_micros()).unwrap_or(u64::MAX);
        state.processing_micros = state.processing_micros.saturating_add(cost);
        state.processed_events = state.processed_events.saturating_add(1);
        state.worst_processing_micros = state.worst_processing_micros.max(cost);
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
            WorkerMessage::Counter { name, value } => self.set_counter(name, value),
            WorkerMessage::IncrementCounter { name } => {
                let value = self
                    .counters
                    .get(name)
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(1);
                self.set_counter(name, value);
            }
            WorkerMessage::AddCounter { name, value } => {
                let value = self
                    .counters
                    .get(name)
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(value);
                self.set_counter(name, value);
            }
            WorkerMessage::ClockOffset(estimate) => self.clock_offset = Some(estimate),
            WorkerMessage::Anomaly {
                kind,
                media_sequence,
                values,
            } => self.write_anomaly(kind, media_sequence, &values),
        }
        if self.overlay_updated_at.elapsed() >= Duration::from_millis(500) {
            self.refresh_overlay();
            self.overlay_updated_at = Instant::now();
        }
    }

    fn set_counter(&mut self, name: &'static str, value: u64) {
        let previous = self.counters.insert(name, value).unwrap_or(0);
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
                    | "video_assembly_overflows"
                    | "video_render_queue_drops"
                    | "input_reliable_rejected"
                    | "input_pointer_mode_rejections"
            )
        {
            self.write_anomaly(name, None, &[("previous", previous), ("current", value)]);
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
        let mut bytes = Vec::with_capacity(256);
        if serde_json::to_writer(&mut bytes, event).is_err() {
            self.trace_serialization_failures = self.trace_serialization_failures.saturating_add(1);
            return;
        }
        bytes.push(b'\n');
        let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if self.trace_bytes.saturating_add(length) > TRACE_BYTE_LIMIT {
            self.trace_byte_cap_discarded_events =
                self.trace_byte_cap_discarded_events.saturating_add(1);
            return;
        }
        if self.trace.write_all(&bytes).is_err() {
            self.trace_write_failures = self.trace_write_failures.saturating_add(1);
            return;
        }
        self.trace_bytes = self.trace_bytes.saturating_add(length);
    }

    fn write_anomaly(
        &mut self,
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
            "elapsed_micros": elapsed_micros(self.started),
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
        let mut lines = vec!["Full diagnostics active".to_owned()];
        for (name, accumulator) in &self.metrics {
            let values = accumulator.samples.sorted();
            if !values.is_empty() {
                lines.push(format!(
                    "{name}: p50 {} p99 {} {}",
                    percentile(&values, 50),
                    percentile(&values, 99),
                    accumulator.unit.unwrap_or("units")
                ));
            }
        }
        if let Some(clock) = self.clock_offset {
            lines.push(format!(
                "clock: offset {} us +/-{} us",
                clock.offset_micros, clock.uncertainty_micros
            ));
        }
        *self
            .overlay
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = lines.join("\n");
    }

    fn finish(&mut self, queue_rejected_events: u64) -> io::Result<()> {
        self.trace.flush()?;
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
            trace_byte_limit: TRACE_BYTE_LIMIT,
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
                "The named diagnostic-writer thread owns aggregation, serialization, file output, and anomaly selection.",
            ],
        };
        let file = File::create(&self.summary_path)?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, &summary)?;
        writer.flush()
    }
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

fn retain_recent_sessions(directory: &Path) -> io::Result<()> {
    let mut summaries = fs::read_dir(directory)?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            (name.starts_with("session-") && name.ends_with(".json")).then(|| entry.path())
        })
        .collect::<Vec<_>>();
    summaries.sort_unstable();
    let remove_count = summaries.len().saturating_sub(RETAINED_SESSIONS - 1);
    for summary in summaries.into_iter().take(remove_count) {
        let stem = summary.with_extension("");
        let _ = fs::remove_file(&summary);
        let _ = fs::remove_file(stem.with_extension("jsonl"));
        if let Some(stem) = stem.to_str() {
            for number in 1..=MAX_ANOMALY_ARTIFACTS {
                let _ = fs::remove_file(format!("{stem}-anomaly-{number}.anomaly"));
            }
        }
    }
    Ok(())
}

fn elapsed_micros(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
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
}
