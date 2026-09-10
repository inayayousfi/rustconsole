use backon::BackoffBuilder;
use rustconsole_player_core::process_protocol::{
    LaunchRequest, PlayerCommand, PlayerEvent, read_command, write_event,
};
use rustconsole_player_core::{StreamProgress, TimedInputEvent};
use rustconsole_player_gui::{GuiFrame, PlayerGui, PlayerGuiAction, PlayerGuiView, PointerButton};
use rustconsole_player_linux::{
    AudioPlaybackDecision, AudioPlaybackQueue, AudioPlaybackSnapshot, Av1ColorDescription,
    DecodedAudioSamples, DecodedVideoFrame, DmaBufFrameFormat, NativeDmaBufFrame, SdlAudioOutput,
    StreamCallbacks, VideoStreamSample,
};
use rustconsole_protocol::InputEvent;
use rustconsole_render::{DecodedVideoColor, OverlayStatistics, PlayerVideoBackend};
use rustconsole_render_vulkan::{VulkanOutputPreference, VulkanRenderer};
use rustconsole_render_vulkan_linux::DmaBufFrameImporter;
use sdl3::event::{Event, WindowEvent};
use sdl3::iostream::IOStream;
use sdl3::mouse::MouseButton;
use sdl3::surface::Surface;
use sdl3::video::{FullscreenType, Window};
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufReader, BufWriter};
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

mod diagnostics;
use diagnostics::LatencyDiagnostics;

const RENDERING_BACKEND_LABEL: &str = "Rendering backend: Vulkan";
const RECONNECT_STABLE_RESET: Duration = Duration::from_secs(30);
const PRESENTATION_TRACKER_CAPACITY: usize = 512;

struct ProcessCpuSampler {
    wall: Instant,
    cpu_micros: u64,
}

impl ProcessCpuSampler {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            wall: Instant::now(),
            cpu_micros: process_cpu_micros()?,
        })
    }

    fn sample(&mut self) -> std::io::Result<Option<u64>> {
        let wall = self.wall.elapsed();
        if wall < Duration::from_millis(500) {
            return Ok(None);
        }
        let cpu_micros = process_cpu_micros()?;
        let cpu_delta = cpu_micros.saturating_sub(self.cpu_micros);
        let wall_micros = wall.as_micros().max(1);
        let basis_points = (u128::from(cpu_delta) * 10_000 / wall_micros).min(u128::from(u64::MAX));
        self.wall = Instant::now();
        self.cpu_micros = cpu_micros;
        Ok(Some(basis_points as u64))
    }
}

fn process_cpu_micros() -> std::io::Result<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the provided rusage value on success.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the successful call above initialized usage.
    let usage = unsafe { usage.assume_init() };
    let timeval_micros = |time: libc::timeval| {
        u64::try_from(time.tv_sec)
            .unwrap_or(0)
            .saturating_mul(1_000_000)
            .saturating_add(u64::try_from(time.tv_usec).unwrap_or(0))
    };
    Ok(timeval_micros(usage.ru_utime).saturating_add(timeval_micros(usage.ru_stime)))
}

fn set_window_icon(window: &mut Window) {
    let result = (|| -> Result<(), sdl3::Error> {
        let mut icon_stream = IOStream::from_bytes(include_bytes!("../../../assets/icon.bmp"))?;
        let icon = Surface::load_bmp_rw(&mut icon_stream)?;
        if window.set_icon(&icon) {
            Ok(())
        } else {
            Err(sdl3::get_error())
        }
    })();
    if let Err(error) = result {
        eprintln!("rustconsole-player: could not set window icon: {error}");
    }
}

fn reconnect_backoff(seed: Option<u64>) -> impl backon::Backoff {
    let mut builder = backon::ExponentialBuilder::default()
        .with_min_delay(Duration::from_millis(250))
        .with_max_delay(Duration::from_secs(5))
        .with_factor(2.0)
        .with_jitter()
        .without_max_times();
    if let Some(seed) = seed {
        builder = builder.with_jitter_seed(seed);
    }
    builder.build()
}

fn should_retry_session_end(ever_streamed: bool, failed: bool) -> bool {
    ever_streamed && failed
}

fn should_reset_reconnect_backoff(recovering: bool, stable_for: Option<Duration>) -> bool {
    recovering && stable_for.is_some_and(|elapsed| elapsed >= RECONNECT_STABLE_RESET)
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rustconsole-player: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [mode] if mode == "pipe-session" => run_pipe_session(),
        [mode, report] if mode == "surface-proof" => run_surface_proof(Path::new(report)),
        [mode, report] if mode == "audio-output-proof" => {
            let sdl = sdl3::init()?;
            rustconsole_player_linux::run_sdl_audio_proof(&sdl, Path::new(report))
        }
        _ => Err(
            "expected pipe-session, surface-proof <report-path>, or audio-output-proof <report-path>"
                .into(),
        ),
    }
}

fn run_surface_proof(report: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let frame = rustconsole_player_linux::decode_fixture_dma_buf()?;
    let sdl = sdl3::init()?;
    let video = sdl.video()?;
    let mut window = video
        .window("Rust Console Vulkan proof", 1280, 720)
        .position_centered()
        .resizable()
        .hidden()
        .vulkan()
        .build()
        .map_err(|error| error.to_string())?;
    set_window_icon(&mut window);
    if !window.show() {
        return Err(sdl3::get_error().into());
    }
    let mut events = sdl.event_pump()?;
    let mut gui = PlayerGui::default();
    let mut renderer = VulkanRenderer::new(
        &window,
        DmaBufFrameImporter,
        VulkanOutputPreference::Sdr,
        gui.context(),
    )?;
    let (width, height) = window.size_in_pixels();
    let (logical_width, logical_height) = window.size();
    gui.update_viewport(
        logical_width as f32,
        logical_height as f32,
        width as f32 / logical_width.max(1) as f32,
        0.0,
    );
    let (loading_gui, _) = gui.frame(PlayerGuiView {
        fullscreen: false,
        status: Some("Video negotiated\nWaiting for host packets\nWaiting 0.2 seconds"),
        diagnostics: "",
    });
    renderer.present_loading(width.max(1), height.max(1), 0.25, loading_gui)?;
    gui.update_viewport(
        logical_width as f32,
        logical_height as f32,
        width as f32 / logical_width.max(1) as f32,
        0.25,
    );
    let (frame_gui, _) = gui.frame(PlayerGuiView {
        fullscreen: false,
        status: None,
        diagnostics: "SDL3 Vulkan DMA-BUF\nfixture frame",
    });
    renderer.present(&frame, width.max(1), height.max(1), false, frame_gui)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while std::time::Instant::now() < deadline {
        for _ in events.poll_iter() {}
        std::thread::sleep(Duration::from_millis(4));
    }
    std::fs::write(
        report,
        "status=ok\nrenderer=sdl3-vulkan-dma-buf\nloading=procedural-draw-submitted\noverlay=egui-draw-submitted\npixel_readback=not-performed\n",
    )?;
    Ok(())
}

enum SessionEvent {
    Authenticated([u8; 32]),
    Progress(StreamProgress),
    Statistics(VideoStreamSample),
    Reconfigure(rustconsole_protocol::wire::VideoReconfigurationCause),
    Ended(Result<(), String>),
}

struct QueuedVideoFrame {
    sequence: u64,
    encoded_frame_bytes: usize,
    decoded_dma_buf_bytes: usize,
    captured_at_micros: u64,
    encoded_at_micros: u64,
    packetized_at_micros: u64,
    input_sequence: u64,
    assembled_at: Option<Instant>,
    assembled_at_micros: u64,
    assembly_duration: Duration,
    decoder_queue_duration: Option<Duration>,
    decode_duration: Duration,
    decode_to_dma_buf_export_duration: Option<Duration>,
    dma_buf_export_duration: Option<Duration>,
    decoder_input_hash_duration: Duration,
    assembly_to_decoder_matched: Option<bool>,
    diagnostic_marker_input_sequence: Option<u64>,
    diagnostic_copy_duration: Option<Duration>,
    diagnostic_copy_bytes: u64,
    capture_player_at: Option<Instant>,
    queued_at: Instant,
    color: Av1ColorDescription,
    frame: NativeDmaBufFrame,
}

struct PendingPresentation {
    frame_sequence: u64,
    frame_queued_at: Instant,
    capture_player_at: Option<Instant>,
    input_sequence: u64,
    input_started_at: Option<Instant>,
    diagnostic_marker_input_sequence: Option<u64>,
    correlated_input_started_at: Option<Instant>,
}

struct VideoPlaybackClock {
    captured_at_micros: u64,
    presented_at: Instant,
}

impl VideoPlaybackClock {
    fn timestamp_at(&self, now: Instant) -> u64 {
        self.captured_at_micros.saturating_add(
            u64::try_from(now.saturating_duration_since(self.presented_at).as_micros())
                .unwrap_or(u64::MAX),
        )
    }
}

struct ActiveStreamSession {
    stop: Arc<AtomicBool>,
    input: mpsc::SyncSender<TimedInputEvent>,
    input_queue_drops: Arc<AtomicU64>,
    diagnostic_probe_sequence: Arc<AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ActiveStreamSession {
    fn send_input(&self, event: InputEvent) {
        let occurred_at = Instant::now();
        let _ = self.input.send(TimedInputEvent { event, occurred_at });
    }

    fn try_send_input(&self, event: InputEvent) {
        if self
            .input
            .try_send(TimedInputEvent {
                event,
                occurred_at: Instant::now(),
            })
            .is_err()
        {
            self.input_queue_drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn start_stream_session(
    launch: &LaunchRequest,
    events: mpsc::Sender<SessionEvent>,
    frames: Arc<Mutex<VecDeque<QueuedVideoFrame>>>,
    video_render_queue_drops: Arc<AtomicU64>,
    audio: AudioPlaybackQueue,
) -> ActiveStreamSession {
    let stop = Arc::new(AtomicBool::new(false));
    let diagnostic_probe_sequence = Arc::new(AtomicU64::new(0));
    let input_queue_drops = Arc::new(AtomicU64::new(0));
    let (input, input_rx) = mpsc::sync_channel(1024);
    let session_stop = Arc::clone(&stop);
    let address = launch.address.to_string();
    let password = (!launch.password.is_empty()).then(|| launch.password.to_vec());
    let remember_password = launch.remember_password;
    let maximum_bitrate_bits_per_second = launch.maximum_bitrate_bits_per_second;
    let latency_diagnostics = launch.latency_diagnostics;
    let stream_diagnostic_probe_sequence = Arc::clone(&diagnostic_probe_sequence);
    let thread = std::thread::spawn(move || {
        let consume_stop = Arc::clone(&session_stop);
        let authenticated_tx = events.clone();
        let progress_tx = events.clone();
        let statistics_tx = events.clone();
        let result = rustconsole_player_linux::stream_quic_video(
            &address,
            password,
            remember_password,
            maximum_bitrate_bits_per_second,
            latency_diagnostics,
            stream_diagnostic_probe_sequence,
            || session_stop.load(Ordering::Acquire),
            move || input_rx.try_recv().ok(),
            StreamCallbacks {
                authenticated: move |host_identity| {
                    let _ = authenticated_tx.send(SessionEvent::Authenticated(host_identity));
                },
                progress: move |progress| {
                    let _ = progress_tx.send(SessionEvent::Progress(progress));
                },
                statistics: move |statistics| {
                    let _ = statistics_tx.send(SessionEvent::Statistics(statistics));
                },
                audio: move |event| {
                    audio.push(event);
                    Ok(())
                },
                video: move |decoded: DecodedVideoFrame| {
                    let dma_buf_export_started = latency_diagnostics.then(Instant::now);
                    let decode_to_dma_buf_export_duration = dma_buf_export_started
                        .zip(decoded.decoded_at)
                        .map(|(export_started, decoded_at)| {
                            export_started.saturating_duration_since(decoded_at)
                        });
                    let mapped = decoded.frame.map_dma_buf()?;
                    let dma_buf_export_duration =
                        dma_buf_export_started.map(|started| started.elapsed());
                    let decoded_dma_buf_bytes = latency_diagnostics.then(|| {
                        mapped
                            .objects()
                            .iter()
                            .fold(0_usize, |total, object| total.saturating_add(object.size))
                    });
                    let expected_format = match decoded.color_description {
                        Av1ColorDescription::Bt709Limited => DmaBufFrameFormat::Nv12,
                        Av1ColorDescription::Bt2020PqLimited => DmaBufFrameFormat::P010,
                    };
                    if mapped.frame_format() != Some(expected_format) {
                        return Err("decoded DMA-BUF format contradicts AV1 color metadata".into());
                    }
                    let mut frames = frames.lock().unwrap();
                    while frames.len() >= 2 {
                        frames.pop_front();
                        video_render_queue_drops.fetch_add(1, Ordering::Relaxed);
                    }
                    frames.push_back(QueuedVideoFrame {
                        sequence: decoded.sequence,
                        encoded_frame_bytes: decoded.encoded_frame_bytes,
                        decoded_dma_buf_bytes: decoded_dma_buf_bytes.unwrap_or(0),
                        captured_at_micros: decoded.captured_at_micros,
                        encoded_at_micros: decoded.encoded_at_micros,
                        packetized_at_micros: decoded.packetized_at_micros,
                        input_sequence: decoded.input_sequence,
                        assembled_at: decoded.assembled_at,
                        assembled_at_micros: decoded.assembled_at_micros,
                        assembly_duration: decoded.assembly_duration,
                        decoder_queue_duration: decoded.decoder_queue_duration,
                        decode_duration: decoded.decode_duration,
                        decode_to_dma_buf_export_duration,
                        dma_buf_export_duration,
                        decoder_input_hash_duration: decoded.decoder_input_hash_duration,
                        assembly_to_decoder_matched: decoded.assembly_to_decoder_matched,
                        diagnostic_marker_input_sequence: decoded.diagnostic_marker_input_sequence,
                        diagnostic_copy_duration: decoded.diagnostic_copy_duration,
                        diagnostic_copy_bytes: decoded.diagnostic_copy_bytes,
                        capture_player_at: None,
                        queued_at: Instant::now(),
                        color: decoded.color_description,
                        frame: mapped,
                    });
                    Ok(!consume_stop.load(Ordering::Acquire))
                },
            },
        );
        match result {
            Ok(result) => match result.end {
                rustconsole_player_core::StreamEnd::Stopped => {
                    let _ = events.send(SessionEvent::Ended(Ok(())));
                }
                rustconsole_player_core::StreamEnd::ReconfigurationRequired(cause) => {
                    let _ = events.send(SessionEvent::Reconfigure(cause));
                }
            },
            Err(error) => {
                let _ = events.send(SessionEvent::Ended(Err(error.to_string())));
            }
        }
    });
    ActiveStreamSession {
        stop,
        input,
        input_queue_drops,
        diagnostic_probe_sequence,
        thread: Some(thread),
    }
}

fn run_pipe_session() -> Result<(), Box<dyn std::error::Error>> {
    let mut commands = BufReader::new(std::io::stdin());
    let PlayerCommand::Launch(launch) = read_command(&mut commands)? else {
        return Err("first player command must launch a session".into());
    };
    let mut latency_diagnostics = LatencyDiagnostics::open(launch.latency_diagnostics)?;
    for (name, capacity) in [
        ("video_render_queue_capacity", 2),
        ("audio_playback_queue_capacity", 4),
        ("input_player_queue_capacity", 1_024),
        ("video_assembly_queue_capacity", 2),
        ("diagnostic_host_stream_queue_capacity", 1_024),
        ("diagnostic_payload_match_queue_capacity", 4_096),
        ("diagnostic_writer_queue_capacity", 4_096),
        (
            "presentation_tracker_capacity",
            PRESENTATION_TRACKER_CAPACITY as u64,
        ),
    ] {
        latency_diagnostics.counter(name, capacity);
    }
    let mut cpu_sampler = latency_diagnostics
        .enabled()
        .then(ProcessCpuSampler::new)
        .transpose()?;
    let mut events = BufWriter::new(std::io::stdout());
    let sdl = sdl3::init()?;
    let video = sdl.video()?;
    let mut window = video
        .window("Rust Console", 1280, 720)
        .position_centered()
        .resizable()
        .hidden()
        .vulkan()
        .build()
        .map_err(|error| error.to_string())?;
    set_window_icon(&mut window);
    let mut event_pump = sdl.event_pump()?;
    let audio_queue = AudioPlaybackQueue::default();
    let mut audio_output = SdlAudioOutput::new(&sdl);
    let frames = Arc::new(Mutex::new(VecDeque::with_capacity(2)));
    let video_render_queue_drops = Arc::new(AtomicU64::new(0));
    let (command_tx, command_rx) = mpsc::sync_channel(1);
    let mut overlay = StreamOverlay::new(launch.maximum_bitrate_bits_per_second);
    std::thread::spawn(move || {
        loop {
            let command = read_command(&mut commands);
            let finished = matches!(command, Ok(PlayerCommand::Stop) | Err(_));
            if command_tx.send(command).is_err() || finished {
                break;
            }
        }
    });

    let (session_tx, session_rx) = mpsc::channel();
    let mut session = start_stream_session(
        &launch,
        session_tx.clone(),
        Arc::clone(&frames),
        Arc::clone(&video_render_queue_drops),
        audio_queue.clone(),
    );

    let mut gui = PlayerGui::default();
    let gui_started = Instant::now();
    let mut pointer_routing = PointerRouting::default();
    let mut input_focus = RemoteInputFocus::new();
    let mut renderer =
        None::<Box<dyn PlayerVideoBackend<NativeDmaBufFrame, GuiFrame, Error = String>>>;
    let mut last_frame = None::<QueuedVideoFrame>;
    let mut pending_video_timestamp = None;
    let mut video_clock = None::<VideoPlaybackClock>;
    let mut authenticated = false;
    let mut started = false;
    let mut ever_streamed = false;
    let mut recovering = false;
    let mut stable_since = None::<Instant>;
    let mut retry_at = None::<Instant>;
    let mut retry_backoff = reconnect_backoff(None);
    let mut redraw = false;
    let mut loading_started = None::<Instant>;
    let mut loading_presented = Instant::now() - Duration::from_millis(17);
    let mut loading_status = "Host authenticated\nStarting video session".to_owned();
    let mut stream_result = None;
    let mut output_preference = VulkanOutputPreference::Sdr;
    let mut clock_offset = None::<rustconsole_player_core::ClockOffsetEstimate>;
    let mut input_started = BTreeMap::<u64, Instant>::new();
    let mut correlated_input_started = BTreeMap::<u64, Instant>::new();
    let mut integrity_matches = [0_u64; 2];
    let mut integrity_mismatches = [0_u64; 2];
    let mut worker_integrity_mismatches = [0_u64; 2];
    let mut pending_presentations = BTreeMap::<u64, PendingPresentation>::new();
    let mut presentation_tracker_drops = 0_u64;
    let mut last_diagnostic_frame = None;
    'running: loop {
        if let Ok(command) = command_rx.try_recv() {
            match command {
                Ok(PlayerCommand::Stop) | Err(_) => break 'running,
                Ok(PlayerCommand::Launch(_)) => {
                    stream_result = Some(Err("player received a duplicate launch".into()));
                    break 'running;
                }
                Ok(PlayerCommand::Reconnect) => {
                    session.stop();
                    while session_rx.try_recv().is_ok() {}
                    renderer = None;
                    pending_presentations.clear();
                    last_diagnostic_frame = None;
                    input_started.clear();
                    correlated_input_started.clear();
                    last_frame = None;
                    frames.lock().unwrap().clear();
                    audio_queue
                        .push(rustconsole_player_linux::DecodedAudioEvent::Reset { generation: 0 });
                    audio_output.reset();
                    overlay = StreamOverlay::new(launch.maximum_bitrate_bits_per_second);
                    pending_video_timestamp = None;
                    video_clock = None;
                    authenticated = false;
                    started = false;
                    recovering = true;
                    stable_since = None;
                    retry_at = None;
                    retry_backoff = reconnect_backoff(None);
                    redraw = false;
                    loading_started = Some(Instant::now());
                    loading_presented = Instant::now() - Duration::from_millis(17);
                    loading_status = "Reconnecting to host".to_owned();
                    stream_result = None;
                    session = start_stream_session(
                        &launch,
                        session_tx.clone(),
                        Arc::clone(&frames),
                        Arc::clone(&video_render_queue_drops),
                        audio_queue.clone(),
                    );
                }
            }
        }
        if retry_at.is_some_and(|deadline| Instant::now() >= deadline) {
            retry_at = None;
            loading_status = "Reconnecting to host".to_owned();
            redraw = true;
            session = start_stream_session(
                &launch,
                session_tx.clone(),
                Arc::clone(&frames),
                Arc::clone(&video_render_queue_drops),
                audio_queue.clone(),
            );
        }
        while let Ok(event) = session_rx.try_recv() {
            match event {
                SessionEvent::Authenticated(host_identity) => {
                    authenticated = true;
                    write_event(&mut events, &PlayerEvent::Authenticated { host_identity })?;
                    if !window.show() {
                        return Err(sdl3::get_error().into());
                    }
                    loading_started = Some(Instant::now());
                    redraw = true;
                }
                SessionEvent::Statistics(statistics) => {
                    overlay.observe(statistics);
                    redraw = true;
                }
                SessionEvent::Progress(progress) => {
                    loading_status = match progress {
                        StreamProgress::AudioTransport(snapshot) => {
                            let stopped = snapshot.stopped();
                            let changed = overlay.audio.as_ref().is_none_or(|previous| {
                                previous.host.generation != snapshot.host.generation
                                    || previous.host.status != snapshot.host.status
                            });
                            if stopped && changed {
                                audio_queue.push(
                                    rustconsole_player_linux::DecodedAudioEvent::Reset {
                                        generation: snapshot.host.generation,
                                    },
                                );
                            }
                            overlay.audio = Some(snapshot);
                            redraw = true;
                            continue;
                        }
                        StreamProgress::KeyboardLeds {
                            generation,
                            sequence,
                            mask,
                        } => {
                            eprintln!(
                                "keyboard LEDs: generation={generation} sequence={sequence} mask={mask:#04x}"
                            );
                            continue;
                        }
                        StreamProgress::ClockOffset(estimate) => {
                            if clock_offset.is_none_or(|current| {
                                estimate.uncertainty_micros < current.uncertainty_micros
                            }) {
                                clock_offset = Some(estimate);
                            }
                            latency_diagnostics.set_clock_offset(estimate);
                            continue;
                        }
                        StreamProgress::InputSent {
                            sequence,
                            occurred_at,
                            sent_at,
                            send_completed_at,
                            correlates_test_marker,
                        } => {
                            if correlates_test_marker {
                                session
                                    .diagnostic_probe_sequence
                                    .store(sequence, Ordering::Release);
                            }
                            latency_diagnostics.observe(
                                "player_input_queue",
                                u64::try_from(
                                    sent_at.saturating_duration_since(occurred_at).as_micros(),
                                )
                                .unwrap_or(u64::MAX),
                                None,
                                Some(sequence),
                                "input control-message write started",
                            );
                            latency_diagnostics.observe(
                                "player_input_control_write",
                                u64::try_from(
                                    send_completed_at
                                        .saturating_duration_since(sent_at)
                                        .as_micros(),
                                )
                                .unwrap_or(u64::MAX),
                                None,
                                Some(sequence),
                                "input control-message write completed",
                            );
                            input_started.insert(sequence, occurred_at);
                            if correlates_test_marker {
                                correlated_input_started.insert(sequence, occurred_at);
                            }
                            continue;
                        }
                        StreamProgress::InputAcknowledged {
                            sequence,
                            player_sent_at_micros,
                            player_received_at_micros,
                            host_received_at_micros,
                            host_submitted_at_micros,
                            pointer_datagrams_received,
                            pointer_updates_applied,
                            pointer_updates_ignored,
                            mouse_reports_published,
                            keyboard_reports_published,
                            reliable_transitions_received,
                            reliable_transitions_applied,
                            reliable_transitions_rejected,
                            reliable_transitions_missing,
                            reliable_transitions_duplicate_or_late,
                            release_all_transitions,
                            pointer_missing_datagrams,
                            pointer_stale_generations,
                            pointer_duplicate_or_late,
                            pointer_mode_rejections,
                            pointer_relative_baselines,
                        } => {
                            if let Some(round_trip_micros) =
                                player_received_at_micros.checked_sub(player_sent_at_micros)
                            {
                                latency_diagnostics.observe_classified(
                                    "input_ack_round_trip",
                                    round_trip_micros,
                                    None,
                                    Some(sequence),
                                    "player received host acknowledgement",
                                    "direct",
                                    Some("player-to-host network, host submission, and host-to-player acknowledgement"),
                                );
                            }
                            if let Some(duration) =
                                host_submitted_at_micros.checked_sub(host_received_at_micros)
                            {
                                latency_diagnostics.observe_classified(
                                    "host_input_receive_to_ring_signal",
                                    duration,
                                    None,
                                    Some(sequence),
                                    "virtual HID report published to shared memory and its event signaled",
                                    "direct",
                                    Some("host input validation, state update, report construction, shared-memory publication, and event signal"),
                                );
                            }
                            if let Some(offset) = clock_offset {
                                let host_received_player_clock =
                                    i128::from(host_received_at_micros)
                                        - i128::from(offset.offset_micros);
                                if let Ok(duration) = u64::try_from(
                                    host_received_player_clock - i128::from(player_sent_at_micros),
                                ) {
                                    latency_diagnostics.observe_classified(
                                        "input_player_send_to_host_receive",
                                        duration,
                                        None,
                                        Some(sequence),
                                        "host decoded the input control message",
                                        "clock-adjusted",
                                        Some("QUIC scheduling and network transit; includes clock uncertainty"),
                                    );
                                }
                                let host_submitted_player_clock =
                                    i128::from(host_submitted_at_micros)
                                        - i128::from(offset.offset_micros);
                                if let Ok(duration) = u64::try_from(
                                    i128::from(player_received_at_micros)
                                        - host_submitted_player_clock,
                                ) {
                                    latency_diagnostics.observe_classified(
                                        "input_ring_signal_to_ack_receive",
                                        duration,
                                        None,
                                        Some(sequence),
                                        "player decoded the input acknowledgement",
                                        "clock-adjusted",
                                        Some("host acknowledgement write and network transit; includes clock uncertainty"),
                                    );
                                }
                            }
                            latency_diagnostics.counter(
                                "input_pointer_datagrams_received",
                                pointer_datagrams_received,
                            );
                            latency_diagnostics
                                .counter("input_pointer_updates_applied", pointer_updates_applied);
                            latency_diagnostics
                                .counter("input_pointer_updates_ignored", pointer_updates_ignored);
                            latency_diagnostics
                                .counter("input_mouse_reports_published", mouse_reports_published);
                            latency_diagnostics.counter(
                                "input_keyboard_reports_published",
                                keyboard_reports_published,
                            );
                            for (name, value) in [
                                ("input_reliable_received", reliable_transitions_received),
                                ("input_reliable_applied", reliable_transitions_applied),
                                ("input_reliable_rejected", reliable_transitions_rejected),
                                ("input_reliable_missing", reliable_transitions_missing),
                                (
                                    "input_reliable_duplicate_or_late",
                                    reliable_transitions_duplicate_or_late,
                                ),
                                ("input_release_all", release_all_transitions),
                                ("input_pointer_missing", pointer_missing_datagrams),
                                ("input_pointer_stale_generation", pointer_stale_generations),
                                ("input_pointer_duplicate_or_late", pointer_duplicate_or_late),
                                ("input_pointer_mode_rejections", pointer_mode_rejections),
                                (
                                    "input_pointer_relative_baselines",
                                    pointer_relative_baselines,
                                ),
                            ] {
                                latency_diagnostics.counter(name, value);
                            }
                            continue;
                        }
                        StreamProgress::PayloadIntegrity(sample) => {
                            let (
                                index,
                                producer_metric,
                                host_metric,
                                player_metric,
                                matches_counter,
                                mismatches_counter,
                                worker_mismatches_counter,
                                capture_to_encode_metric,
                                encode_metric,
                                encoded_to_queue_metric,
                                queue_to_service_metric,
                                service_to_packetization_metric,
                                packetization_metric,
                                send_scheduling_metric,
                                send_span_metric,
                                send_to_assembly_metric,
                                packetization_to_assembly_metric,
                            ) = match sample.kind {
                                rustconsole_protocol::diagnostics::MediaKind::Audio => (
                                    0,
                                    "audio_worker_payload_hash",
                                    "audio_host_payload_hash",
                                    "audio_player_payload_hash",
                                    "audio_integrity_matches",
                                    "audio_integrity_mismatches",
                                    "audio_worker_service_mismatches",
                                    "audio_capture_to_encode_start",
                                    "audio_encode",
                                    "audio_encoded_to_worker_queue",
                                    "audio_worker_queue_to_service",
                                    "audio_service_to_packetization",
                                    "audio_packetization",
                                    "audio_packet_send_scheduling",
                                    "audio_packet_send_span",
                                    "audio_last_send_to_assembly",
                                    "audio_packetization_to_assembly",
                                ),
                                rustconsole_protocol::diagnostics::MediaKind::Video => (
                                    1,
                                    "video_worker_payload_hash",
                                    "video_host_payload_hash",
                                    "video_player_payload_hash",
                                    "video_integrity_matches",
                                    "video_integrity_mismatches",
                                    "video_worker_service_mismatches",
                                    "video_capture_to_encode_start",
                                    "video_encode",
                                    "video_encoded_to_worker_queue",
                                    "video_worker_queue_to_service",
                                    "video_service_to_packetization",
                                    "video_packetization",
                                    "video_packet_send_scheduling",
                                    "video_packet_send_span",
                                    "video_last_send_to_assembly",
                                    "video_packetization_to_assembly",
                                ),
                            };
                            let sequence = Some(sample.sequence);
                            if let Some(duration) = sample
                                .encode_started_at_micros
                                .checked_sub(sample.captured_at_micros)
                            {
                                latency_diagnostics.observe_classified(
                                    capture_to_encode_metric,
                                    duration,
                                    sequence,
                                    None,
                                    "encoder call started",
                                    "direct",
                                    Some("capture-to-encoder scheduling and source preparation"),
                                );
                            }
                            if let Some(duration) = sample
                                .encoded_at_micros
                                .checked_sub(sample.encode_started_at_micros)
                            {
                                latency_diagnostics.observe_classified(
                                    encode_metric,
                                    duration,
                                    sequence,
                                    None,
                                    "encoder call returned",
                                    "direct",
                                    None,
                                );
                            }
                            if let Some(duration) = sample
                                .worker_queued_at_micros
                                .checked_sub(sample.encoded_at_micros)
                            {
                                latency_diagnostics.observe_classified(
                                    encoded_to_queue_metric,
                                    duration,
                                    sequence,
                                    None,
                                    "worker event write started",
                                    "direct",
                                    Some("payload hashing and worker scheduling"),
                                );
                            }
                            if let Some(duration) = sample
                                .service_received_at_micros
                                .checked_sub(sample.worker_queued_at_micros)
                            {
                                latency_diagnostics.observe_classified(
                                    queue_to_service_metric,
                                    duration,
                                    sequence,
                                    None,
                                    "service decoded the worker event",
                                    "direct",
                                    Some("worker pipe write, pipe residence, service polling, and event decoding"),
                                );
                            }
                            if let Some(duration) = sample
                                .packetized_at_micros
                                .checked_sub(sample.service_received_at_micros)
                            {
                                latency_diagnostics.observe_classified(
                                    service_to_packetization_metric,
                                    duration,
                                    sequence,
                                    None,
                                    "media packetization started",
                                    "direct",
                                    Some("service scheduling and diagnostic boundary hashing"),
                                );
                            }
                            if let Some(duration) = sample
                                .packetization_completed_at_micros
                                .checked_sub(sample.packetized_at_micros)
                            {
                                latency_diagnostics.observe(
                                    packetization_metric,
                                    duration,
                                    sequence,
                                    None,
                                    "all media datagrams constructed",
                                );
                            }
                            if let Some(duration) = sample
                                .first_send_attempt_at_micros
                                .checked_sub(sample.packetization_completed_at_micros)
                            {
                                latency_diagnostics.observe(
                                    send_scheduling_metric,
                                    duration,
                                    sequence,
                                    None,
                                    "first bounded QUIC datagram send attempt started",
                                );
                            }
                            if let Some(duration) = sample
                                .last_send_completed_at_micros
                                .checked_sub(sample.first_send_attempt_at_micros)
                            {
                                latency_diagnostics.observe_classified(
                                    send_span_metric,
                                    duration,
                                    sequence,
                                    None,
                                    "last media datagram accepted by the QUIC sender",
                                    "direct",
                                    Some("bounded send retries and all frame or packet datagrams"),
                                );
                            }
                            if let Some(offset) = clock_offset {
                                let packetized_player_clock =
                                    i128::from(sample.packetized_at_micros)
                                        - i128::from(offset.offset_micros);
                                if let Ok(duration) = u64::try_from(
                                    i128::from(sample.assembled_at_micros)
                                        - packetized_player_clock,
                                ) {
                                    latency_diagnostics.observe_classified(
                                        packetization_to_assembly_metric,
                                        duration,
                                        sequence,
                                        None,
                                        "complete media payload assembled on the player",
                                        "clock-adjusted",
                                        Some("host packetization, QUIC scheduling, network transit, and player assembly; includes clock uncertainty"),
                                    );
                                }
                                let last_send_player_clock =
                                    i128::from(sample.last_send_completed_at_micros)
                                        - i128::from(offset.offset_micros);
                                if sample.last_send_completed_at_micros != 0
                                    && let Ok(duration) = u64::try_from(
                                        i128::from(sample.assembled_at_micros)
                                            - last_send_player_clock,
                                    )
                                {
                                    latency_diagnostics.observe_classified(
                                        send_to_assembly_metric,
                                        duration,
                                        sequence,
                                        None,
                                        "complete media payload assembled on the player",
                                        "clock-adjusted",
                                        Some("network tail and player assembly after the host completed all sends; includes clock uncertainty"),
                                    );
                                }
                            }
                            if sample.kind == rustconsole_protocol::diagnostics::MediaKind::Video {
                                for (metric, duration, endpoint) in [
                                    (
                                        "video_capture_wait_and_acquisition",
                                        sample.capture_acquisition_micros,
                                        "captured source texture acquired",
                                    ),
                                    (
                                        "video_cross_adapter_copy",
                                        sample.cross_adapter_copy_micros,
                                        "Intel source copied into NVIDIA-local texture",
                                    ),
                                    (
                                        "video_color_conversion",
                                        sample.color_conversion_micros,
                                        "NVIDIA-local source converted to encoder format",
                                    ),
                                    (
                                        "video_encoder_call",
                                        sample.encoder_call_micros,
                                        "NVENC texture submission returned",
                                    ),
                                ] {
                                    latency_diagnostics
                                        .observe(metric, duration, sequence, None, endpoint);
                                }
                                if sample.mirror_decode_micros != 0 {
                                    latency_diagnostics.observe_classified(
                                        "video_host_mirror_decode",
                                        sample.mirror_decode_micros,
                                        sequence,
                                        None,
                                        "D3D11VA mirror decoder returned or requested another packet",
                                        "direct",
                                        Some("host worker critical path while full diagnostics is enabled"),
                                    );
                                }
                            }
                            if sample.kind == rustconsole_protocol::diagnostics::MediaKind::Audio {
                                latency_diagnostics.observe_measurement(
                                    "audio_capture_buffer_frames",
                                    sample.audio_capture_buffer_frames,
                                    "frames",
                                    sequence,
                                );
                                latency_diagnostics.observe_measurement(
                                    "audio_capture_queue_depth",
                                    sample.audio_capture_queue_depth,
                                    "packets",
                                    sequence,
                                );
                                for (name, value) in [
                                    (
                                        "audio_capture_discontinuities",
                                        sample.audio_capture_discontinuities,
                                    ),
                                    (
                                        "audio_invalid_capture_timestamps",
                                        sample.audio_invalid_capture_timestamps,
                                    ),
                                    ("audio_device_reopens", sample.audio_device_reopens),
                                    ("audio_encoder_resets", sample.audio_encoder_resets),
                                    (
                                        "audio_capture_queue_capacity",
                                        sample.audio_capture_queue_capacity,
                                    ),
                                    (
                                        "audio_capture_queue_drops",
                                        sample.audio_capture_queue_drops,
                                    ),
                                ] {
                                    latency_diagnostics.counter(name, value);
                                }
                            }
                            if sample.quality_present {
                                let quality_sequence =
                                    u64::try_from(sample.quality_presentation_timestamp).ok();
                                latency_diagnostics.observe_classified(
                                    "video_quality_source_readback",
                                    sample.source_readback_micros,
                                    quality_sequence,
                                    None,
                                    "source luma copied from the encoder input texture",
                                    "direct",
                                    Some("host worker critical path at 5 Hz"),
                                );
                                latency_diagnostics.observe_classified(
                                    "video_quality_decoded_readback",
                                    sample.decoded_readback_micros,
                                    quality_sequence,
                                    None,
                                    "mirror-decoded luma transferred from D3D11VA",
                                    "direct",
                                    Some("host worker critical path at 5 Hz"),
                                );
                                latency_diagnostics.observe_classified(
                                    "video_quality_scoring",
                                    sample.scoring_micros,
                                    quality_sequence,
                                    None,
                                    "source and decoded luma scores completed",
                                    "direct",
                                    Some("host worker critical path at 5 Hz"),
                                );
                                latency_diagnostics.observe_measurement(
                                    "video_quality_readback_bytes",
                                    sample.readback_bytes,
                                    "bytes",
                                    quality_sequence,
                                );
                                latency_diagnostics.observe_measurement_classified(
                                    "video_luma_psnr",
                                    sample.luma_psnr_millidecibels,
                                    "millidecibels",
                                    quality_sequence,
                                    "derived",
                                    Some("host source and D3D11VA mirror-decoded luma at 5 Hz"),
                                );
                                latency_diagnostics.observe_measurement_classified(
                                    "video_luma_mean_absolute_error",
                                    sample.luma_mean_absolute_error_ppm,
                                    "parts-per-million-of-range",
                                    quality_sequence,
                                    "derived",
                                    Some("host source and D3D11VA mirror-decoded luma at 5 Hz"),
                                );
                            }
                            latency_diagnostics.observe_measurement(
                                producer_metric,
                                sample.producer_hash_duration_micros,
                                "microseconds",
                                sequence,
                            );
                            latency_diagnostics.observe_measurement(
                                host_metric,
                                sample.host_hash_duration_micros,
                                "microseconds",
                                sequence,
                            );
                            latency_diagnostics.observe_measurement(
                                player_metric,
                                sample.player_hash_duration_micros,
                                "microseconds",
                                sequence,
                            );
                            if sample.matched {
                                integrity_matches[index] += 1;
                            } else {
                                integrity_mismatches[index] += 1;
                            }
                            if !sample.worker_to_service_matched {
                                worker_integrity_mismatches[index] += 1;
                            }
                            latency_diagnostics.counter(matches_counter, integrity_matches[index]);
                            latency_diagnostics
                                .counter(mismatches_counter, integrity_mismatches[index]);
                            latency_diagnostics.counter(
                                worker_mismatches_counter,
                                worker_integrity_mismatches[index],
                            );
                            latency_diagnostics.counter(
                                "host_diagnostic_queue_drops",
                                sample.host_dropped_records,
                            );
                            continue;
                        }
                        StreamProgress::PayloadIntegrityCounters(counters) => {
                            latency_diagnostics.counter(
                                "integrity_pending_host_records",
                                counters.pending_host_records,
                            );
                            latency_diagnostics.counter(
                                "integrity_pending_player_payloads",
                                counters.pending_player_payloads,
                            );
                            latency_diagnostics.counter(
                                "integrity_duplicate_host_records",
                                counters.duplicate_host_records,
                            );
                            latency_diagnostics.counter(
                                "integrity_duplicate_player_payloads",
                                counters.duplicate_player_payloads,
                            );
                            latency_diagnostics.counter(
                                "integrity_unmatched_host_records",
                                counters.unmatched_host_records,
                            );
                            latency_diagnostics.counter(
                                "integrity_unmatched_player_payloads",
                                counters.unmatched_player_payloads,
                            );
                            continue;
                        }
                        StreamProgress::DiagnosticQueues(queues) => {
                            for (name, value) in [
                                ("video_assembly_queue_depth", queues.video_depth),
                                ("audio_receiver_queue_depth", queues.audio_depth),
                                ("diagnostic_event_queue_depth", queues.event_depth),
                                ("diagnostic_payload_queue_depth", queues.digest_depth),
                            ] {
                                latency_diagnostics
                                    .observe_measurement(name, value, "records", None);
                            }
                            for (name, value) in [
                                ("video_assembly_queue_drops", queues.video_drops),
                                ("audio_receiver_queue_drops", queues.audio_drops),
                                ("diagnostic_event_queue_drops", queues.event_drops),
                                ("diagnostic_payload_queue_drops", queues.digest_drops),
                                ("video_keyframe_requests", queues.keyframe_requests),
                            ] {
                                latency_diagnostics.counter(name, value);
                            }
                            continue;
                        }
                        StreamProgress::KeyframeRecovered { duration } => {
                            latency_diagnostics.increment_counter("video_keyframe_recoveries");
                            latency_diagnostics.observe(
                                "video_keyframe_recovery",
                                u64::try_from(duration.as_micros()).unwrap_or(u64::MAX),
                                None,
                                None,
                                "first accepted recovery keyframe",
                            );
                            continue;
                        }
                        StreamProgress::NegotiatingVideo => {
                            "Host authenticated\nNegotiating video".to_owned()
                        }
                        StreamProgress::VideoNegotiated(configuration) => {
                            output_preference = match configuration.mode.bit_depth {
                                rustconsole_protocol::VideoBitDepth::Eight => {
                                    VulkanOutputPreference::Sdr
                                }
                                rustconsole_protocol::VideoBitDepth::Ten => {
                                    VulkanOutputPreference::Hdr10IfAvailable
                                }
                            };
                            renderer = None;
                            format!(
                                "Video negotiated\n{}-bit YUV 4:2:0",
                                match configuration.mode.bit_depth {
                                    rustconsole_protocol::VideoBitDepth::Eight => 8,
                                    rustconsole_protocol::VideoBitDepth::Ten => 10,
                                }
                            )
                        }
                        StreamProgress::WaitingForVideoPackets => {
                            "Video negotiated\nWaiting for host packets".to_owned()
                        }
                        StreamProgress::ReceivingVideoPackets { frame, totals } => {
                            for (name, value) in [
                                ("video_received_chunks", totals.received_chunks),
                                ("video_lost_chunks", totals.lost_chunks),
                                ("video_late_chunks", totals.late_chunks),
                                ("video_assembly_overflows", totals.assembly_overflows),
                                ("video_completed_frames", totals.completed_frames),
                                ("video_incomplete_frames", totals.incomplete_frames),
                                (
                                    "video_completed_payload_bytes",
                                    totals.completed_payload_bytes,
                                ),
                            ] {
                                latency_diagnostics.counter(name, value);
                            }
                            format!(
                                "Receiving host packets\nFrame {}  {}/{} chunks  {:.1} KiB\nAssembly {:.1}/{:.1} ms\nComplete {}  Incomplete {}  Overflow {}\nEstimated capacity {:.2} Mbit/s\nTarget {:.2} / Maximum {:.0} Mbit/s",
                                frame.sequence,
                                frame.received_chunks,
                                frame.expected_chunks,
                                frame.frame_size as f64 / 1024.0,
                                frame.elapsed.as_secs_f64() * 1_000.0,
                                frame.budget.as_secs_f64() * 1_000.0,
                                totals.completed_frames,
                                totals.incomplete_frames,
                                totals.assembly_overflows,
                                frame.estimated_capacity_bits_per_second as f64 / 1_000_000.0,
                                frame.target_bitrate_bits_per_second as f64 / 1_000_000.0,
                                launch.maximum_bitrate_bits_per_second as f64 / 1_000_000.0,
                            )
                        }
                        StreamProgress::FirstFrameAssembled {
                            target_bitrate_bits_per_second,
                            estimated_capacity_bits_per_second,
                        } => {
                            format!(
                                "First frame assembled\nDecoding video\nEstimated capacity {:.2} Mbit/s\nTarget {:.2} / Maximum {:.0} Mbit/s",
                                estimated_capacity_bits_per_second as f64 / 1_000_000.0,
                                target_bitrate_bits_per_second as f64 / 1_000_000.0,
                                launch.maximum_bitrate_bits_per_second as f64 / 1_000_000.0,
                            )
                        }
                    };
                    redraw = true;
                }
                SessionEvent::Ended(result)
                    if should_retry_session_end(ever_streamed, result.is_err()) =>
                {
                    let error = result.expect_err("retryable session end must contain an error");
                    session.stop();
                    while session_rx.try_recv().is_ok() {}
                    renderer = None;
                    pending_presentations.clear();
                    last_diagnostic_frame = None;
                    input_started.clear();
                    correlated_input_started.clear();
                    last_frame = None;
                    frames.lock().unwrap().clear();
                    audio_queue
                        .push(rustconsole_player_linux::DecodedAudioEvent::Reset { generation: 0 });
                    audio_output.reset();
                    overlay = StreamOverlay::new(launch.maximum_bitrate_bits_per_second);
                    pending_video_timestamp = None;
                    video_clock = None;
                    authenticated = false;
                    started = false;
                    recovering = true;
                    stable_since = None;
                    redraw = true;
                    loading_started = Some(Instant::now());
                    loading_presented = Instant::now() - Duration::from_millis(17);
                    stream_result = None;
                    let delay = retry_backoff
                        .next()
                        .expect("unlimited reconnect backoff must yield a delay");
                    retry_at = Some(Instant::now() + delay);
                    loading_status = format!(
                        "Connection lost\nRetrying in {:.1} seconds",
                        delay.as_secs_f32()
                    );
                    eprintln!(
                        "stream session failed: {error}; reconnecting in {:.3} seconds",
                        delay.as_secs_f64()
                    );
                }
                SessionEvent::Ended(result) => {
                    stream_result = Some(result);
                    break 'running;
                }
                SessionEvent::Reconfigure(cause) => {
                    eprintln!("video reconfiguration requested: {cause:?}");
                    session.stop();
                    while session_rx.try_recv().is_ok() {}
                    renderer = None;
                    pending_presentations.clear();
                    last_diagnostic_frame = None;
                    input_started.clear();
                    correlated_input_started.clear();
                    last_frame = None;
                    frames.lock().unwrap().clear();
                    audio_queue
                        .push(rustconsole_player_linux::DecodedAudioEvent::Reset { generation: 0 });
                    audio_output.reset();
                    overlay = StreamOverlay::new(launch.maximum_bitrate_bits_per_second);
                    pending_video_timestamp = None;
                    video_clock = None;
                    authenticated = false;
                    started = false;
                    recovering = true;
                    stable_since = None;
                    retry_at = None;
                    retry_backoff = reconnect_backoff(None);
                    redraw = false;
                    loading_started = Some(Instant::now());
                    loading_presented = Instant::now() - Duration::from_millis(17);
                    loading_status = "Host video changed\nReconnecting".to_owned();
                    stream_result = None;
                    session = start_stream_session(
                        &launch,
                        session_tx.clone(),
                        Arc::clone(&frames),
                        Arc::clone(&video_render_queue_drops),
                        audio_queue.clone(),
                    );
                }
            }
        }
        for event in event_pump.poll_iter() {
            match event {
                Event::Quit { .. } => break 'running,
                Event::KeyDown {
                    scancode: Some(scancode),
                    repeat: false,
                    ..
                } if input_focus.accepts() => {
                    let usage = scancode as u32;
                    if let Ok(hid_usage) = u16::try_from(usage) {
                        session.send_input(InputEvent::Key {
                            hid_usage,
                            pressed: true,
                        });
                    }
                }
                Event::KeyUp {
                    scancode: Some(scancode),
                    ..
                } if input_focus.accepts() => {
                    let usage = scancode as u32;
                    if let Ok(hid_usage) = u16::try_from(usage) {
                        session.send_input(InputEvent::Key {
                            hid_usage,
                            pressed: false,
                        });
                    }
                }
                Event::MouseButtonDown {
                    mouse_btn, x, y, ..
                } if input_focus.accepts() => {
                    gui.pointer_moved(x, y);
                    if let Some(gui_button) = gui_pointer_button(mouse_btn) {
                        let captured = gui.captures_pointer_at(x, y);
                        gui.pointer_button(x, y, gui_button, true);
                        redraw = true;
                        let Some(button) = mouse_button(mouse_btn) else {
                            continue;
                        };
                        if pointer_routing.press(button, captured) {
                            session.send_input(InputEvent::PointerButton {
                                button,
                                pressed: true,
                            });
                        }
                    } else if let Some(button) = mouse_button(mouse_btn) {
                        session.send_input(InputEvent::PointerButton {
                            button,
                            pressed: true,
                        });
                    }
                }
                Event::MouseButtonUp {
                    mouse_btn, x, y, ..
                } if input_focus.accepts() => {
                    gui.pointer_moved(x, y);
                    if let Some(gui_button) = gui_pointer_button(mouse_btn) {
                        gui.pointer_button(x, y, gui_button, false);
                        redraw = true;
                    }
                    if let Some(button) = mouse_button(mouse_btn)
                        && pointer_routing.release(button)
                    {
                        session.send_input(InputEvent::PointerButton {
                            button,
                            pressed: false,
                        });
                    }
                }
                Event::MouseMotion { x, y, .. } if input_focus.accepts() => {
                    gui.pointer_moved(x, y);
                    redraw = true;
                    let (width, height) = window.size();
                    if !gui.captures_pointer_at(x, y)
                        && width > 1
                        && height > 1
                        && x >= 0.0
                        && y >= 0.0
                    {
                        let x = ((x * 32767.0) / (width - 1) as f32).clamp(0.0, 32767.0) as u16;
                        let y = ((y * 32767.0) / (height - 1) as f32).clamp(0.0, 32767.0) as u16;
                        session.try_send_input(InputEvent::PointerPosition { x, y });
                    }
                }
                Event::MouseWheel {
                    x,
                    y,
                    mouse_x,
                    mouse_y,
                    ..
                } if input_focus.accepts() => {
                    gui.mouse_wheel(x, y);
                    redraw = true;
                    if gui.captures_pointer_at(mouse_x, mouse_y) {
                        continue;
                    }
                    let horizontal = x.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                    let vertical = y.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                    if horizontal != 0 || vertical != 0 {
                        session.send_input(InputEvent::Wheel {
                            horizontal,
                            vertical,
                        });
                    }
                }
                Event::Window {
                    win_event: WindowEvent::PixelSizeChanged(_, _),
                    ..
                } => redraw = true,
                Event::Window {
                    win_event: WindowEvent::MouseLeave,
                    ..
                } => {
                    gui.pointer_gone();
                    redraw = true;
                }
                Event::Window {
                    win_event: WindowEvent::FocusGained,
                    ..
                } => {
                    input_focus.gain();
                    gui.set_focused(true);
                    redraw = true;
                }
                Event::Window {
                    win_event: WindowEvent::FocusLost,
                    ..
                } => {
                    input_focus.lose();
                    session.send_input(InputEvent::ReleaseAll);
                    pointer_routing.release_all();
                    gui.set_focused(false);
                    redraw = true;
                }
                _ => {}
            }
        }
        let (latest, render_queue_depth) = {
            let mut frames = frames.lock().unwrap();
            let render_queue_depth = frames.len();
            let latest = frames.pop_back();
            video_render_queue_drops.fetch_add(frames.len() as u64, Ordering::Relaxed);
            frames.clear();
            (latest, render_queue_depth)
        };
        latency_diagnostics.counter(
            "video_render_queue_drops",
            video_render_queue_drops.load(Ordering::Relaxed),
        );
        if let Some(mut frame) = latest {
            frame.capture_player_at = clock_offset.and_then(|offset| {
                let captured_player_micros =
                    i128::from(frame.captured_at_micros) - i128::from(offset.offset_micros);
                u64::try_from(i128::from(frame.assembled_at_micros) - captured_player_micros)
                    .ok()
                    .and_then(|duration| {
                        frame.assembled_at.and_then(|assembled_at| {
                            assembled_at.checked_sub(Duration::from_micros(duration))
                        })
                    })
            });
            pending_video_timestamp = Some(frame.captured_at_micros);
            if let Some(duration) = frame.diagnostic_copy_duration {
                latency_diagnostics.observe_full_frame_copy(
                    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX),
                    frame.diagnostic_copy_bytes,
                    frame.sequence,
                );
            }
            latency_diagnostics.observe(
                "frame_assembly",
                u64::try_from(frame.assembly_duration.as_micros()).unwrap_or(u64::MAX),
                Some(frame.sequence),
                None,
                "complete encoded frame assembled",
            );
            if let Some(duration) = frame.decoder_queue_duration {
                latency_diagnostics.observe(
                    "video_decoder_queue",
                    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX),
                    Some(frame.sequence),
                    None,
                    "VA-API decode started",
                );
            }
            latency_diagnostics.observe(
                "decode",
                u64::try_from(frame.decode_duration.as_micros()).unwrap_or(u64::MAX),
                Some(frame.sequence),
                None,
                "VA-API decode returned a frame",
            );
            if let Some(duration) = frame.dma_buf_export_duration {
                latency_diagnostics.observe(
                    "video_dma_buf_export",
                    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX),
                    Some(frame.sequence),
                    None,
                    "FFmpeg exported the decoded hardware frame as a DMA-BUF descriptor",
                );
            }
            if let Some(duration) = frame.decode_to_dma_buf_export_duration {
                latency_diagnostics.observe_classified(
                    "video_decode_to_dma_buf_export_start",
                    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX),
                    Some(frame.sequence),
                    None,
                    "FFmpeg DMA-BUF export started",
                    "direct",
                    Some("post-decode validation and diagnostic marker probing when requested"),
                );
            }
            latency_diagnostics.observe_measurement(
                "video_encoded_frame_size",
                frame.encoded_frame_bytes as u64,
                "bytes",
                Some(frame.sequence),
            );
            latency_diagnostics.observe_measurement(
                "video_decoded_dma_buf_size",
                frame.decoded_dma_buf_bytes as u64,
                "bytes",
                Some(frame.sequence),
            );
            latency_diagnostics.observe_measurement(
                "video_render_queue_depth",
                render_queue_depth as u64,
                "frames",
                Some(frame.sequence),
            );
            if frame.assembly_to_decoder_matched.is_some() {
                latency_diagnostics.observe_measurement(
                    "video_decoder_input_hash",
                    u64::try_from(frame.decoder_input_hash_duration.as_micros())
                        .unwrap_or(u64::MAX),
                    "microseconds",
                    Some(frame.sequence),
                );
                if frame.assembly_to_decoder_matched == Some(false) {
                    latency_diagnostics.increment_counter("video_assembly_decoder_mismatches");
                }
            }
            if let Some(duration) = frame
                .encoded_at_micros
                .checked_sub(frame.captured_at_micros)
            {
                latency_diagnostics.observe_classified(
                    "host_capture_to_encoded_event",
                    duration,
                    Some(frame.sequence),
                    None,
                    "host service received the encoded worker frame",
                    "direct",
                    Some("capture, encoding, worker transfer, and service queue residence"),
                );
            }
            if let Some(duration) = frame
                .packetized_at_micros
                .checked_sub(frame.encoded_at_micros)
            {
                latency_diagnostics.observe(
                    "host_packetization_setup",
                    duration,
                    Some(frame.sequence),
                    None,
                    "host packetization started",
                );
            }
            if let Some(offset) = clock_offset {
                let captured_player_clock =
                    i128::from(frame.captured_at_micros) - i128::from(offset.offset_micros);
                let decoded_player_clock = i128::from(frame.assembled_at_micros)
                    + i128::try_from(frame.decode_duration.as_micros()).unwrap_or(i128::MAX);
                if let Ok(duration) = u64::try_from(decoded_player_clock - captured_player_clock) {
                    latency_diagnostics.observe_classified(
                        "capture_to_decode",
                        duration,
                        Some(frame.sequence),
                        None,
                        "VA-API decode completion; includes clock uncertainty",
                        "clock-adjusted",
                        Some("all host, network, assembly, and decode stages"),
                    );
                }
            }
            last_frame = Some(frame);
            redraw = true;
        }
        if (authenticated || recovering)
            && last_frame.is_none()
            && loading_presented.elapsed() >= Duration::from_millis(16)
        {
            redraw = true;
        }
        if (authenticated || recovering) && redraw {
            if renderer.is_none() {
                gui.reset_renderer();
                renderer = Some(Box::new(VulkanRenderer::new(
                    &window,
                    DmaBufFrameImporter,
                    output_preference,
                    gui.context(),
                )?));
            }
            let (logical_width, logical_height) = window.size();
            let (width, height) = window.size_in_pixels();
            gui.update_viewport(
                logical_width as f32,
                logical_height as f32,
                width as f32 / logical_width.max(1) as f32,
                gui_started.elapsed().as_secs_f64(),
            );
            let fullscreen = window.fullscreen_state() != FullscreenType::Off;
            let diagnostics = if gui.diagnostics_visible() {
                let mut text = overlay.text("Streaming");
                if latency_diagnostics.enabled() {
                    text.push('\n');
                    text.push_str(&latency_diagnostics.overlay_text());
                }
                text
            } else {
                String::new()
            };
            redraw = false;
            if let Some(frame) = &last_frame {
                let diagnose_submission = last_diagnostic_frame != Some(frame.sequence);
                let (gui_frame, gui_action) = gui.frame(PlayerGuiView {
                    fullscreen,
                    status: None,
                    diagnostics: &diagnostics,
                });
                if latency_diagnostics.enabled() {
                    latency_diagnostics.observe(
                        "video_render_queue",
                        u64::try_from(frame.queued_at.elapsed().as_micros()).unwrap_or(u64::MAX),
                        Some(frame.sequence),
                        None,
                        "Vulkan renderer started processing the decoded frame",
                    );
                }
                let submission = renderer.as_mut().unwrap().present_frame(
                    &frame.frame,
                    match frame.color {
                        Av1ColorDescription::Bt709Limited => DecodedVideoColor::Bt709Limited,
                        Av1ColorDescription::Bt2020PqLimited => DecodedVideoColor::Bt2020PqLimited,
                    },
                    width.max(1),
                    height.max(1),
                    latency_diagnostics.enabled(),
                    gui_frame,
                )?;
                if let Some(render) = submission.diagnostics {
                    for (metric, duration, endpoint) in [
                        (
                            "video_renderer_pre_import",
                            render.pre_import,
                            "Vulkan renderer began native-frame import",
                        ),
                        (
                            "video_native_frame_import",
                            render.native_frame_import,
                            "Vulkan images and memory imported the DMA-BUF frame",
                        ),
                        (
                            "video_render_command_preparation",
                            render.command_preparation,
                            "Vulkan frame commands were recorded",
                        ),
                        (
                            "video_graphics_queue_submission",
                            render.queue_submission,
                            "vkQueueSubmit returned",
                        ),
                        (
                            "video_presentation_queueing",
                            render.presentation_queueing,
                            "vkQueuePresentKHR returned",
                        ),
                    ] {
                        latency_diagnostics.observe(
                            metric,
                            u64::try_from(duration.as_micros()).unwrap_or(u64::MAX),
                            Some(frame.sequence),
                            None,
                            endpoint,
                        );
                    }
                }
                let presented_at = Instant::now();
                if submission.feedback_available && diagnose_submission {
                    while pending_presentations.len() >= PRESENTATION_TRACKER_CAPACITY {
                        pending_presentations.pop_first();
                        presentation_tracker_drops = presentation_tracker_drops.saturating_add(1);
                    }
                    pending_presentations.insert(
                        submission.id,
                        PendingPresentation {
                            frame_sequence: frame.sequence,
                            frame_queued_at: frame.queued_at,
                            capture_player_at: frame.capture_player_at,
                            input_sequence: frame.input_sequence,
                            input_started_at: input_started.get(&frame.input_sequence).copied(),
                            diagnostic_marker_input_sequence: frame
                                .diagnostic_marker_input_sequence,
                            correlated_input_started_at: frame
                                .diagnostic_marker_input_sequence
                                .and_then(|sequence| {
                                    correlated_input_started.get(&sequence).copied()
                                }),
                        },
                    );
                } else if diagnose_submission {
                    observe_presented_frame(
                        &mut latency_diagnostics,
                        frame.sequence,
                        frame.queued_at,
                        frame.capture_player_at,
                        frame.input_sequence,
                        input_started.remove(&frame.input_sequence),
                        frame.diagnostic_marker_input_sequence,
                        frame
                            .diagnostic_marker_input_sequence
                            .and_then(|sequence| correlated_input_started.remove(&sequence)),
                        presented_at,
                        "Vulkan queue_present returned; compositor feedback unavailable",
                    );
                    input_started.retain(|sequence, _| *sequence > frame.input_sequence);
                    if frame.diagnostic_marker_input_sequence.is_some() {
                        correlated_input_started
                            .retain(|sequence, _| *sequence > frame.input_sequence);
                    }
                }
                if diagnose_submission {
                    last_diagnostic_frame = Some(frame.sequence);
                }
                if let Some(captured_at_micros) = pending_video_timestamp.take() {
                    video_clock = Some(VideoPlaybackClock {
                        captured_at_micros,
                        presented_at: Instant::now(),
                    });
                }
                if !started {
                    started = true;
                    ever_streamed = true;
                    stable_since = Some(Instant::now());
                    write_event(&mut events, &PlayerEvent::Started)?;
                }
                if let Some(action) = gui_action {
                    apply_gui_action(action, &mut window);
                    redraw = true;
                }
            } else {
                let elapsed_seconds = loading_started.unwrap().elapsed().as_secs_f32();
                let loading_overlay = format!(
                    "{RENDERING_BACKEND_LABEL}\n{loading_status}\n{}\nWaiting {:.1} seconds",
                    overlay.audio_text(),
                    elapsed_seconds,
                );
                let (gui_frame, gui_action) = gui.frame(PlayerGuiView {
                    fullscreen,
                    status: Some(&loading_overlay),
                    diagnostics: &diagnostics,
                });
                renderer.as_mut().unwrap().present_loading(
                    width.max(1),
                    height.max(1),
                    elapsed_seconds,
                    gui_frame,
                )?;
                loading_presented = Instant::now();
                if let Some(action) = gui_action {
                    apply_gui_action(action, &mut window);
                    redraw = true;
                }
            }
            redraw |= gui.context().has_requested_repaint();
        }
        if let Some(renderer) = renderer.as_mut() {
            while let Some(feedback) = renderer.take_presentation_feedback() {
                if let Some(pending) = pending_presentations.remove(&feedback.id) {
                    observe_presented_frame(
                        &mut latency_diagnostics,
                        pending.frame_sequence,
                        pending.frame_queued_at,
                        pending.capture_player_at,
                        pending.input_sequence,
                        pending.input_started_at,
                        pending.diagnostic_marker_input_sequence,
                        pending.correlated_input_started_at,
                        feedback.presented_at,
                        "VK_KHR_present_wait reported compositor presentation completion",
                    );
                    if pending.input_sequence != 0 {
                        input_started.retain(|sequence, _| *sequence > pending.input_sequence);
                        if pending.diagnostic_marker_input_sequence.is_some() {
                            correlated_input_started
                                .retain(|sequence, _| *sequence > pending.input_sequence);
                        }
                    }
                }
            }
        }
        if should_reset_reconnect_backoff(recovering, stable_since.map(|started| started.elapsed()))
        {
            retry_backoff = reconnect_backoff(None);
            recovering = false;
            stable_since = None;
            eprintln!("stream stable for 30 seconds; reconnect backoff reset");
        }
        if audio_queue.take_reset().is_some() {
            audio_output.reset();
        }
        if overlay
            .audio
            .as_ref()
            .is_some_and(|audio| audio.negotiated())
        {
            audio_output.poll();
            let video_timestamp_micros = video_clock
                .as_ref()
                .map(|clock| clock.timestamp_at(Instant::now()));
            while let Some(decision) = audio_queue.pop_for_video(video_timestamp_micros) {
                match decision {
                    AudioPlaybackDecision::Samples {
                        decoded,
                        queue_duration,
                        sync_hold_duration,
                    } => {
                        audio_output.play(&decoded.samples);
                        observe_audio_samples(
                            &mut latency_diagnostics,
                            &decoded,
                            video_timestamp_micros,
                            clock_offset,
                            u64::from(audio_output.queued_micros()),
                            queue_duration,
                            sync_hold_duration,
                        );
                    }
                    AudioPlaybackDecision::Late {
                        sequence,
                        queue_duration,
                        lateness_micros,
                    } => {
                        if let Some(duration) = queue_duration {
                            latency_diagnostics.observe(
                                "audio_late_drop_queue_residence",
                                u64::try_from(duration.as_micros()).unwrap_or(u64::MAX),
                                None,
                                Some(sequence),
                                "late decoded packet removed from the playback queue",
                            );
                        }
                        latency_diagnostics.observe_classified(
                            "audio_video_sync_late_drop",
                            lateness_micros,
                            None,
                            Some(sequence),
                            "audio timestamp compared with the current video clock",
                            "derived",
                            None,
                        );
                    }
                }
            }
        }
        overlay.audio_playback = audio_output.snapshot(&audio_queue);
        latency_diagnostics.observe_measurement(
            "audio_playback_queue_depth",
            overlay.audio_playback.pending_packets as u64,
            "packets",
            None,
        );
        latency_diagnostics.observe_measurement(
            "presentation_tracker_depth",
            pending_presentations.len() as u64,
            "frames",
            None,
        );
        latency_diagnostics.counter("presentation_tracker_drops", presentation_tracker_drops);
        observe_audio_counters(&mut latency_diagnostics, &overlay);
        latency_diagnostics.counter(
            "input_player_queue_rejections",
            session.input_queue_drops.load(Ordering::Relaxed),
        );
        if let Some(load) = cpu_sampler
            .as_mut()
            .and_then(|sampler| sampler.sample().ok().flatten())
        {
            latency_diagnostics.observe_measurement_classified(
                "player_process_cpu_load",
                load,
                "basis-points-of-one-logical-CPU",
                None,
                "direct",
                Some("user and kernel CPU time consumed by the player process"),
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    session.stop();
    drop(renderer);
    drop(last_frame);
    if let Some(path) = latency_diagnostics.finish()? {
        eprintln!("latency diagnostic summary: {}", path.display());
    }
    match stream_result.unwrap_or(Ok(())) {
        Ok(()) => write_event(&mut events, &PlayerEvent::Ended)?,
        Err(error) => write_event(&mut events, &PlayerEvent::Error(error))?,
    }
    Ok(())
}

fn observe_audio_counters(diagnostics: &mut LatencyDiagnostics, overlay: &StreamOverlay) {
    let playback = &overlay.audio_playback;
    diagnostics.counter("audio_playback_queue_drops", playback.queue_drops);
    diagnostics.counter("audio_playback_late_drops", playback.late_drops);
    diagnostics.counter("audio_output_total_drops", playback.device_drops);
    diagnostics.counter("audio_invalid_format_drops", playback.invalid_format_drops);
    diagnostics.counter(
        "audio_unavailable_device_drops",
        playback.unavailable_device_drops,
    );
    diagnostics.counter(
        "audio_device_query_failures",
        playback.device_query_failures,
    );
    diagnostics.counter(
        "audio_software_capacity_rejections",
        playback.software_capacity_drops,
    );
    diagnostics.counter(
        "audio_device_submission_failures",
        playback.device_submission_failures,
    );
    diagnostics.counter("audio_underruns", playback.underruns);
    diagnostics.counter("audio_resets", playback.resets);
    diagnostics.counter("audio_decoder_failures", playback.decoder_failures);
    let Some(audio) = overlay.audio.as_ref() else {
        return;
    };
    diagnostics.counter("audio_host_drops", audio.host.dropped_packets);
    diagnostics.counter("audio_missing_packets", audio.receive.missing_packets);
    diagnostics.counter("audio_expired_packets", audio.receive.expired_packets);
    diagnostics.counter("audio_overflow_packets", audio.receive.overflow_packets);
    diagnostics.counter("audio_late_fragments", audio.receive.late_fragments);
    diagnostics.counter(
        "audio_duplicate_fragments",
        audio.receive.duplicate_fragments,
    );
    diagnostics.counter(
        "audio_malformed_fragments",
        audio.receive.malformed_fragments,
    );
    diagnostics.counter("audio_video_queue_drops", audio.video_queue_drops);
    diagnostics.counter("audio_event_queue_drops", audio.audio_queue_drops);
}

fn observe_audio_samples(
    diagnostics: &mut LatencyDiagnostics,
    decoded: &DecodedAudioSamples,
    video_timestamp_micros: Option<u64>,
    clock_offset: Option<rustconsole_player_core::ClockOffsetEstimate>,
    sdl_queued_micros: u64,
    playback_queue_duration: Option<Duration>,
    sync_hold_duration: Option<Duration>,
) {
    if !diagnostics.enabled() {
        return;
    }
    let sequence = Some(decoded.sequence);
    let assembly_to_decode = decoded
        .decoded_at
        .saturating_duration_since(decoded.assembled_at);
    let decoded_queue = Instant::now().saturating_duration_since(decoded.decoded_at);
    let assembly_to_decode_micros =
        u64::try_from(assembly_to_decode.as_micros()).unwrap_or(u64::MAX);
    let decoded_queue_micros = u64::try_from(decoded_queue.as_micros()).unwrap_or(u64::MAX);
    diagnostics.observe_classified(
        "audio_assembly_to_decode",
        assembly_to_decode_micros,
        None,
        sequence,
        "Opus packet decoded",
        "direct",
        Some("decoder queue residence and Opus decode"),
    );
    if let Some(duration) = decoded.ordered_playout_duration {
        diagnostics.observe(
            "audio_ordered_playout",
            u64::try_from(duration.as_micros()).unwrap_or(u64::MAX),
            None,
            sequence,
            "ordered audio packet released for decoding",
        );
    }
    if let Some(duration) = decoded.decoder_queue_duration {
        diagnostics.observe(
            "audio_decoder_queue",
            u64::try_from(duration.as_micros()).unwrap_or(u64::MAX),
            None,
            sequence,
            "Opus decode started",
        );
    }
    diagnostics.observe(
        "audio_decode",
        u64::try_from(decoded.decode_duration.as_micros()).unwrap_or(u64::MAX),
        None,
        sequence,
        "FFmpeg libopus returned stereo samples",
    );
    if decoded.assembly_to_decoder_matched.is_some() {
        diagnostics.observe_measurement(
            "audio_decoder_input_hash",
            u64::try_from(decoded.decoder_input_hash_duration.as_micros()).unwrap_or(u64::MAX),
            "microseconds",
            sequence,
        );
        if decoded.assembly_to_decoder_matched == Some(false) {
            diagnostics.increment_counter("audio_assembly_decoder_mismatches");
        }
    }
    diagnostics.observe(
        "audio_decode_queue_to_sdl",
        decoded_queue_micros,
        None,
        sequence,
        "decoded audio submitted to SDL",
    );
    if let Some(duration) = playback_queue_duration {
        diagnostics.observe(
            "audio_decoded_playback_queue",
            u64::try_from(duration.as_micros()).unwrap_or(u64::MAX),
            None,
            sequence,
            "decoded audio removed from the playback queue",
        );
    }
    if let Some(duration) = sync_hold_duration {
        diagnostics.observe(
            "audio_video_sync_hold",
            u64::try_from(duration.as_micros()).unwrap_or(u64::MAX),
            None,
            sequence,
            "audio packet released after waiting for the video clock",
        );
    }
    diagnostics.observe_measurement(
        "audio_encoded_packet_size",
        decoded.encoded_bytes as u64,
        "bytes",
        sequence,
    );
    diagnostics.observe_measurement(
        "audio_sdl_queue_depth",
        sdl_queued_micros,
        "microseconds",
        sequence,
    );
    diagnostics.observe_measurement(
        "audio_concealed_packets",
        decoded.concealed_packets,
        "packets",
        sequence,
    );

    if let Some(video_timestamp_micros) = video_timestamp_micros {
        let (skew, audio_ahead) = if decoded.samples.captured_at.0 >= video_timestamp_micros {
            (decoded.samples.captured_at.0 - video_timestamp_micros, true)
        } else {
            (
                video_timestamp_micros - decoded.samples.captured_at.0,
                false,
            )
        };
        diagnostics.observe_measurement_classified(
            "audio_video_absolute_skew",
            skew,
            "microseconds",
            sequence,
            "derived",
            None,
        );
        diagnostics.observe_measurement_classified(
            "audio_ahead_of_video",
            u64::from(audio_ahead),
            "boolean",
            sequence,
            "derived",
            None,
        );
    }

    if let Some(offset) = clock_offset {
        let captured_player_clock =
            i128::from(decoded.samples.captured_at.0) - i128::from(offset.offset_micros);
        let decoded_player_clock = i128::from(decoded.assembled_at_micros)
            + i128::try_from(assembly_to_decode.as_micros()).unwrap_or(i128::MAX);
        if let Ok(capture_to_decode) = u64::try_from(decoded_player_clock - captured_player_clock) {
            diagnostics.observe_classified(
                "audio_capture_to_decode",
                capture_to_decode,
                None,
                sequence,
                "Opus decode completion; includes clock uncertainty",
                "clock-adjusted",
                Some("all host, network, assembly, queue, and decode stages"),
            );
            diagnostics.observe_classified(
                "audio_capture_to_estimated_playback",
                capture_to_decode
                    .saturating_add(decoded_queue_micros)
                    .saturating_add(sdl_queued_micros),
                None,
                sequence,
                "estimated SDL playback from queued byte depth",
                "clock-adjusted estimate",
                Some("all capture-to-decode stages, decoded queue residence, and SDL queued duration"),
            );
        }
    }

    let mut left_energy = 0.0_f64;
    let mut right_energy = 0.0_f64;
    let mut peak = 0.0_f32;
    let mut clipped = 0_u64;
    let mut non_finite = 0_u64;
    let mut frames = 0_u64;
    for frame in decoded.samples.interleaved.chunks_exact(2) {
        let (left, right) = (frame[0], frame[1]);
        if !left.is_finite() || !right.is_finite() {
            non_finite = non_finite.saturating_add(1);
            continue;
        }
        left_energy += f64::from(left) * f64::from(left);
        right_energy += f64::from(right) * f64::from(right);
        peak = peak.max(left.abs()).max(right.abs());
        clipped = clipped.saturating_add(u64::from(left.abs() >= 0.999));
        clipped = clipped.saturating_add(u64::from(right.abs() >= 0.999));
        frames = frames.saturating_add(1);
    }
    let denominator = frames.max(1) as f64;
    diagnostics.observe_measurement(
        "audio_left_rms",
        ((left_energy / denominator).sqrt() * 1_000_000.0) as u64,
        "millionths_full_scale",
        sequence,
    );
    diagnostics.observe_measurement(
        "audio_right_rms",
        ((right_energy / denominator).sqrt() * 1_000_000.0) as u64,
        "millionths_full_scale",
        sequence,
    );
    diagnostics.observe_measurement(
        "audio_peak",
        (f64::from(peak) * 1_000_000.0) as u64,
        "millionths_full_scale",
        sequence,
    );
    diagnostics.observe_measurement("audio_clipped_samples", clipped, "samples", sequence);
    diagnostics.observe_measurement("audio_non_finite_frames", non_finite, "frames", sequence);
    if clipped != 0 {
        diagnostics.record_anomaly(
            "audio_clipped_samples",
            sequence,
            &[("clipped_samples", clipped)],
        );
    }
    if non_finite != 0 {
        diagnostics.record_anomaly(
            "audio_non_finite_frames",
            sequence,
            &[("non_finite_frames", non_finite)],
        );
    }
    diagnostics.observe_measurement(
        "audio_silent_packet",
        u64::from(peak < 0.000_01),
        "boolean",
        sequence,
    );
}

fn observe_presented_frame(
    diagnostics: &mut LatencyDiagnostics,
    frame_sequence: u64,
    frame_queued_at: Instant,
    capture_player_at: Option<Instant>,
    input_sequence: u64,
    input_started_at: Option<Instant>,
    diagnostic_marker_input_sequence: Option<u64>,
    correlated_input_started_at: Option<Instant>,
    presented_at: Instant,
    endpoint: &'static str,
) {
    if let Some(captured_at) = capture_player_at {
        diagnostics.observe_classified(
            "video_source_capture_to_presentation",
            u64::try_from(
                presented_at
                    .saturating_duration_since(captured_at)
                    .as_micros(),
            )
            .unwrap_or(u64::MAX),
            Some(frame_sequence),
            None,
            endpoint,
            "clock-adjusted",
            Some("all host, network, assembly, decode, render, and presentation stages; includes clock uncertainty"),
        );
    }
    diagnostics.observe(
        "decode_queue_to_present",
        u64::try_from(
            presented_at
                .saturating_duration_since(frame_queued_at)
                .as_micros(),
        )
        .unwrap_or(u64::MAX),
        Some(frame_sequence),
        None,
        endpoint,
    );
    if input_sequence != 0
        && let Some(started) = input_started_at
    {
        diagnostics.observe_classified(
            "input_to_next_present",
            u64::try_from(presented_at.saturating_duration_since(started).as_micros())
                .unwrap_or(u64::MAX),
            Some(frame_sequence),
            Some(input_sequence),
            endpoint,
            "direct",
            Some("player input queue, network, host input, application response, capture, encode, network media, decode, render, and compositor presentation"),
        );
    }
    if let (Some(marker_input_sequence), Some(started)) = (
        diagnostic_marker_input_sequence,
        correlated_input_started_at,
    ) {
        diagnostics.observe_classified(
            "input_to_correlated_marker_present",
            u64::try_from(presented_at.saturating_duration_since(started).as_micros())
                .unwrap_or(u64::MAX),
            Some(frame_sequence),
            Some(marker_input_sequence),
            endpoint,
            "direct",
            Some("all input, application response, video, render, and compositor stages"),
        );
    }
}

fn apply_gui_action(action: PlayerGuiAction, window: &mut Window) {
    match action {
        PlayerGuiAction::ToggleFullscreen => {
            let fullscreen = window.fullscreen_state() == FullscreenType::Off;
            if let Err(error) = window.set_fullscreen(fullscreen) {
                eprintln!("rustconsole-player: could not change fullscreen state: {error}");
            }
        }
    }
}

fn gui_pointer_button(button: MouseButton) -> Option<PointerButton> {
    match button {
        MouseButton::Left => Some(PointerButton::Primary),
        MouseButton::Right => Some(PointerButton::Secondary),
        MouseButton::Middle => Some(PointerButton::Middle),
        MouseButton::X1 => Some(PointerButton::Extra1),
        MouseButton::X2 => Some(PointerButton::Extra2),
        _ => None,
    }
}

#[derive(Default)]
struct PointerRouting {
    gui_buttons: u8,
    host_buttons: u8,
}

struct RemoteInputFocus {
    focused: bool,
}

impl RemoteInputFocus {
    fn new() -> Self {
        Self { focused: true }
    }

    fn accepts(&self) -> bool {
        self.focused
    }

    fn gain(&mut self) {
        self.focused = true;
    }

    fn lose(&mut self) {
        self.focused = false;
    }
}

impl PointerRouting {
    fn press(&mut self, button: u8, captured_by_gui: bool) -> bool {
        let mask = pointer_button_mask(button);
        if self.host_buttons & mask != 0 {
            return true;
        }
        if self.gui_buttons & mask != 0 {
            return false;
        }
        if captured_by_gui {
            self.gui_buttons |= mask;
            false
        } else {
            self.host_buttons |= mask;
            true
        }
    }

    fn release(&mut self, button: u8) -> bool {
        let mask = pointer_button_mask(button);
        self.gui_buttons &= !mask;
        let sent_to_host = self.host_buttons & mask != 0;
        self.host_buttons &= !mask;
        sent_to_host
    }

    fn release_all(&mut self) {
        self.gui_buttons = 0;
        self.host_buttons = 0;
    }
}

fn pointer_button_mask(button: u8) -> u8 {
    1_u8.checked_shl(u32::from(button.saturating_sub(1)))
        .unwrap_or(0)
}

fn mouse_button(button: MouseButton) -> Option<u8> {
    match button {
        MouseButton::Left => Some(1),
        MouseButton::Right => Some(2),
        MouseButton::Middle => Some(3),
        MouseButton::X1 => Some(4),
        MouseButton::X2 => Some(5),
        _ => None,
    }
}

struct StreamOverlay {
    audio: Option<rustconsole_player_core::AudioTransportSnapshot>,
    audio_playback: AudioPlaybackSnapshot,
    statistics: OverlayStatistics,
    maximum_megabits_per_second: f64,
    target_megabits_per_second: f64,
    estimated_capacity_megabits_per_second: f64,
    completed_frames: u64,
    incomplete_frames: u64,
    interval_started: Instant,
    interval_frames: u64,
    interval_bytes: u64,
}

impl StreamOverlay {
    fn new(maximum_bitrate_bits_per_second: u64) -> Self {
        Self {
            audio: None,
            audio_playback: AudioPlaybackSnapshot::default(),
            statistics: OverlayStatistics::default(),
            maximum_megabits_per_second: maximum_bitrate_bits_per_second as f64 / 1_000_000.0,
            target_megabits_per_second: 1.0,
            estimated_capacity_megabits_per_second: 1.0,
            completed_frames: 0,
            incomplete_frames: 0,
            interval_started: Instant::now(),
            interval_frames: 0,
            interval_bytes: 0,
        }
    }

    fn observe(&mut self, sample: VideoStreamSample) {
        self.interval_frames += 1;
        self.interval_bytes = self
            .interval_bytes
            .saturating_add(sample.encoded_frame_bytes as u64);
        self.statistics.round_trip_time = sample.round_trip_time;
        self.statistics.lost_chunks = sample.lost_chunks;
        self.statistics.late_chunks = sample.late_chunks;
        self.statistics.assembly_overflows = sample.assembly_overflows;
        self.completed_frames = sample.completed_frames;
        self.incomplete_frames = sample.incomplete_frames;
        self.target_megabits_per_second =
            sample.target_bitrate_bits_per_second as f64 / 1_000_000.0;
        self.estimated_capacity_megabits_per_second =
            sample.estimated_capacity_bits_per_second as f64 / 1_000_000.0;
        let elapsed = self.interval_started.elapsed();
        if elapsed >= Duration::from_millis(500) {
            let seconds = elapsed.as_secs_f64();
            self.statistics.frames_per_second = self.interval_frames as f64 / seconds;
            self.statistics.encoded_megabits_per_second =
                self.interval_bytes as f64 * 8.0 / seconds / 1_000_000.0;
            self.interval_started = Instant::now();
            self.interval_frames = 0;
            self.interval_bytes = 0;
        }
    }

    fn text(&self, state: &str) -> String {
        let video = format!(
            "{RENDERING_BACKEND_LABEL}\n{state}\nFPS {:5.1}\nActual bitrate {:5.2} Mbit/s\nEstimated capacity {:5.2} Mbit/s\nTarget bitrate {:5.2} Mbit/s\nMaximum bitrate {:.0} Mbit/s\nRTT {:5.1} ms\nComplete {}  Incomplete {}\nLost {}  Late {}  Overflow {}",
            self.statistics.frames_per_second,
            self.statistics.encoded_megabits_per_second,
            self.estimated_capacity_megabits_per_second,
            self.target_megabits_per_second,
            self.maximum_megabits_per_second,
            self.statistics.round_trip_time.as_secs_f64() * 1_000.0,
            self.completed_frames,
            self.incomplete_frames,
            self.statistics.lost_chunks,
            self.statistics.late_chunks,
            self.statistics.assembly_overflows,
        );
        format!("{video}\n{}", self.audio_text())
    }

    fn audio_text(&self) -> String {
        let Some(audio) = &self.audio else {
            return "Audio: negotiating".into();
        };
        let status = match audio.host.status {
            0 => "not negotiated",
            1 => "waiting",
            2 => "receiving",
            3 => "device unavailable",
            4 => "failed",
            5 => "stopped",
            _ => "unknown state",
        };
        let stats = audio.receive;
        let detail = if !self.audio_playback.detail.is_empty() {
            format!("\nAudio detail: {}", self.audio_playback.detail)
        } else if audio.host.detail.is_empty() {
            String::new()
        } else {
            format!("\nAudio detail: {}", audio.host.detail)
        };
        let output = if !audio.negotiated() {
            "SDL output disabled".to_owned()
        } else if self.audio_playback.device_name.is_empty() {
            "opening SDL output".to_owned()
        } else {
            format!("SDL {}", self.audio_playback.device_name)
        };
        format!(
            "Audio: {status}; {output}\nOpus stereo 48 kHz  10 ms  128 kbit/s  SDL queue {} ms\nAudio packets {}  Host drops {}  Missing {}\nAudio expired {}  Network overflow {}  Late {}\nAudio duplicate {}  Invalid {}  Video queue drops {}  Audio queue drops {}\nPlayback pending {}  Queue drops {}  Late drops {}  Device drops {}  Underruns {}  Resets {}{detail}",
            self.audio_playback.queued_micros / 1_000,
            stats.completed_packets,
            audio.host.dropped_packets,
            stats.missing_packets,
            stats.expired_packets,
            stats.overflow_packets,
            stats.late_fragments,
            stats.duplicate_fragments,
            stats.malformed_fragments,
            audio.video_queue_drops,
            audio.audio_queue_drops,
            self.audio_playback.pending_packets,
            self.audio_playback.queue_drops,
            self.audio_playback.late_drops,
            self.audio_playback.device_drops,
            self.audio_playback.underruns,
            self.audio_playback.resets,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_clock_advances_between_presented_frames() {
        let presented_at = Instant::now();
        let clock = VideoPlaybackClock {
            captured_at_micros: 1_000_000,
            presented_at,
        };
        assert_eq!(
            clock.timestamp_at(presented_at + Duration::from_millis(100)),
            1_100_000
        );
    }

    #[test]
    fn reconnect_backoff_grows_to_its_bounded_jittered_delay() {
        let expected_bases = [250, 500, 1_000, 2_000, 4_000, 5_000, 5_000];
        let delays = reconnect_backoff(Some(7)).take(expected_bases.len());
        for (delay, base_millis) in delays.zip(expected_bases) {
            assert!(delay >= Duration::from_millis(base_millis));
            assert!(delay < Duration::from_millis(base_millis * 2));
        }
    }

    #[test]
    fn rebuilding_reconnect_backoff_resets_the_sequence() {
        let first = reconnect_backoff(Some(19)).next().unwrap();
        let mut advanced = reconnect_backoff(Some(19));
        let _ = advanced.next();
        assert!(advanced.next().unwrap() >= Duration::from_millis(500));
        assert_eq!(reconnect_backoff(Some(19)).next().unwrap(), first);
    }

    #[test]
    fn only_a_failed_previously_streaming_session_retries_automatically() {
        assert!(!should_retry_session_end(false, true));
        assert!(should_retry_session_end(true, true));
        assert!(!should_retry_session_end(true, false));
    }

    #[test]
    fn stable_stream_resets_backoff_at_thirty_seconds() {
        assert!(!should_reset_reconnect_backoff(
            true,
            Some(Duration::from_secs(29))
        ));
        assert!(should_reset_reconnect_backoff(
            true,
            Some(Duration::from_secs(30))
        ));
        assert!(!should_reset_reconnect_backoff(
            false,
            Some(Duration::from_secs(30))
        ));
    }

    #[test]
    fn pointer_release_follows_a_gui_captured_press() {
        let mut routing = PointerRouting::default();
        assert!(!routing.press(1, true));
        assert!(!routing.release(1));
    }

    #[test]
    fn pointer_release_reaches_the_host_after_crossing_the_gui() {
        let mut routing = PointerRouting::default();
        assert!(routing.press(1, false));
        assert!(routing.release(1));
        assert!(!routing.release(1));
    }

    #[test]
    fn focus_loss_clears_pointer_routing() {
        let mut routing = PointerRouting::default();
        assert!(routing.press(1, false));
        assert!(!routing.press(2, true));

        routing.release_all();

        assert!(!routing.release(1));
        assert!(!routing.release(2));
    }

    #[test]
    fn focus_loss_blocks_late_input_until_focus_returns() {
        let mut focus = RemoteInputFocus::new();
        assert!(focus.accepts());

        focus.lose();
        assert!(!focus.accepts());

        focus.gain();
        assert!(focus.accepts());
    }

    #[test]
    fn audio_overlay_distinguishes_transport_from_playback() {
        let mut overlay = StreamOverlay::new(20_000_000);
        let mut audio = rustconsole_player_core::AudioTransportSnapshot::default();
        audio.host.status = 2;
        audio.host.dropped_packets = 3;
        audio.receive.completed_packets = 12;
        audio.receive.expired_packets = 2;
        overlay.audio = Some(audio);
        let text = overlay.text("Streaming");
        assert!(text.contains("Audio: receiving; opening SDL output"));
        assert!(text.contains("Audio packets 12  Host drops 3"));
        assert!(text.contains("Audio expired 2"));
    }

    #[test]
    fn audio_overlay_keeps_sdl_closed_when_audio_is_not_negotiated() {
        let mut overlay = StreamOverlay::new(20_000_000);
        overlay.audio = Some(rustconsole_player_core::AudioTransportSnapshot::default());
        assert!(overlay.audio_text().contains("SDL output disabled"));
    }

    #[test]
    fn overlay_includes_all_transport_counters() {
        let mut overlay = StreamOverlay::new(100_000_000);
        overlay.observe(VideoStreamSample {
            encoded_frame_bytes: 125_000,
            target_bitrate_bits_per_second: 5_000_000,
            estimated_capacity_bits_per_second: 8_000_000,
            round_trip_time: Duration::from_millis(7),
            lost_chunks: 2,
            late_chunks: 3,
            assembly_overflows: 4,
            completed_frames: 8,
            incomplete_frames: 1,
        });
        let text = overlay.text("Streaming");
        assert!(text.starts_with("Rendering backend: Vulkan\nStreaming\n"));
        assert!(text.contains(
            "Actual bitrate  0.00 Mbit/s\nEstimated capacity  8.00 Mbit/s\nTarget bitrate  5.00 Mbit/s\nMaximum bitrate 100 Mbit/s"
        ));
        assert!(text.contains("RTT   7.0 ms"));
        assert!(text.contains("Complete 8  Incomplete 1"));
        assert!(text.contains("Lost 2  Late 3  Overflow 4"));
    }
}
