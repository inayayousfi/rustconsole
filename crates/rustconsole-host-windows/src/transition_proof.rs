#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionGoal {
    DesktopRoundTrip,
    Login,
}

impl TransitionGoal {
    pub const fn required_path(self) -> &'static str {
        match self {
            Self::DesktopRoundTrip => "Default->Winlogon->Default",
            Self::Login => "Winlogon->Default",
        }
    }

    const fn initial_desktop(self) -> &'static str {
        match self {
            Self::DesktopRoundTrip => "Default",
            Self::Login => "Winlogon",
        }
    }
}

pub struct TransitionProgress {
    goal: TransitionGoal,
    stage: u8,
    observed_path: Vec<String>,
}

impl TransitionProgress {
    pub fn new(goal: TransitionGoal, initial_desktop: &str) -> Result<Self, String> {
        if initial_desktop != goal.initial_desktop() {
            return Err(format!(
                "{} proof must start on {}, started on {initial_desktop}",
                match goal {
                    TransitionGoal::DesktopRoundTrip => "desktop round-trip",
                    TransitionGoal::Login => "login transition",
                },
                goal.initial_desktop()
            ));
        }
        Ok(Self {
            goal,
            stage: 0,
            observed_path: vec![initial_desktop.to_owned()],
        })
    }

    pub fn desktop_attached(&mut self, desktop: &str) {
        if self.observed_path.last().is_none_or(|last| last != desktop) {
            self.observed_path.push(desktop.to_owned());
        }
    }

    pub fn presented(&mut self, desktop: &str) {
        self.stage = match (self.goal, self.stage, desktop) {
            (TransitionGoal::DesktopRoundTrip, 0, "Default") => 1,
            (TransitionGoal::DesktopRoundTrip, 1, "Winlogon") => 2,
            (TransitionGoal::DesktopRoundTrip, 2, "Default") => 3,
            (TransitionGoal::Login, 0, "Winlogon") => 1,
            (TransitionGoal::Login, 1, "Default") => 2,
            _ => self.stage,
        };
    }

    pub const fn complete(&self) -> bool {
        self.stage
            >= match self.goal {
                TransitionGoal::DesktopRoundTrip => 3,
                TransitionGoal::Login => 2,
            }
    }

    pub fn observed_path(&self) -> String {
        self.observed_path.join("->")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_round_trip_requires_presented_frames_in_order() {
        assert_eq!(
            TransitionGoal::DesktopRoundTrip.required_path(),
            "Default->Winlogon->Default"
        );
        let mut progress =
            TransitionProgress::new(TransitionGoal::DesktopRoundTrip, "Default").unwrap();
        progress.presented("Default");
        progress.desktop_attached("Winlogon");
        progress.presented("Winlogon");

        assert!(!progress.complete());

        progress.desktop_attached("Default");
        progress.presented("Default");

        assert!(progress.complete());
        assert_eq!(progress.observed_path(), "Default->Winlogon->Default");
    }

    #[test]
    fn desktop_round_trip_does_not_skip_the_initial_presented_frame() {
        let mut progress =
            TransitionProgress::new(TransitionGoal::DesktopRoundTrip, "Default").unwrap();
        progress.desktop_attached("Winlogon");
        progress.presented("Winlogon");
        progress.desktop_attached("Default");
        progress.presented("Default");

        assert!(!progress.complete());
    }

    #[test]
    fn login_transition_requires_winlogon_then_default() {
        assert_eq!(TransitionGoal::Login.required_path(), "Winlogon->Default");
        assert!(TransitionProgress::new(TransitionGoal::Login, "Default").is_err());

        let mut progress = TransitionProgress::new(TransitionGoal::Login, "Winlogon").unwrap();
        progress.presented("Winlogon");
        progress.desktop_attached("Default");
        progress.presented("Default");

        assert!(progress.complete());
        assert_eq!(progress.observed_path(), "Winlogon->Default");
    }
}
