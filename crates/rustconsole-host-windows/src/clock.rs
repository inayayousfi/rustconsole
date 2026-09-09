fn ticks_to_micros(ticks: i64, frequency: i64) -> Result<u64, &'static str> {
    if ticks < 0 || frequency <= 0 {
        return Err("invalid Windows performance-counter value");
    }
    u64::try_from(ticks as u128 * 1_000_000 / frequency as u128)
        .map_err(|_| "performance-counter timestamp overflow")
}

#[cfg(windows)]
pub struct HostClock {
    frequency: i64,
}

#[cfg(windows)]
impl HostClock {
    pub fn new() -> std::io::Result<Self> {
        let mut frequency = 0;
        // SAFETY: valid output storage; the frequency is constant for this boot.
        unsafe { windows::Win32::System::Performance::QueryPerformanceFrequency(&mut frequency) }
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        ticks_to_micros(0, frequency).map_err(std::io::Error::other)?;
        Ok(Self { frequency })
    }

    pub fn ticks_to_micros(&self, ticks: i64) -> std::io::Result<u64> {
        ticks_to_micros(ticks, self.frequency).map_err(std::io::Error::other)
    }

    pub fn now(&self) -> std::io::Result<u64> {
        let mut ticks = 0;
        // SAFETY: valid output storage for the monotonic counter.
        unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&mut ticks) }
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        self.ticks_to_micros(ticks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clock_units_are_normalized_without_overflow() {
        assert_eq!(ticks_to_micros(10_000_000, 10_000_000), Ok(1_000_000));
        assert_eq!(ticks_to_micros(3_579_545, 3_579_545), Ok(1_000_000));
        assert!(ticks_to_micros(-1, 10).is_err());
        assert!(ticks_to_micros(1, 0).is_err());
        assert!(ticks_to_micros(i64::MAX, 1).is_err());
        assert_eq!(
            ticks_to_micros(i64::MAX, 10_000_000),
            Ok(i64::MAX as u64 / 10)
        );
    }
}
