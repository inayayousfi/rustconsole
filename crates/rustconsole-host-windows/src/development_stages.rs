//! Median per-stage frame timing for the development host, written once per 120 frames.

use std::fs::OpenOptions;
use std::io::Write;

const FRAMES_PER_LINE: usize = 120;
const REPORT: &str = r"C:\ProgramData\RustConsole\development-stages.txt";

#[derive(Default)]
pub struct StageLog {
    samples: [Vec<u64>; 5],
}

impl StageLog {
    pub fn observe(&mut self, stages: [u64; 5]) {
        for (samples, value) in self.samples.iter_mut().zip(stages) {
            samples.push(value);
        }
        if self.samples[0].len() < FRAMES_PER_LINE {
            return;
        }
        let medians = self.samples.each_mut().map(|samples| {
            samples.sort_unstable();
            let median = samples[samples.len() / 2];
            samples.clear();
            median
        });
        if let Ok(mut report) = OpenOptions::new().create(true).append(true).open(REPORT) {
            let [acquire, copy, color, encode, total] = medians;
            let _ = writeln!(
                report,
                "acquire={acquire} copy={copy} color={color} encoder={encode} total={total}"
            );
        }
    }
}
