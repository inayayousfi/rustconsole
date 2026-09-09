use rustconsole_media::VideoFormat;

const REQUIRED_PRESENTED_FRAMES: u8 = 3;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisplayModeSnapshot {
    pub display: String,
    pub format: VideoFormat,
    pub dxgi_format: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisplayModePhase {
    InitialCaptured,
    ChangedCaptured,
}

pub struct DisplayModeProgress {
    initial: DisplayModeSnapshot,
    changed: Option<DisplayModeSnapshot>,
    candidate: Option<DisplayModeSnapshot>,
    stage: u8,
    presented_frames: u8,
    generation: u64,
    changed_generation: u64,
}

impl DisplayModeProgress {
    pub fn new(initial: DisplayModeSnapshot) -> Self {
        Self {
            initial,
            changed: None,
            candidate: None,
            stage: 0,
            presented_frames: 0,
            generation: 0,
            changed_generation: 0,
        }
    }

    pub fn reinitialized(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.candidate = None;
        self.presented_frames = 0;
    }

    pub fn presented(&mut self, snapshot: &DisplayModeSnapshot) -> Option<DisplayModePhase> {
        match self.stage {
            0 if snapshot == &self.initial => {
                if self.count(snapshot) {
                    self.stage = 1;
                    Some(DisplayModePhase::InitialCaptured)
                } else {
                    None
                }
            }
            1 if self.generation > 0 && snapshot.format != self.initial.format => {
                if self.count(snapshot) {
                    self.changed = Some(snapshot.clone());
                    self.changed_generation = self.generation;
                    self.stage = 2;
                    Some(DisplayModePhase::ChangedCaptured)
                } else {
                    None
                }
            }
            2 if self.generation > self.changed_generation && snapshot == &self.initial => {
                if self.count(snapshot) {
                    self.stage = 3;
                }
                None
            }
            _ => {
                self.candidate = None;
                self.presented_frames = 0;
                None
            }
        }
    }

    fn count(&mut self, snapshot: &DisplayModeSnapshot) -> bool {
        if self.candidate.as_ref() != Some(snapshot) {
            self.candidate = Some(snapshot.clone());
            self.presented_frames = 0;
        }
        self.presented_frames = self.presented_frames.saturating_add(1);
        self.presented_frames >= REQUIRED_PRESENTED_FRAMES
    }

    pub const fn complete(&self) -> bool {
        self.stage >= 3
    }

    pub const fn initial(&self) -> &DisplayModeSnapshot {
        &self.initial
    }

    pub fn changed(&self) -> Option<&DisplayModeSnapshot> {
        self.changed.as_ref()
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(width: u32, height: u32, frames_per_second: u16) -> DisplayModeSnapshot {
        DisplayModeSnapshot {
            display: r"\\.\DISPLAY1".to_owned(),
            format: VideoFormat {
                width,
                height,
                frames_per_second,
            },
            dxgi_format: 87,
        }
    }

    fn present_three(
        progress: &mut DisplayModeProgress,
        snapshot: &DisplayModeSnapshot,
    ) -> Option<DisplayModePhase> {
        progress.presented(snapshot);
        progress.presented(snapshot);
        progress.presented(snapshot)
    }

    #[test]
    fn proof_requires_initial_changed_and_restored_presentations() {
        let initial = mode(2560, 1600, 240);
        let changed = mode(2560, 1600, 120);
        let mut progress = DisplayModeProgress::new(initial.clone());

        assert_eq!(progress.initial(), &initial);
        assert_eq!(
            present_three(&mut progress, &initial),
            Some(DisplayModePhase::InitialCaptured)
        );
        assert!(!progress.complete());

        progress.reinitialized();
        assert_eq!(
            present_three(&mut progress, &changed),
            Some(DisplayModePhase::ChangedCaptured)
        );
        assert!(!progress.complete());

        progress.reinitialized();
        assert_eq!(present_three(&mut progress, &initial), None);
        assert!(progress.complete());
        assert_eq!(progress.changed(), Some(&changed));
        assert_eq!(progress.generation(), 2);
    }

    #[test]
    fn changed_mode_is_rejected_without_resource_reinitialization() {
        let initial = mode(2560, 1600, 240);
        let changed = mode(2560, 1600, 120);
        let mut progress = DisplayModeProgress::new(initial.clone());

        present_three(&mut progress, &initial);
        assert_eq!(present_three(&mut progress, &changed), None);
        assert!(progress.changed().is_none());
    }

    #[test]
    fn restoration_requires_another_resource_reinitialization() {
        let initial = mode(2560, 1600, 240);
        let changed = mode(2560, 1600, 120);
        let mut progress = DisplayModeProgress::new(initial.clone());

        present_three(&mut progress, &initial);
        progress.reinitialized();
        present_three(&mut progress, &changed);
        assert_eq!(present_three(&mut progress, &initial), None);
        assert!(!progress.complete());
    }
}
