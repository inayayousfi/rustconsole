use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use rustconsole_test_game::{
    AUDIO_PEAK, LaunchOptions, RawInputState, synchronization_multitone,
    synchronization_pulse_active, tile_palette_index,
};
use sdl3::event::Event;
use sdl3::pixels::Color;
use sdl3::render::FRect;
use std::ffi::c_void;
use std::mem::{MaybeUninit, size_of};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RID_INPUT, RIDEV_NOLEGACY, RIDEV_REMOVE,
    RIM_TYPEKEYBOARD, RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{RI_KEY_BREAK, WM_INPUT};

struct SynchronizedAudio {
    stream: sdl3::audio::AudioStreamOwner,
    cycle: Vec<f32>,
}

impl SynchronizedAudio {
    fn open(sdl: &sdl3::Sdl) -> Result<Self, String> {
        let audio = sdl.audio().map_err(|e| e.to_string())?;
        let spec = sdl3::audio::AudioSpec {
            freq: Some(48_000),
            channels: Some(2),
            format: Some(sdl3::audio::AudioFormat::f32_sys()),
        };
        let device = audio
            .open_playback_device(&spec)
            .map_err(|e| e.to_string())?;
        let stream = device
            .open_device_stream(Some(&spec))
            .map_err(|e| e.to_string())?;
        let cycle = synchronization_multitone();
        stream.put_data_f32(&cycle).map_err(|e| e.to_string())?;
        stream.resume().map_err(|e| e.to_string())?;
        Ok(Self { stream, cycle })
    }

    fn refill(&self) -> Result<(), String> {
        let bytes = self.cycle.len() * size_of::<f32>();
        let queued = self.stream.queued_bytes().map_err(|e| e.to_string())?;
        if queued < 0 {
            return Err("SDL reported a negative audio queue size".into());
        }
        // At most two cycles are queued, regardless of render rate.
        if (queued as usize) < bytes {
            self.stream
                .put_data_f32(&self.cycle)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

const RAW_INPUT_SUBCLASS_ID: usize = 0x5255_5354;
const TILE_SIZE: u32 = 40;
const PALETTE: [Color; 16] = [
    Color::RGB(10, 15, 45),
    Color::RGB(242, 62, 98),
    Color::RGB(255, 180, 45),
    Color::RGB(25, 210, 180),
    Color::RGB(42, 100, 230),
    Color::RGB(175, 65, 220),
    Color::RGB(240, 240, 245),
    Color::RGB(20, 180, 70),
    Color::RGB(235, 95, 35),
    Color::RGB(30, 200, 245),
    Color::RGB(135, 220, 45),
    Color::RGB(245, 55, 190),
    Color::RGB(80, 45, 160),
    Color::RGB(255, 220, 70),
    Color::RGB(40, 40, 45),
    Color::RGB(180, 225, 255),
];

pub fn run(options: LaunchOptions) -> Result<(), String> {
    let sdl = sdl3::init().map_err(|error| error.to_string())?;
    let video = sdl.video().map_err(|error| error.to_string())?;
    let display = video
        .get_primary_display()
        .map_err(|error| error.to_string())?;
    let mode = display.get_mode().map_err(|error| error.to_string())?;
    let bounds = display.get_bounds().map_err(|error| error.to_string())?;
    let mut window = video
        .window("Rust Console Test Game", mode.w as u32, mode.h as u32)
        .position(bounds.x(), bounds.y())
        .hidden()
        .build()
        .map_err(|error| error.to_string())?;
    window
        .set_display_mode(mode)
        .map_err(|error| error.to_string())?;
    window
        .set_fullscreen(true)
        .map_err(|error| error.to_string())?;
    if !window.show() {
        return Err(sdl3::get_error().to_string());
    }
    if !window.set_mouse_grab(true) {
        return Err(sdl3::get_error().to_string());
    }
    sdl.mouse().show_cursor(false);

    let mut canvas = window.into_canvas();
    let raw_state = Arc::new(Mutex::new(RawInputState::default()));
    let raw_input = RawInputWindow::install(canvas.window(), Arc::clone(&raw_state))?;
    let mut events = sdl.event_pump().map_err(|error| error.to_string())?;
    let started = Instant::now();
    let deadline = options.duration.map(|duration| started + duration);
    let frame_interval = Duration::from_secs_f64(1.0 / f64::from(mode.refresh_rate.max(1.0)));
    let mut next_frame = started;
    let mut frames = 0_u64;
    let loop_result = (|| -> Result<(), String> {
        let audio = SynchronizedAudio::open(&sdl)
            .map_err(|error| format!("audio startup failed: {error}"))?;
        loop {
            for event in events.poll_iter() {
                if matches!(event, Event::Quit { .. }) {
                    return Ok(());
                }
            }
            let snapshot = raw_state
                .lock()
                .map_err(|_| "raw-input state is unavailable")?
                .clone();
            if snapshot.exit_requested
                || deadline.is_some_and(|deadline| Instant::now() >= deadline)
            {
                return Ok(());
            }

            render_stress_frame(&mut canvas, frames, started.elapsed(), &snapshot)?;
            audio
                .refill()
                .map_err(|error| format!("audio refill failed: {error}"))?;
            frames += 1;
            next_frame += frame_interval;
            let now = Instant::now();
            if next_frame > now {
                std::thread::sleep(next_frame - now);
            } else if now.duration_since(next_frame) > frame_interval.saturating_mul(4) {
                next_frame = now;
            }
        }
    })();
    let audio_error = loop_result
        .as_ref()
        .err()
        .filter(|error| error.starts_with("audio "))
        .cloned()
        .unwrap_or_default();

    drop(raw_input);
    canvas.window_mut().set_mouse_grab(false);
    sdl.mouse().show_cursor(true);
    let elapsed = started.elapsed();
    let snapshot = raw_state
        .lock()
        .map_err(|_| "raw-input state is unavailable")?
        .clone();
    if let Some(path) = options.report_path {
        std::fs::write(
            path,
            format!(
                "status={}\nrenderer={}\ndisplay={}\nwidth={}\nheight={}\nrefresh_hz={:.3}\nframes={}\nelapsed_micros={}\nmouse_packets={}\nmouse_delta_x={}\nmouse_delta_y={}\nmouse_wheels={}\nkeyboard_packets={}\npixel_readback=not-performed\naudio=synchronized-stereo-multitone\naudio_peak={AUDIO_PEAK}\naudio_sync_period_micros=1000000\naudio_sync_pulse_micros=100000\naudio_error={audio_error}\n",
                if loop_result.is_ok() { "ok" } else { "error" },
                canvas.renderer_name,
                display.get_name().map_err(|error| error.to_string())?,
                mode.w,
                mode.h,
                mode.refresh_rate,
                frames,
                elapsed.as_micros(),
                snapshot.mouse_packets,
                snapshot.mouse_delta_x,
                snapshot.mouse_delta_y,
                snapshot.mouse_wheels,
                snapshot.keyboard_packets,
            ),
        )
        .map_err(|error| format!("could not write report: {error}"))?;
    }
    loop_result
}

fn render_stress_frame(
    canvas: &mut sdl3::render::WindowCanvas,
    frame: u64,
    elapsed: Duration,
    input: &RawInputState,
) -> Result<(), String> {
    let (width, height) = canvas.output_size().map_err(|error| error.to_string())?;
    canvas.set_draw_color(Color::RGB(0, 0, 0));
    canvas.clear();
    let columns = width.div_ceil(TILE_SIZE);
    let rows = height.div_ceil(TILE_SIZE);
    let mut tiles = std::array::from_fn::<Vec<FRect>, 16, _>(|_| Vec::new());
    for row in 0..rows {
        for column in 0..columns {
            tiles[tile_palette_index(frame, column, row)].push(FRect::new(
                (column * TILE_SIZE) as f32,
                (row * TILE_SIZE) as f32,
                TILE_SIZE as f32,
                TILE_SIZE as f32,
            ));
        }
    }
    for (color, rectangles) in PALETTE.into_iter().zip(tiles) {
        canvas.set_draw_color(color);
        canvas
            .fill_rects(&rectangles)
            .map_err(|error| error.to_string())?;
    }

    let marker_x = input.mouse_delta_x.rem_euclid(i64::from(width.max(1))) as f32;
    let marker_y = input.mouse_delta_y.rem_euclid(i64::from(height.max(1))) as f32;
    let marker_color = if input.mouse_buttons != 0 {
        Color::RGB(255, 255, 255)
    } else if input.pressed_keys != 0 {
        Color::RGB(0, 0, 0)
    } else {
        Color::RGB(255, 40, 40)
    };
    canvas.set_draw_color(marker_color);
    canvas
        .fill_rect(FRect::new(marker_x - 36.0, marker_y - 36.0, 72.0, 72.0))
        .map_err(|error| error.to_string())?;
    canvas.set_draw_color(Color::RGB(255, 255, 255));
    canvas
        .fill_rect(FRect::new(16.0, 16.0, 32.0, 32.0))
        .map_err(|error| error.to_string())?;
    canvas.set_draw_color(Color::RGB(0, 0, 0));
    canvas
        .fill_rect(FRect::new(48.0, 16.0, 32.0, 32.0))
        .map_err(|error| error.to_string())?;
    canvas.set_draw_color(if input.mouse_button_presses.is_multiple_of(2) {
        Color::RGB(255, 255, 255)
    } else {
        Color::RGB(0, 0, 0)
    });
    canvas
        .fill_rect(FRect::new(80.0, 16.0, 32.0, 32.0))
        .map_err(|error| error.to_string())?;
    canvas.set_draw_color(if synchronization_pulse_active(elapsed) {
        Color::RGB(255, 230, 0)
    } else {
        Color::RGB(20, 20, 20)
    });
    canvas
        .fill_rect(FRect::new(112.0, 16.0, 32.0, 32.0))
        .map_err(|error| error.to_string())?;
    if !canvas.present() {
        return Err(sdl3::get_error().to_string());
    }
    Ok(())
}

struct RawInputWindow {
    hwnd: HWND,
    callback_state: *const Mutex<RawInputState>,
}

impl RawInputWindow {
    fn install(
        window: &sdl3::video::Window,
        state: Arc<Mutex<RawInputState>>,
    ) -> Result<Self, String> {
        let RawWindowHandle::Win32(handle) = window
            .window_handle()
            .map_err(|error| error.to_string())?
            .as_raw()
        else {
            return Err("SDL did not expose a Windows window handle".into());
        };
        let hwnd = HWND(handle.hwnd.get() as *mut c_void);
        let callback_state = Arc::into_raw(state);
        let installed = unsafe {
            SetWindowSubclass(
                hwnd,
                Some(raw_input_window_proc),
                RAW_INPUT_SUBCLASS_ID,
                callback_state as usize,
            )
        };
        if !installed.as_bool() {
            unsafe { drop(Arc::from_raw(callback_state)) };
            return Err("could not install the raw-input window callback".into());
        }
        let devices = [
            RAWINPUTDEVICE {
                usUsagePage: 0x01,
                usUsage: 0x02,
                dwFlags: RIDEV_NOLEGACY,
                hwndTarget: hwnd,
            },
            RAWINPUTDEVICE {
                usUsagePage: 0x01,
                usUsage: 0x06,
                dwFlags: RIDEV_NOLEGACY,
                hwndTarget: hwnd,
            },
        ];
        if let Err(error) =
            unsafe { RegisterRawInputDevices(&devices, size_of::<RAWINPUTDEVICE>() as u32) }
        {
            unsafe {
                let _ =
                    RemoveWindowSubclass(hwnd, Some(raw_input_window_proc), RAW_INPUT_SUBCLASS_ID);
                drop(Arc::from_raw(callback_state));
            }
            return Err(format!("could not register Windows raw input: {error}"));
        }
        Ok(Self {
            hwnd,
            callback_state,
        })
    }
}

impl Drop for RawInputWindow {
    fn drop(&mut self) {
        let devices = [
            RAWINPUTDEVICE {
                usUsagePage: 0x01,
                usUsage: 0x02,
                dwFlags: RIDEV_REMOVE,
                hwndTarget: HWND::default(),
            },
            RAWINPUTDEVICE {
                usUsagePage: 0x01,
                usUsage: 0x06,
                dwFlags: RIDEV_REMOVE,
                hwndTarget: HWND::default(),
            },
        ];
        unsafe {
            let _ = RegisterRawInputDevices(&devices, size_of::<RAWINPUTDEVICE>() as u32);
            let _ = RemoveWindowSubclass(
                self.hwnd,
                Some(raw_input_window_proc),
                RAW_INPUT_SUBCLASS_ID,
            );
            drop(Arc::from_raw(self.callback_state));
        }
    }
}

unsafe extern "system" fn raw_input_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    callback_state: usize,
) -> LRESULT {
    if message == WM_INPUT {
        let mut input = MaybeUninit::<RAWINPUT>::uninit();
        let mut bytes = size_of::<RAWINPUT>() as u32;
        let read = unsafe {
            GetRawInputData(
                HRAWINPUT(lparam.0 as *mut c_void),
                RID_INPUT,
                Some(input.as_mut_ptr().cast()),
                &mut bytes,
                size_of::<windows::Win32::UI::Input::RAWINPUTHEADER>() as u32,
            )
        };
        if read != u32::MAX && read >= size_of::<RAWINPUT>() as u32 {
            let input = unsafe { input.assume_init() };
            let state = unsafe { &*(callback_state as *const Mutex<RawInputState>) };
            if let Ok(mut state) = state.lock() {
                if input.header.dwType == RIM_TYPEMOUSE.0 {
                    let mouse = unsafe { input.data.mouse };
                    let buttons = unsafe { mouse.Anonymous.Anonymous }.usButtonFlags;
                    state.record_mouse(mouse.lLastX, mouse.lLastY, buttons);
                } else if input.header.dwType == RIM_TYPEKEYBOARD.0 {
                    let keyboard = unsafe { input.data.keyboard };
                    state.record_keyboard(
                        keyboard.VKey,
                        u32::from(keyboard.Flags) & RI_KEY_BREAK == 0,
                    );
                }
            }
        }
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}
