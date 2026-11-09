//! Detects pointer buttons that the desktop holds back until release.
//!
//! Some desktop input settings, such as libinput button scrolling, keep a
//! button press until the button is released and then deliver the press and
//! the release together. The host then sees an instant click instead of a held
//! button, so the player tells the user once instead of failing silently.

/// No person can press and release a button this fast. A press and release
/// closer than this were delivered together after the system held them back.
const HELD_BACK_GAP_NANOSECONDS: u64 = 2_000_000;

/// Remembers the timestamp of each pressed button, numbered 1 to 5.
#[derive(Default)]
pub struct HeldButtonCheck {
    pressed_at: [Option<u64>; 5],
    reported: bool,
}

impl HeldButtonCheck {
    pub fn press(&mut self, button: u8, timestamp_nanoseconds: u64) {
        if let Some(pressed_at) = self.slot(button) {
            *pressed_at = Some(timestamp_nanoseconds);
        }
    }

    /// Returns the button the first time its press arrived together with its
    /// release, and `None` for every other release.
    pub fn release(&mut self, button: u8, timestamp_nanoseconds: u64) -> Option<u8> {
        let pressed_at = self.slot(button)?.take()?;
        if self.reported
            || timestamp_nanoseconds.saturating_sub(pressed_at) >= HELD_BACK_GAP_NANOSECONDS
        {
            return None;
        }
        self.reported = true;
        Some(button)
    }

    fn slot(&mut self, button: u8) -> Option<&mut Option<u64>> {
        self.pressed_at.get_mut(usize::from(button.checked_sub(1)?))
    }
}

/// The message shown after `HeldButtonCheck` reports a button.
pub fn held_button_notice(button: u8) -> String {
    let name = match button {
        1 => "Left",
        2 => "Right",
        3 => "Middle",
        4 => "Back",
        _ => "Forward",
    };
    format!(
        "{name} button arrives only when released:\na desktop setting such as button scrolling holds it back."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MILLISECOND: u64 = 1_000_000;

    #[test]
    fn a_press_delivered_with_its_release_is_reported_once() {
        let mut check = HeldButtonCheck::default();
        check.press(3, 1_000 * MILLISECOND);
        assert_eq!(check.release(3, 1_000 * MILLISECOND + 14_000), Some(3));

        check.press(3, 2_000 * MILLISECOND);
        assert_eq!(check.release(3, 2_000 * MILLISECOND + 14_000), None);
    }

    #[test]
    fn a_real_click_or_hold_is_not_reported() {
        let mut check = HeldButtonCheck::default();
        check.press(1, 1_000 * MILLISECOND);
        assert_eq!(check.release(1, 1_030 * MILLISECOND), None);
        check.press(3, 2_000 * MILLISECOND);
        assert_eq!(check.release(3, 3_000 * MILLISECOND), None);

        check.press(3, 4_000 * MILLISECOND);
        assert_eq!(check.release(3, 4_000 * MILLISECOND), Some(3));
    }
}
