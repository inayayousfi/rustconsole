use backon::BackoffBuilder;
use rustconsole_player_core::StreamProgress;
use rustconsole_player_core::process_protocol::{
    LaunchRequest, PlayerCommand, PlayerEvent, read_command, write_event,
};
use rustconsole_player_gui::{GuiFrame, PlayerGui, PlayerGuiAction, PlayerGuiView, PointerButton};
use rustconsole_player_linux::{
    AudioPlaybackQueue, AudioPlaybackSnapshot, Av1ColorDescription, DecodedVideoFrame,
    DmaBufFrameFormat, NativeDmaBufFrame, SdlAudioOutput, StreamCallbacks, VideoStreamSample,
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
use std::collections::VecDeque;
use std::io::{BufReader, BufWriter};
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const RENDERING_BACKEND_LABEL: &str = "Rendering backend: Vulkan";
const RECONNECT_STABLE_RESET: Duration = Duration::from_secs(30);

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
    renderer.present(&frame, width.max(1), height.max(1), frame_gui)?;
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
    input: mpsc::SyncSender<InputEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ActiveStreamSession {
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
    frames: Arc<Mutex<VecDeque<(u64, Av1ColorDescription, NativeDmaBufFrame)>>>,
    audio: AudioPlaybackQueue,
) -> ActiveStreamSession {
    let stop = Arc::new(AtomicBool::new(false));
    let (input, input_rx) = mpsc::sync_channel(1024);
    let session_stop = Arc::clone(&stop);
    let address = launch.address.to_string();
    let password = (!launch.password.is_empty()).then(|| launch.password.to_vec());
    let remember_password = launch.remember_password;
    let maximum_bitrate_bits_per_second = launch.maximum_bitrate_bits_per_second;
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
                    let mapped = decoded.frame.map_dma_buf()?;
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
                    }
                    frames.push_back((
                        decoded.captured_at_micros,
                        decoded.color_description,
                        mapped,
                    ));
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
        thread: Some(thread),
    }
}

fn run_pipe_session() -> Result<(), Box<dyn std::error::Error>> {
    let mut commands = BufReader::new(std::io::stdin());
    let PlayerCommand::Launch(launch) = read_command(&mut commands)? else {
        return Err("first player command must launch a session".into());
    };
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
        audio_queue.clone(),
    );

    let mut gui = PlayerGui::default();
    let gui_started = Instant::now();
    let mut pointer_routing = PointerRouting::default();
    let mut renderer =
        None::<Box<dyn PlayerVideoBackend<NativeDmaBufFrame, GuiFrame, Error = String>>>;
    let mut last_frame = None::<(u64, Av1ColorDescription, NativeDmaBufFrame)>;
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
                } => {
                    let usage = scancode as u32;
                    if let Ok(hid_usage) = u16::try_from(usage) {
                        let _ = session.input.send(InputEvent::Key {
                            hid_usage,
                            pressed: true,
                        });
                    }
                }
                Event::KeyUp {
                    scancode: Some(scancode),
                    ..
                } => {
                    let usage = scancode as u32;
                    if let Ok(hid_usage) = u16::try_from(usage) {
                        let _ = session.input.send(InputEvent::Key {
                            hid_usage,
                            pressed: false,
                        });
                    }
                }
                Event::MouseButtonDown {
                    mouse_btn, x, y, ..
                } => {
                    gui.pointer_moved(x, y);
                    if let Some(gui_button) = gui_pointer_button(mouse_btn) {
                        let captured = gui.captures_pointer_at(x, y);
                        gui.pointer_button(x, y, gui_button, true);
                        redraw = true;
                        let Some(button) = mouse_button(mouse_btn) else {
                            continue;
                        };
                        if pointer_routing.press(button, captured) {
                            let _ = session.input.send(InputEvent::PointerButton {
                                button,
                                pressed: true,
                            });
                        }
                    } else if let Some(button) = mouse_button(mouse_btn) {
                        let _ = session.input.send(InputEvent::PointerButton {
                            button,
                            pressed: true,
                        });
                    }
                }
                Event::MouseButtonUp {
                    mouse_btn, x, y, ..
                } => {
                    gui.pointer_moved(x, y);
                    if let Some(gui_button) = gui_pointer_button(mouse_btn) {
                        gui.pointer_button(x, y, gui_button, false);
                        redraw = true;
                    }
                    if let Some(button) = mouse_button(mouse_btn)
                        && pointer_routing.release(button)
                    {
                        let _ = session.input.send(InputEvent::PointerButton {
                            button,
                            pressed: false,
                        });
                    }
                }
                Event::MouseMotion { x, y, .. } => {
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
                        let _ = session.input.try_send(InputEvent::PointerPosition { x, y });
                    }
                }
                Event::MouseWheel {
                    x,
                    y,
                    mouse_x,
                    mouse_y,
                    ..
                } => {
                    gui.mouse_wheel(x, y);
                    redraw = true;
                    if gui.captures_pointer_at(mouse_x, mouse_y) {
                        continue;
                    }
                    let horizontal = x.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                    let vertical = y.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                    if horizontal != 0 || vertical != 0 {
                        let _ = session.input.send(InputEvent::Wheel {
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
                    gui.set_focused(true);
                    redraw = true;
                }
                Event::Window {
                    win_event: WindowEvent::FocusLost,
                    ..
                } => {
                    gui.set_focused(false);
                    redraw = true;
                }
                _ => {}
            }
        }
        let latest = {
            let mut frames = frames.lock().unwrap();
            let latest = frames.pop_back();
            frames.clear();
            latest
        };
        if let Some(frame) = latest {
            pending_video_timestamp = Some(frame.0);
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
                overlay.text("Streaming")
            } else {
                String::new()
            };
            redraw = false;
            if let Some(frame) = &last_frame {
                let (gui_frame, gui_action) = gui.frame(PlayerGuiView {
                    fullscreen,
                    status: None,
                    diagnostics: &diagnostics,
                });
                renderer.as_mut().unwrap().present_frame(
                    &frame.2,
                    match frame.1 {
                        Av1ColorDescription::Bt709Limited => DecodedVideoColor::Bt709Limited,
                        Av1ColorDescription::Bt2020PqLimited => DecodedVideoColor::Bt2020PqLimited,
                    },
                    width.max(1),
                    height.max(1),
                    gui_frame,
                )?;
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
            while let Some(samples) = audio_queue.pop_for_video(video_timestamp_micros) {
                audio_output.play(samples);
            }
        }
        overlay.audio_playback = audio_output.snapshot(&audio_queue);
        std::thread::sleep(Duration::from_millis(2));
    }
    session.stop();
    drop(renderer);
    drop(last_frame);
    match stream_result.unwrap_or(Ok(())) {
        Ok(()) => write_event(&mut events, &PlayerEvent::Ended)?,
        Err(error) => write_event(&mut events, &PlayerEvent::Error(error))?,
    }
    Ok(())
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
