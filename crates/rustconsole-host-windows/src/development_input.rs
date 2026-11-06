//! Input stand-in for the development host. The installed service owns the virtual input
//! devices, so a test host running beside it accepts input messages and discards them.

use rustconsole_input_windows::{HidReport, KeyboardLeds, ReportSink, WindowsRingError};
use std::convert::Infallible;
use std::path::Path;

pub struct DiscardReports;

impl ReportSink for DiscardReports {
    type Error = Infallible;

    fn submit(&mut self, _report: HidReport) -> Result<(), Self::Error> {
        Ok(())
    }
}

pub struct VirtualInputOwner {
    generation: u64,
    sink: DiscardReports,
}

impl VirtualInputOwner {
    pub fn create(_inf_path: &Path, generation: u64) -> Result<Self, Infallible> {
        Ok(Self {
            generation,
            sink: DiscardReports,
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn sink_mut(&mut self) -> &mut DiscardReports {
        &mut self.sink
    }

    pub fn drain_keyboard_leds(&mut self) -> Result<Vec<(u64, KeyboardLeds)>, WindowsRingError> {
        Ok(Vec::new())
    }
}
