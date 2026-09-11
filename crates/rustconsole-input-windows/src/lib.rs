//! Safe Windows virtual-input state and driver integration.

use rustconsole_protocol::InputEvent;
use rustconsole_protocol::wire::{InputAck, InputTransition, PointerMode, input_transition};
use std::fmt;

mod descriptor;
mod ipc;
#[cfg(windows)]
mod windows_ipc;
#[cfg(windows)]
mod windows_owner;
pub use descriptor::{DescriptorError, DescriptorLayout, ReportLengths, parse_report_descriptor};
pub use ipc::{
    FEATURE_WRITE_OPERATION, OUTPUT_REPORT_OPERATION, OutputRecord, REPORT_CAPACITY,
    REPORT_RING_MAGIC, REPORT_RING_VERSION, REPORT_SLOT_COUNT, ReportRingHeader, ReportSlot,
    RingError, SharedReportRing,
};
#[cfg(windows)]
pub use windows_ipc::{WindowsRingError, WindowsRingPair};
#[cfg(windows)]
pub use windows_owner::{VirtualInputOwner, remove_persistent_devices};

pub const TEST_VENDOR_ID: u16 = 0x1209;
pub const TEST_MOUSE_PRODUCT_ID: u16 = 0x000e;
pub const TEST_KEYBOARD_PRODUCT_ID: u16 = 0x000f;
pub const MOUSE_REPORT_SIZE: usize = 8;
pub const KEYBOARD_REPORT_SIZE: usize = 29;
pub const KEYBOARD_LED_REPORT_SIZE: usize = 1;

pub const MOUSE_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x85, 0x01, 0x05, 0x09, 0x19, 0x01,
    0x29, 0x05, 0x15, 0x00, 0x25, 0x01, 0x95, 0x05, 0x75, 0x01, 0x81, 0x02, 0x95, 0x03, 0x75, 0x01,
    0x81, 0x03, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x16, 0x00, 0x80, 0x26, 0xff, 0x7f, 0x75, 0x10,
    0x95, 0x02, 0x81, 0x06, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06,
    0x05, 0x0c, 0x0a, 0x38, 0x02, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, 0xc0,
    0xc0, 0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x85, 0x02, 0x05, 0x09, 0x19,
    0x01, 0x29, 0x05, 0x15, 0x00, 0x25, 0x01, 0x95, 0x05, 0x75, 0x01, 0x81, 0x02, 0x95, 0x03, 0x75,
    0x01, 0x81, 0x03, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26, 0xff, 0x7f, 0x75, 0x10,
    0x95, 0x02, 0x81, 0x02, 0x75, 0x08, 0x95, 0x02, 0x81, 0x03, 0xc0, 0xc0,
];

/// One no-ID NKRO report: eight modifiers followed by usages 0x00..0xdf.
pub const KEYBOARD_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x05, 0x75, 0x01, 0x05, 0x08, 0x19, 0x01, 0x29, 0x05,
    0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x01, 0x05, 0x07, 0x19, 0x00, 0x29, 0xdf, 0x15, 0x00,
    0x25, 0x01, 0x75, 0x01, 0x96, 0xe0, 0x00, 0x81, 0x02, 0xc0,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HidReport {
    Mouse([u8; MOUSE_REPORT_SIZE]),
    Keyboard([u8; KEYBOARD_REPORT_SIZE]),
}

pub trait ReportSink {
    type Error;
    fn submit(&mut self, report: HidReport) -> Result<(), Self::Error>;
}

pub trait ReportNotification {
    type Error;
    fn notify(&mut self) -> Result<(), Self::Error>;
}

pub struct RingReportSink<'a, MouseNotify, KeyboardNotify> {
    pub mouse: &'a mut SharedReportRing,
    pub keyboard: &'a mut SharedReportRing,
    pub mouse_notify: MouseNotify,
    pub keyboard_notify: KeyboardNotify,
}

#[derive(Debug)]
pub enum RingSinkError<E> {
    Ring(RingError),
    Notify(E),
}

impl<MouseNotify, KeyboardNotify, E> ReportSink for RingReportSink<'_, MouseNotify, KeyboardNotify>
where
    MouseNotify: ReportNotification<Error = E>,
    KeyboardNotify: ReportNotification<Error = E>,
{
    type Error = RingSinkError<E>;

    fn submit(&mut self, report: HidReport) -> Result<(), Self::Error> {
        let (ring, notify, bytes): (
            &mut SharedReportRing,
            &mut dyn ReportNotification<Error = E>,
            &[u8],
        ) = match &report {
            HidReport::Mouse(bytes) => (self.mouse, &mut self.mouse_notify, bytes),
            HidReport::Keyboard(bytes) => (self.keyboard, &mut self.keyboard_notify, bytes),
        };
        ring.publish(bytes).map_err(RingSinkError::Ring)?;
        notify.notify().map_err(RingSinkError::Notify)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KeyboardLeds(u8);

impl KeyboardLeds {
    pub fn decode(report: &[u8]) -> Result<Self, InputError> {
        if report.len() != KEYBOARD_LED_REPORT_SIZE || report[0] & !0x1f != 0 {
            return Err(InputError::InvalidLedReport);
        }
        Ok(Self(report[0]))
    }

    #[must_use]
    pub const fn mask(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VirtualInputState {
    keyboard: [u8; KEYBOARD_REPORT_SIZE],
    mouse_buttons: u8,
}

#[derive(Default)]
pub struct InputSession {
    generation: u64,
    reliable_sequence: u64,
    pointer_mode: Option<PointerMode>,
    state: VirtualInputState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerUpdate {
    Absolute { x: u16, y: u16 },
    Relative { delta_x: i64, delta_y: i64 },
}

impl InputSession {
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn pointer_mode(&self) -> Option<PointerMode> {
        self.pointer_mode
    }

    #[must_use]
    pub const fn reliable_sequence(&self) -> u64 {
        self.reliable_sequence
    }

    pub fn reliable<S: ReportSink>(
        &mut self,
        transition: InputTransition,
        sink: &mut S,
    ) -> Result<InputAck, InputSessionError<S::Error>> {
        let player_sent_at_micros = transition.player_sent_at_micros;
        if transition.generation == 0 || transition.sequence == 0 {
            return Err(InputSessionError::InvalidTransition);
        }
        if transition.generation < self.generation {
            return Err(InputSessionError::StaleTransition);
        }
        let new_generation = transition.generation > self.generation;
        let expected = if new_generation {
            0
        } else {
            self.reliable_sequence
        }
        .checked_add(1)
        .ok_or(InputSessionError::SequenceExhausted)?;
        if transition.sequence != expected {
            return Err(InputSessionError::OutOfOrder);
        }
        let action = transition
            .action
            .ok_or(InputSessionError::InvalidTransition)?;
        validate_action(action)?;
        if new_generation {
            self.state
                .release_all(sink)
                .map_err(InputSessionError::Apply)?;
            self.generation = transition.generation;
            self.reliable_sequence = 0;
            self.pointer_mode = None;
        }
        match action {
            input_transition::Action::Key(key) => {
                let usage = u16::try_from(key.hid_usage)
                    .map_err(|_| InputSessionError::InvalidTransition)?;
                self.state
                    .apply(
                        InputEvent::Key {
                            hid_usage: usage,
                            pressed: key.pressed,
                        },
                        sink,
                    )
                    .map_err(InputSessionError::Apply)?;
            }
            input_transition::Action::PointerButton(button) => {
                let pressed = button.pressed;
                let button = u8::try_from(button.button)
                    .map_err(|_| InputSessionError::InvalidTransition)?;
                self.state
                    .apply(InputEvent::PointerButton { button, pressed }, sink)
                    .map_err(InputSessionError::Apply)?;
            }
            input_transition::Action::Wheel(wheel) => {
                let horizontal = i16::try_from(wheel.horizontal)
                    .map_err(|_| InputSessionError::InvalidTransition)?;
                let vertical = i16::try_from(wheel.vertical)
                    .map_err(|_| InputSessionError::InvalidTransition)?;
                self.state
                    .apply(
                        InputEvent::Wheel {
                            horizontal,
                            vertical,
                        },
                        sink,
                    )
                    .map_err(InputSessionError::Apply)?;
            }
            input_transition::Action::PointerMode(mode) => {
                let mode = PointerMode::try_from(mode.mode)
                    .ok()
                    .filter(|mode| *mode != PointerMode::Unspecified)
                    .ok_or(InputSessionError::InvalidTransition)?;
                self.pointer_mode = Some(mode);
            }
            input_transition::Action::ReleaseAll(_) => self
                .state
                .release_all(sink)
                .map_err(InputSessionError::Apply)?,
        }
        self.reliable_sequence = transition.sequence;
        Ok(InputAck {
            generation: self.generation,
            through_sequence: self.reliable_sequence,
            player_sent_at_micros,
            host_received_at_micros: 0,
            host_submitted_at_micros: 0,
            pointer_datagrams_received: 0,
            pointer_updates_applied: 0,
            pointer_updates_ignored: 0,
            mouse_reports_published: 0,
            keyboard_reports_published: 0,
            reliable_transitions_received: 0,
            reliable_transitions_applied: 0,
            reliable_transitions_rejected: 0,
            reliable_transitions_missing: 0,
            reliable_transitions_duplicate_or_late: 0,
            release_all_transitions: 0,
            pointer_missing_datagrams: 0,
            pointer_stale_generations: 0,
            pointer_duplicate_or_late: 0,
            pointer_mode_rejections: 0,
            pointer_relative_baselines: 0,
        })
    }

    pub fn pointer<S: ReportSink>(
        &mut self,
        update: PointerUpdate,
        sink: &mut S,
    ) -> Result<(), InputSessionError<S::Error>> {
        let event = match update {
            PointerUpdate::Absolute { x, y }
                if self.pointer_mode == Some(PointerMode::Absolute) =>
            {
                InputEvent::PointerPosition { x, y }
            }
            PointerUpdate::Relative { delta_x, delta_y }
                if self.pointer_mode == Some(PointerMode::Relative) =>
            {
                InputEvent::PointerMotion {
                    delta_x: i32::try_from(delta_x)
                        .map_err(|_| InputSessionError::InvalidPointer)?,
                    delta_y: i32::try_from(delta_y)
                        .map_err(|_| InputSessionError::InvalidPointer)?,
                }
            }
            _ => return Err(InputSessionError::InvalidPointer),
        };
        self.state
            .apply(event, sink)
            .map_err(InputSessionError::Apply)
    }

    pub fn close<S: ReportSink>(
        &mut self,
        sink: &mut S,
    ) -> Result<(), InputSessionError<S::Error>> {
        self.state
            .release_all(sink)
            .map_err(InputSessionError::Apply)
    }
}

fn validate_action<E>(action: input_transition::Action) -> Result<(), InputSessionError<E>> {
    match action {
        input_transition::Action::Key(key)
            if u16::try_from(key.hid_usage)
                .ok()
                .is_some_and(|usage| matches!(usage, 1..=0xe7)) =>
        {
            Ok(())
        }
        input_transition::Action::PointerButton(button) if (1..=5).contains(&button.button) => {
            Ok(())
        }
        input_transition::Action::Wheel(wheel)
            if i16::try_from(wheel.horizontal).is_ok() && i16::try_from(wheel.vertical).is_ok() =>
        {
            Ok(())
        }
        input_transition::Action::PointerMode(mode)
            if PointerMode::try_from(mode.mode)
                .is_ok_and(|mode| mode != PointerMode::Unspecified) =>
        {
            Ok(())
        }
        input_transition::Action::ReleaseAll(_) => Ok(()),
        _ => Err(InputSessionError::InvalidTransition),
    }
}

#[derive(Debug)]
pub enum InputSessionError<E> {
    InvalidTransition,
    StaleTransition,
    OutOfOrder,
    SequenceExhausted,
    InvalidPointer,
    Apply(ApplyError<E>),
}

impl VirtualInputState {
    pub fn apply<S: ReportSink>(
        &mut self,
        event: InputEvent,
        sink: &mut S,
    ) -> Result<(), ApplyError<S::Error>> {
        match event {
            InputEvent::Key { hid_usage, pressed } => self.key(hid_usage, pressed, sink),
            InputEvent::ReleaseAll => self.release_all(sink),
            InputEvent::PointerButton { button, pressed } => self.button(button, pressed, sink),
            InputEvent::PointerMotion { delta_x, delta_y } => self.relative(delta_x, delta_y, sink),
            InputEvent::PointerPosition { x, y } => sink
                .submit(HidReport::Mouse(mouse_absolute(self.mouse_buttons, x, y)))
                .map_err(ApplyError::Sink),
            InputEvent::Wheel {
                horizontal,
                vertical,
            } => self.wheel(i32::from(horizontal), i32::from(vertical), sink),
        }
    }

    fn key<S: ReportSink>(
        &mut self,
        usage: u16,
        pressed: bool,
        sink: &mut S,
    ) -> Result<(), ApplyError<S::Error>> {
        let mut next = self.keyboard;
        let (byte, bit) = match usage {
            0xe0..=0xe7 => (0, (usage - 0xe0) as u8),
            1..=0xdf => (1 + usize::from(usage / 8), (usage % 8) as u8),
            _ => return Err(ApplyError::Input(InputError::UnsupportedKeyUsage(usage))),
        };
        if pressed {
            next[byte] |= 1 << bit;
        } else {
            next[byte] &= !(1 << bit);
        }
        if next == self.keyboard {
            return Ok(());
        }
        sink.submit(HidReport::Keyboard(next))
            .map_err(ApplyError::Sink)?;
        self.keyboard = next;
        Ok(())
    }

    fn button<S: ReportSink>(
        &mut self,
        button: u8,
        pressed: bool,
        sink: &mut S,
    ) -> Result<(), ApplyError<S::Error>> {
        if !(1..=5).contains(&button) {
            return Err(ApplyError::Input(InputError::UnsupportedMouseButton(
                button,
            )));
        }
        let bit = 1 << (button - 1);
        let next = if pressed {
            self.mouse_buttons | bit
        } else {
            self.mouse_buttons & !bit
        };
        if next == self.mouse_buttons {
            return Ok(());
        }
        sink.submit(HidReport::Mouse(mouse_relative(next, 0, 0, 0, 0)))
            .map_err(ApplyError::Sink)?;
        self.mouse_buttons = next;
        Ok(())
    }

    fn relative<S: ReportSink>(
        &self,
        mut x: i32,
        mut y: i32,
        sink: &mut S,
    ) -> Result<(), ApplyError<S::Error>> {
        while x != 0 || y != 0 {
            let part_x = x.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
            let part_y = y.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
            sink.submit(HidReport::Mouse(mouse_relative(
                self.mouse_buttons,
                part_x,
                part_y,
                0,
                0,
            )))
            .map_err(ApplyError::Sink)?;
            x -= i32::from(part_x);
            y -= i32::from(part_y);
        }
        Ok(())
    }

    fn wheel<S: ReportSink>(
        &self,
        mut horizontal: i32,
        mut vertical: i32,
        sink: &mut S,
    ) -> Result<(), ApplyError<S::Error>> {
        while horizontal != 0 || vertical != 0 {
            let h = horizontal.clamp(-127, 127) as i8;
            let v = vertical.clamp(-127, 127) as i8;
            sink.submit(HidReport::Mouse(mouse_relative(
                self.mouse_buttons,
                0,
                0,
                v,
                h,
            )))
            .map_err(ApplyError::Sink)?;
            horizontal -= i32::from(h);
            vertical -= i32::from(v);
        }
        Ok(())
    }

    pub fn release_all<S: ReportSink>(&mut self, sink: &mut S) -> Result<(), ApplyError<S::Error>> {
        if self.keyboard != [0; KEYBOARD_REPORT_SIZE] {
            sink.submit(HidReport::Keyboard([0; KEYBOARD_REPORT_SIZE]))
                .map_err(ApplyError::Sink)?;
            self.keyboard = [0; KEYBOARD_REPORT_SIZE];
        }
        if self.mouse_buttons != 0 {
            sink.submit(HidReport::Mouse(mouse_relative(0, 0, 0, 0, 0)))
                .map_err(ApplyError::Sink)?;
            self.mouse_buttons = 0;
        }
        Ok(())
    }
}

fn mouse_relative(buttons: u8, x: i16, y: i16, wheel: i8, pan: i8) -> [u8; 8] {
    let mut report = [0; 8];
    report[0] = 1;
    report[1] = buttons;
    report[2..4].copy_from_slice(&x.to_le_bytes());
    report[4..6].copy_from_slice(&y.to_le_bytes());
    report[6] = wheel as u8;
    report[7] = pan as u8;
    report
}

fn mouse_absolute(buttons: u8, x: u16, y: u16) -> [u8; 8] {
    let mut report = [0; 8];
    report[0] = 2;
    report[1] = buttons;
    report[2..4].copy_from_slice(&x.min(32767).to_le_bytes());
    report[4..6].copy_from_slice(&y.min(32767).to_le_bytes());
    report
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputError {
    UnsupportedKeyUsage(u16),
    UnsupportedMouseButton(u8),
    InvalidLedReport,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ApplyError<E> {
    Input(InputError),
    Sink(E),
}

impl<E: fmt::Display> fmt::Display for ApplyError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(error) => write!(formatter, "invalid input: {error:?}"),
            Self::Sink(error) => write!(formatter, "input report submission failed: {error}"),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for ApplyError<E> {}

#[cfg(test)]
mod tests {
    use super::*;
    use rustconsole_protocol::wire::{KeyTransition, PointerModeTransition, input_transition};
    use rustconsole_session::input_datagram::{PointerSnapshot, PointerSnapshotReceiver};

    #[derive(Default)]
    struct Sink {
        reports: Vec<HidReport>,
        fail: bool,
    }

    impl ReportSink for Sink {
        type Error = &'static str;
        fn submit(&mut self, report: HidReport) -> Result<(), Self::Error> {
            if self.fail {
                Err("full")
            } else {
                self.reports.push(report);
                Ok(())
            }
        }
    }

    #[test]
    fn nkro_tracks_many_keys_and_both_modifier_sides() {
        let mut state = VirtualInputState::default();
        let mut sink = Sink::default();
        for usage in 4..=20 {
            state
                .apply(
                    InputEvent::Key {
                        hid_usage: usage,
                        pressed: true,
                    },
                    &mut sink,
                )
                .unwrap();
        }
        state
            .apply(
                InputEvent::Key {
                    hid_usage: 0xe0,
                    pressed: true,
                },
                &mut sink,
            )
            .unwrap();
        state
            .apply(
                InputEvent::Key {
                    hid_usage: 0xe7,
                    pressed: true,
                },
                &mut sink,
            )
            .unwrap();
        let HidReport::Keyboard(report) = sink.reports.last().unwrap() else {
            panic!("expected keyboard report")
        };
        assert_eq!(report[0], 0x81);
        assert!((4..=20).all(|usage| report[1 + usage / 8] & (1 << (usage % 8)) != 0));
    }

    #[test]
    fn failed_transition_does_not_change_authoritative_state() {
        let mut state = VirtualInputState::default();
        let mut sink = Sink {
            fail: true,
            ..Sink::default()
        };
        assert!(
            state
                .apply(
                    InputEvent::PointerButton {
                        button: 1,
                        pressed: true
                    },
                    &mut sink
                )
                .is_err()
        );
        sink.fail = false;
        state
            .apply(InputEvent::PointerPosition { x: 4, y: 5 }, &mut sink)
            .unwrap();
        assert_eq!(sink.reports, [HidReport::Mouse(mouse_absolute(0, 4, 5))]);
    }

    #[test]
    fn large_relative_and_wheel_values_are_split_without_loss() {
        let mut state = VirtualInputState::default();
        let mut sink = Sink::default();
        state
            .apply(
                InputEvent::PointerMotion {
                    delta_x: 70_000,
                    delta_y: -70_000,
                },
                &mut sink,
            )
            .unwrap();
        assert_eq!(sink.reports.len(), 3);
        sink.reports.clear();
        state.wheel(-300, 300, &mut sink).unwrap();
        assert_eq!(sink.reports.len(), 3);
    }

    #[test]
    fn release_all_clears_both_devices_and_leds_are_strict() {
        let mut state = VirtualInputState::default();
        let mut sink = Sink::default();
        state
            .apply(
                InputEvent::Key {
                    hid_usage: 4,
                    pressed: true,
                },
                &mut sink,
            )
            .unwrap();
        state
            .apply(
                InputEvent::PointerButton {
                    button: 2,
                    pressed: true,
                },
                &mut sink,
            )
            .unwrap();
        state.release_all(&mut sink).unwrap();
        assert_eq!(
            sink.reports[sink.reports.len() - 2],
            HidReport::Keyboard([0; 29])
        );
        assert_eq!(
            sink.reports.last(),
            Some(&HidReport::Mouse(mouse_relative(0, 0, 0, 0, 0)))
        );
        assert_eq!(KeyboardLeds::decode(&[0x07]).unwrap().mask(), 7);
        assert!(KeyboardLeds::decode(&[0x20]).is_err());
    }

    #[test]
    fn session_acks_only_contiguous_applied_transitions() {
        let mut session = InputSession::default();
        let mut sink = Sink::default();
        let transition = |sequence, pressed| InputTransition {
            generation: 7,
            sequence,
            action: Some(input_transition::Action::Key(KeyTransition {
                hid_usage: 4,
                pressed,
            })),
            player_sent_at_micros: 0,
        };
        assert_eq!(
            session.reliable(transition(1, true), &mut sink).unwrap(),
            InputAck {
                generation: 7,
                through_sequence: 1,
                ..InputAck::default()
            }
        );
        assert!(matches!(
            session.reliable(transition(3, false), &mut sink),
            Err(InputSessionError::OutOfOrder)
        ));
        assert_eq!(sink.reports.len(), 1);
        assert_eq!(
            session.reliable(transition(2, false), &mut sink).unwrap(),
            InputAck {
                generation: 7,
                through_sequence: 2,
                ..InputAck::default()
            }
        );
        session.reliable(transition(3, true), &mut sink).unwrap();
        let report_count = sink.reports.len();
        assert!(matches!(
            session.reliable(
                InputTransition {
                    generation: 8,
                    sequence: 1,
                    action: None,
                    player_sent_at_micros: 0,
                },
                &mut sink,
            ),
            Err(InputSessionError::InvalidTransition)
        ));
        assert_eq!(session.generation(), 7);
        assert_eq!(sink.reports.len(), report_count);
    }

    #[test]
    fn session_recovers_relative_loss_and_releases_on_close() {
        let mut session = InputSession::default();
        let mut sink = Sink::default();
        session
            .reliable(
                InputTransition {
                    generation: 4,
                    sequence: 1,
                    action: Some(input_transition::Action::PointerMode(
                        PointerModeTransition {
                            mode: PointerMode::Relative as i32,
                        },
                    )),
                    player_sent_at_micros: 0,
                },
                &mut sink,
            )
            .unwrap();
        assert_eq!(session.generation(), 4);
        assert_eq!(session.pointer_mode(), Some(PointerMode::Relative));
        let mut receiver = PointerSnapshotReceiver::default();
        assert!(
            receiver
                .push(PointerSnapshot::Relative {
                    generation: 4,
                    sequence: 1,
                    cumulative_x: 10,
                    cumulative_y: -2,
                })
                .unwrap()
                .is_none()
        );
        let update = receiver
            .push(PointerSnapshot::Relative {
                generation: 4,
                sequence: 3,
                cumulative_x: 25,
                cumulative_y: 8,
            })
            .unwrap()
            .unwrap();
        session
            .pointer(
                match update {
                    rustconsole_session::input_datagram::PointerUpdate::Absolute { x, y } => {
                        PointerUpdate::Absolute { x, y }
                    }
                    rustconsole_session::input_datagram::PointerUpdate::Relative {
                        delta_x,
                        delta_y,
                    } => PointerUpdate::Relative { delta_x, delta_y },
                },
                &mut sink,
            )
            .unwrap();
        assert_eq!(
            sink.reports,
            [HidReport::Mouse(mouse_relative(0, 15, 10, 0, 0))]
        );
        session.close(&mut sink).unwrap();
    }
}
