#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VideoColorMode {
    #[default]
    Bt709LimitedToSrgb = 0,
    Bt2020PqLimitedToHdr10 = 1,
    Bt2020PqLimitedToSrgbBt2390 = 2,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VideoColorParameters {
    mode: VideoColorMode,
    source_peak_nits: f32,
    target_peak_nits: f32,
    target_black_nits: f32,
}

impl VideoColorParameters {
    #[must_use]
    pub const fn bt709_limited_to_srgb() -> Self {
        Self {
            mode: VideoColorMode::Bt709LimitedToSrgb,
            source_peak_nits: 0.0,
            target_peak_nits: 0.0,
            target_black_nits: 0.0,
        }
    }

    #[must_use]
    pub const fn bt2020_pq_limited_to_hdr10(source_peak_nits: f32) -> Self {
        Self {
            mode: VideoColorMode::Bt2020PqLimitedToHdr10,
            source_peak_nits,
            target_peak_nits: source_peak_nits,
            target_black_nits: 0.0,
        }
    }

    #[must_use]
    pub const fn bt2020_pq_limited_to_srgb_bt2390(
        source_peak_nits: f32,
        target_peak_nits: f32,
        target_black_nits: f32,
    ) -> Self {
        Self {
            mode: VideoColorMode::Bt2020PqLimitedToSrgbBt2390,
            source_peak_nits,
            target_peak_nits,
            target_black_nits,
        }
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        // SAFETY: repr(C) fixes this four-word push-constant layout and the
        // returned slice cannot outlive the borrowed value.
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref(self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

impl Default for VideoColorParameters {
    fn default() -> Self {
        Self::bt709_limited_to_srgb()
    }
}

const _: () = assert!(std::mem::size_of::<VideoColorParameters>() == 16);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_parameters_match_the_shader_push_constant_layout() {
        let parameters =
            VideoColorParameters::bt2020_pq_limited_to_srgb_bt2390(1_000.0, 203.0, 0.203);
        assert_eq!(parameters.as_bytes().len(), 16);
        assert_eq!(parameters.mode, VideoColorMode::Bt2020PqLimitedToSrgbBt2390);
        assert_eq!(parameters.source_peak_nits, 1_000.0);
        assert_eq!(parameters.target_peak_nits, 203.0);
        assert_eq!(parameters.target_black_nits, 0.203);
    }

    #[test]
    fn bt709_limited_endpoints_become_linear_black_and_white() {
        assert!((bt709_limited_luma(16.0 / 255.0) - 0.0).abs() < 1e-6);
        assert!((bt709_limited_luma(235.0 / 255.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn pq_reference_points_match_st_2084() {
        let hundred = pq_encode(100.0);
        let thousand = pq_encode(1_000.0);
        assert!((hundred - 0.508_078_4).abs() < 1e-6, "{hundred}");
        assert!((thousand - 0.751_827_1).abs() < 3e-6, "{thousand}");
        assert!((pq_decode(pq_encode(100.0)) - 100.0).abs() < 0.001);
    }

    #[test]
    fn bt2390_is_monotone_and_maps_source_peak_to_target_peak() {
        let values = [0.001, 1.0, 10.0, 100.0, 400.0, 1_000.0]
            .map(|nits| pq_decode(bt2390_curve(pq_encode(nits), 1_000.0, 203.0, 0.203)));
        assert!(values.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!((values[5] - 203.0).abs() < 0.01);
        assert!(
            values.iter().all(|value| (0.2029..=203.01).contains(value)),
            "{values:?}"
        );
    }

    fn bt709_limited_luma(sample: f32) -> f32 {
        let encoded = (sample * 255.0 - 16.0) / 219.0;
        if encoded < 0.081 {
            encoded / 4.5
        } else {
            ((encoded + 0.099) / 1.099).powf(1.0 / 0.45)
        }
    }

    fn pq_encode(nits: f32) -> f32 {
        let m1 = 2610.0 / 16384.0;
        let m2 = 2523.0 / 32.0;
        let c1 = 3424.0 / 4096.0;
        let c2 = 2413.0 / 128.0;
        let c3 = 2392.0 / 128.0;
        let p = (nits.max(0.0) / 10_000.0).powf(m1);
        ((c1 + c2 * p) / (1.0 + c3 * p)).powf(m2)
    }

    fn pq_decode(encoded: f32) -> f32 {
        let m1 = 2610.0 / 16384.0;
        let m2 = 2523.0 / 32.0;
        let c1 = 3424.0 / 4096.0;
        let c2 = 2413.0 / 128.0;
        let c3 = 2392.0 / 128.0;
        let p = encoded.max(0.0).powf(1.0 / m2);
        10_000.0 * ((p - c1).max(0.0) / (c2 - c3 * p)).powf(1.0 / m1)
    }

    fn bt2390_curve(
        encoded_luminance: f32,
        source_peak_nits: f32,
        target_peak_nits: f32,
        target_black_nits: f32,
    ) -> f32 {
        let output_white = pq_encode(target_peak_nits);
        let output_black = pq_encode(target_black_nits);
        let input_white = pq_encode(source_peak_nits).max(output_white + 0.001);
        let input_black = pq_encode(0.001).min(output_black - 0.001);
        let minimum = (output_black - input_black) / (input_white - input_black);
        let maximum = (output_white - input_black) / (input_white - input_black);
        let knee = 1.5 * maximum - 0.5;
        let mut mapped = (encoded_luminance - input_black) / (input_white - input_black);
        if mapped >= knee {
            let t = (mapped - knee) / (1.0 - knee);
            let t2 = t * t;
            let t3 = t2 * t;
            mapped = (2.0 * t3 - 3.0 * t2 + 1.0) * knee
                + (t3 - 2.0 * t2 + t) * (1.0 - knee)
                + (-2.0 * t3 + 3.0 * t2) * maximum;
        }
        mapped += minimum * (1.0 - mapped).powi(4);
        (mapped * (input_white - input_black) + input_black).clamp(output_black, output_white)
    }
}
