//! Median per-stage timing of fresh frames for the development host, written once per 120 frames.

use std::fs::OpenOptions;
use std::io::Write;

const FRAMES_PER_LINE: usize = 120;
const REPORT: &str = r"C:\ProgramData\RustConsole\development-stages.txt";

#[derive(Default)]
pub struct StageLog {
    samples: [Vec<u64>; 7],
    last_present_time: i64,
    repeats: u32,
}

impl StageLog {
    pub fn observe(&mut self, present_time: i64, stages: [u64; 7]) {
        if present_time == self.last_present_time {
            self.repeats += 1;
        } else {
            self.last_present_time = present_time;
            for (samples, value) in self.samples.iter_mut().zip(stages) {
                samples.push(value);
            }
        }
        if self.samples[0].len() + self.repeats as usize <= FRAMES_PER_LINE {
            return;
        }
        let medians = self.samples.each_mut().map(|samples| {
            samples.sort_unstable();
            let median = samples.get(samples.len() / 2).copied().unwrap_or(0);
            samples.clear();
            median
        });
        if let Ok(mut report) = OpenOptions::new().create(true).append(true).open(REPORT) {
            let [acquire, copy, color, encode, total, age, taken] = medians;
            let _ = writeln!(
                report,
                "acquire={acquire} copy={copy} color={color} encoder={encode} total={total} \
                 age={age} taken={taken} repeats={}",
                self.repeats
            );
        }
        self.repeats = 0;
    }
}
