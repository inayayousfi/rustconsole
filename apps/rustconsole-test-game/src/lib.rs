use std::path::PathBuf;
use std::time::Duration;

pub const MAXIMUM_DURATION_SECONDS: u64 = 3_600;
pub const ESCAPE_VIRTUAL_KEY: u16 = 0x1b;
pub const AUDIO_SAMPLE_RATE: usize = 48_000;
pub const SYNCHRONIZATION_CYCLE_FRAMES: usize = AUDIO_SAMPLE_RATE * 4;
pub const AUDIO_PEAK: f32 = 0.24;
pub const SYNCHRONIZATION_PERIOD: Duration = Duration::from_secs(1);
pub const SYNCHRONIZATION_PULSE: Duration = Duration::from_millis(100);

/// One loop with distinct channel tones and a shared pulse aligned to the visual cue.
pub fn synchronization_multitone() -> Vec<f32> {
    let fade_frames = AUDIO_SAMPLE_RATE / 50;
    let pulse_frames = SYNCHRONIZATION_PULSE.as_secs_f64() * AUDIO_SAMPLE_RATE as f64;
    let mut samples = Vec::with_capacity(SYNCHRONIZATION_CYCLE_FRAMES * 2);
    for frame in 0..SYNCHRONIZATION_CYCLE_FRAMES {
        let fade = (frame.min(SYNCHRONIZATION_CYCLE_FRAMES - 1 - frame) as f64
            / fade_frames as f64)
            .min(1.0);
        let phase = frame as f64 / AUDIO_SAMPLE_RATE as f64;
        let pulse_position = (frame % AUDIO_SAMPLE_RATE) as f64;
        let pulse_envelope = if pulse_position < pulse_frames {
            (std::f64::consts::PI * pulse_position / pulse_frames)
                .sin()
                .powi(2)
        } else {
            0.0
        };
        let pulse = 0.08 * pulse_envelope * (880.0 * std::f64::consts::TAU * phase).sin();
        let left = 0.08 * (220.0 * std::f64::consts::TAU * phase).sin()
            + 0.06 * (440.0 * std::f64::consts::TAU * phase).sin()
            + pulse;
        let right = 0.08 * (330.0 * std::f64::consts::TAU * phase).sin()
            + 0.06 * (550.0 * std::f64::consts::TAU * phase).sin()
            + pulse;
        samples.push((left * fade) as f32);
        samples.push((right * fade) as f32);
    }
    samples
}

#[must_use]
pub fn synchronization_pulse_active(elapsed: Duration) -> bool {
    elapsed.as_nanos() % SYNCHRONIZATION_PERIOD.as_nanos() < SYNCHRONIZATION_PULSE.as_nanos()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawInputState {
    pub mouse_packets: u64,
    pub keyboard_packets: u64,
    pub mouse_delta_x: i64,
    pub mouse_delta_y: i64,
    pub mouse_buttons: u8,
    pub mouse_button_presses: u64,
    pub mouse_wheels: u64,
    pub pressed_keys: u16,
    pub exit_requested: bool,
    keys: [bool; 256],
}

impl Default for RawInputState {
    fn default() -> Self {
        Self {
            mouse_packets: 0,
            keyboard_packets: 0,
            mouse_delta_x: 0,
            mouse_delta_y: 0,
            mouse_buttons: 0,
            mouse_button_presses: 0,
            mouse_wheels: 0,
            pressed_keys: 0,
            exit_requested: false,
            keys: [false; 256],
        }
    }
}

impl RawInputState {
    pub fn record_mouse(&mut self, delta_x: i32, delta_y: i32, button_flags: u16) {
        self.mouse_packets += 1;
        self.mouse_delta_x += i64::from(delta_x);
        self.mouse_delta_y += i64::from(delta_y);

        for (index, down, up) in [
            (0, 0x0001, 0x0002),
            (1, 0x0004, 0x0008),
            (2, 0x0010, 0x0020),
            (3, 0x0040, 0x0080),
            (4, 0x0100, 0x0200),
        ] {
            if button_flags & down != 0 {
                self.mouse_buttons |= 1 << index;
                self.mouse_button_presses = self.mouse_button_presses.saturating_add(1);
            }
            if button_flags & up != 0 {
                self.mouse_buttons &= !(1 << index);
            }
        }
        if button_flags & (0x0400 | 0x0800) != 0 {
            self.mouse_wheels += 1;
        }
    }

    pub fn record_keyboard(&mut self, virtual_key: u16, pressed: bool) {
        self.keyboard_packets += 1;
        let Some(key) = self.keys.get_mut(usize::from(virtual_key)) else {
            return;
        };
        if *key != pressed {
            self.pressed_keys = if pressed {
                self.pressed_keys.saturating_add(1)
            } else {
                self.pressed_keys.saturating_sub(1)
            };
            *key = pressed;
        }
        if virtual_key == ESCAPE_VIRTUAL_KEY && pressed {
            self.exit_requested = true;
        }
    }

    pub fn release_all(&mut self) {
        self.keys.fill(false);
        self.pressed_keys = 0;
        self.mouse_buttons = 0;
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LaunchOptions {
    pub duration: Option<Duration>,
    pub report_path: Option<PathBuf>,
}

pub fn parse_options(arguments: impl IntoIterator<Item = String>) -> Result<LaunchOptions, String> {
    let mut arguments = arguments.into_iter();
    let mut options = LaunchOptions::default();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--duration-seconds" => {
                let value = arguments
                    .next()
                    .ok_or("--duration-seconds requires a value")?;
                let seconds = value
                    .parse::<u64>()
                    .map_err(|_| "duration must be a whole number of seconds")?;
                if !(1..=MAXIMUM_DURATION_SECONDS).contains(&seconds) {
                    return Err(format!(
                        "duration must be between 1 and {MAXIMUM_DURATION_SECONDS} seconds"
                    ));
                }
                options.duration = Some(Duration::from_secs(seconds));
            }
            "--report" => {
                let value = arguments.next().ok_or("--report requires a path")?;
                if value.is_empty() {
                    return Err("report path must not be empty".into());
                }
                options.report_path = Some(PathBuf::from(value));
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    Ok(options)
}

#[must_use]
pub fn tile_palette_index(frame: u64, column: u32, row: u32) -> usize {
    let mut value = frame
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .wrapping_add(u64::from(column) << 32)
        .wrapping_add(u64::from(row));
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    ((value ^ (value >> 31)) & 15) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synchronization_multitone_is_moderate_bounded_and_repeatable() {
        let samples = synchronization_multitone();
        assert_eq!(samples.len(), SYNCHRONIZATION_CYCLE_FRAMES * 2);
        assert_eq!(samples, synchronization_multitone());
        assert!(
            samples
                .iter()
                .all(|sample| sample.is_finite() && sample.abs() <= AUDIO_PEAK)
        );
        assert_eq!(&samples[..2], &[0.0, 0.0]);
        assert!(
            samples[samples.len() - 2..]
                .iter()
                .all(|sample| *sample == 0.0)
        );
        let rms = (samples.iter().map(|sample| sample * sample).sum::<f32>()
            / samples.len() as f32)
            .sqrt();
        assert!(rms > 0.05);
        assert!(synchronization_pulse_active(Duration::from_millis(50)));
        assert!(!synchronization_pulse_active(Duration::from_millis(150)));
        assert!(synchronization_pulse_active(Duration::from_millis(1_050)));
    }

    #[test]
    fn raw_input_tracks_transitions_without_duplicate_holds() {
        let mut state = RawInputState::default();
        state.record_keyboard(65, true);
        state.record_keyboard(65, true);
        state.record_keyboard(66, true);
        state.record_keyboard(65, false);
        state.record_mouse(7, -3, 0x0001 | 0x0400);

        assert_eq!(state.keyboard_packets, 4);
        assert_eq!(state.pressed_keys, 1);
        assert_eq!(state.mouse_packets, 1);
        assert_eq!((state.mouse_delta_x, state.mouse_delta_y), (7, -3));
        assert_eq!(state.mouse_buttons, 1);
        assert_eq!(state.mouse_button_presses, 1);
        assert_eq!(state.mouse_wheels, 1);
    }

    #[test]
    fn escape_requests_exit_and_release_clears_held_state() {
        let mut state = RawInputState::default();
        state.record_keyboard(ESCAPE_VIRTUAL_KEY, true);
        state.record_mouse(0, 0, 0x0001 | 0x0004);
        assert!(state.exit_requested);

        state.release_all();
        assert_eq!(state.pressed_keys, 0);
        assert_eq!(state.mouse_buttons, 0);
    }

    #[test]
    fn options_accept_bounded_unattended_run() {
        let options = parse_options([
            "--duration-seconds".into(),
            "60".into(),
            "--report".into(),
            "proof.txt".into(),
        ])
        .unwrap();

        assert_eq!(options.duration, Some(Duration::from_secs(60)));
        assert_eq!(options.report_path, Some(PathBuf::from("proof.txt")));
    }

    #[test]
    fn options_reject_unbounded_or_unknown_values() {
        assert!(parse_options(["--duration-seconds".into(), "0".into()]).is_err());
        assert!(parse_options(["--duration-seconds".into(), "3601".into()]).is_err());
        assert!(parse_options(["--other".into()]).is_err());
    }

    #[test]
    fn stress_tiles_are_repeatable_and_change_between_frames() {
        let first = (0..8)
            .map(|column| tile_palette_index(20, column, 3))
            .collect::<Vec<_>>();
        let repeated = (0..8)
            .map(|column| tile_palette_index(20, column, 3))
            .collect::<Vec<_>>();
        let next = (0..8)
            .map(|column| tile_palette_index(21, column, 3))
            .collect::<Vec<_>>();

        assert_eq!(first, repeated);
        assert_ne!(first, next);
    }
}
