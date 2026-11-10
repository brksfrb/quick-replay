//! Encoder rate-control plan, platform independent so the rules are tested on Linux too.
//!
//! The Media Foundation encoder only spends the bitrate it is given when the rate-control mode
//! is bitrate-driven. In `Quality` mode (eAVEncCommonRateControlMode_Quality) the NVIDIA MFT
//! runs constant quality and treats `AVEncCommonMeanBitRate` as unused, so a 143 Mbps Maximum
//! target produced ~35 Mbps files. Maximum therefore asks for CBR (then peak-constrained or
//! unconstrained VBR) and treats the mean bitrate as part of the mode: a mode whose bitrate
//! property is rejected is not accepted.

use crate::config::Quality;

/// Mirrors eAVEncCommonRateControlMode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Cbr,
    PeakConstrainedVbr,
    UnconstrainedVbr,
    Quality,
}

/// One ICodecAPI property write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Property {
    RateControlMode(Mode),
    /// CODECAPI_AVEncCommonMeanBitRate, bits per second.
    MeanBitRate(u32),
    /// CODECAPI_AVEncCommonMaxBitRate, bits per second.
    MaxBitRate(u32),
    /// CODECAPI_AVEncCommonBufferSize, bits.
    BufferSize(u32),
    /// CODECAPI_AVEncCommonQuality, 0..=100.
    Quality(u32),
}

/// How the bitrate setting is used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Profile {
    /// Low preset: low latency, budgeted VBR.
    Fast,
    /// Medium / High / Custom: constant quality first; the bitrate only applies if the driver
    /// rejects Quality mode. Unchanged from v0.1.7.
    ConstantQuality,
    /// Maximum: the bitrate is the target and must reach the driver.
    TargetBitrate,
}

impl Profile {
    pub fn for_quality(quality: Quality) -> Profile {
        match quality {
            Quality::Low => Profile::Fast,
            Quality::Medium | Quality::High | Quality::Custom => Profile::ConstantQuality,
            Quality::Maximum => Profile::TargetBitrate,
        }
    }
}

/// A mode is accepted only if every `required` write succeeds. `optional` writes are best effort.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub mode: Mode,
    pub required: Vec<Property>,
    pub optional: Vec<Property>,
}

const CONSTANT_QUALITY: u32 = 93;
/// Maximum's last resort when the driver takes no bitrate-driven mode at all.
const LAST_RESORT_QUALITY: u32 = 100;

/// Maximum: the mean bitrate (and the peak for VBR) must be accepted together with the mode.
fn target_mode(mode: Mode, mean: u32, peak: u32) -> Candidate {
    let (required, optional) = match mode {
        Mode::Cbr => (
            vec![Property::RateControlMode(mode), Property::MeanBitRate(mean)],
            vec![Property::MaxBitRate(mean), Property::BufferSize(mean)],
        ),
        _ => (
            vec![Property::RateControlMode(mode), Property::MeanBitRate(mean), Property::MaxBitRate(peak)],
            vec![Property::BufferSize(mean)],
        ),
    };
    Candidate { mode, required, optional }
}

/// v0.1.7 behaviour for the other presets: the mode alone decides; the mean bitrate is written
/// best effort, plus a 2x peak for peak-constrained VBR.
fn legacy_mode(mode: Mode, mean: u32) -> Candidate {
    let mut required = vec![Property::RateControlMode(mode)];
    if mode == Mode::Quality {
        required.push(Property::Quality(CONSTANT_QUALITY));
    }
    let mut optional = vec![Property::MeanBitRate(mean)];
    if mode == Mode::PeakConstrainedVbr {
        optional.push(Property::MaxBitRate(mean.saturating_mul(2)));
    }
    Candidate { mode, required, optional }
}

/// The modes to try, in order.
pub fn plan(profile: Profile, bitrate_kbps: u32) -> Vec<Candidate> {
    let mean = bitrate_kbps.saturating_mul(1000);
    match profile {
        Profile::Fast => [Mode::PeakConstrainedVbr, Mode::Cbr].map(|m| legacy_mode(m, mean)).to_vec(),
        Profile::ConstantQuality => [Mode::Quality, Mode::PeakConstrainedVbr, Mode::UnconstrainedVbr, Mode::Cbr]
            .map(|m| legacy_mode(m, mean))
            .to_vec(),
        Profile::TargetBitrate => {
            let peak = mean.saturating_add(mean / 2);
            vec![
                target_mode(Mode::Cbr, mean, mean),
                target_mode(Mode::PeakConstrainedVbr, mean, peak),
                target_mode(Mode::UnconstrainedVbr, mean, peak),
                Candidate {
                    mode: Mode::Quality,
                    required: vec![Property::RateControlMode(Mode::Quality), Property::Quality(LAST_RESORT_QUALITY)],
                    optional: vec![Property::MeanBitRate(mean)],
                },
            ]
        }
    }
}

/// Writes the first candidate the driver fully accepts and returns its mode.
/// `set` performs one property write and reports whether the driver accepted it.
pub fn apply(candidates: &[Candidate], mut set: impl FnMut(&Property) -> bool) -> Option<Mode> {
    for candidate in candidates {
        if candidate.required.iter().all(&mut set) {
            for property in &candidate.optional {
                set(property);
            }
            return Some(candidate.mode);
        }
    }
    None
}

/// True when the selected mode actually spends the configured bitrate.
pub fn honors_bitrate(mode: Option<Mode>) -> bool {
    matches!(mode, Some(Mode::Cbr | Mode::PeakConstrainedVbr | Mode::UnconstrainedVbr))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAXIMUM_1080P144: u32 = 143_327;

    fn record(candidates: &[Candidate], reject: impl Fn(&Property) -> bool) -> (Option<Mode>, Vec<Property>) {
        let mut accepted = Vec::new();
        let mode = apply(candidates, |p| {
            if reject(p) {
                false
            } else {
                accepted.push(*p);
                true
            }
        });
        (mode, accepted)
    }

    fn last_mode(writes: &[Property]) -> Option<Mode> {
        writes.iter().rev().find_map(|p| match p {
            Property::RateControlMode(m) => Some(*m),
            _ => None,
        })
    }

    #[test]
    fn maximum_writes_its_bitrate_to_the_driver() {
        let (mode, writes) = record(&plan(Profile::TargetBitrate, MAXIMUM_1080P144), |_| false);
        assert_eq!(mode, Some(Mode::Cbr));
        assert!(honors_bitrate(mode));
        assert_eq!(last_mode(&writes), Some(Mode::Cbr));
        assert!(writes.contains(&Property::MeanBitRate(143_327_000)));
        assert!(writes.contains(&Property::MaxBitRate(143_327_000)));
        assert!(writes.contains(&Property::BufferSize(143_327_000)));
        // The v0.1.7 bug: Quality mode with the 143 Mbps number left unused.
        assert!(!writes.iter().any(|p| matches!(p, Property::Quality(_))));
        assert!(!writes.contains(&Property::RateControlMode(Mode::Quality)));
    }

    #[test]
    fn maximum_never_plans_quality_before_every_bitrate_mode() {
        let candidates = plan(Profile::TargetBitrate, MAXIMUM_1080P144);
        let quality_at = candidates.iter().position(|c| c.mode == Mode::Quality).unwrap();
        assert_eq!(quality_at, candidates.len() - 1);
        for candidate in &candidates[..quality_at] {
            assert!(candidate.required.contains(&Property::MeanBitRate(143_327_000)), "{candidate:?}");
        }
    }

    #[test]
    fn rejected_bitrate_does_not_count_as_a_bitrate_mode() {
        // A driver that takes the mode but not the bitrate must not leave CBR "selected" with
        // the driver's default bitrate.
        let (mode, writes) =
            record(&plan(Profile::TargetBitrate, MAXIMUM_1080P144), |p| matches!(p, Property::MeanBitRate(_)));
        // Every bitrate write rejected: only the logged last resort remains.
        assert_eq!(mode, Some(Mode::Quality));
        assert!(!honors_bitrate(mode));
        assert_eq!(last_mode(&writes), Some(Mode::Quality));
        assert!(writes.contains(&Property::Quality(100)));
    }

    #[test]
    fn maximum_falls_back_to_vbr_with_peak_when_cbr_is_rejected() {
        let (mode, writes) =
            record(&plan(Profile::TargetBitrate, MAXIMUM_1080P144), |p| *p == Property::RateControlMode(Mode::Cbr));
        assert_eq!(mode, Some(Mode::PeakConstrainedVbr));
        assert!(writes.contains(&Property::MeanBitRate(143_327_000)));
        assert!(writes.contains(&Property::MaxBitRate(214_990_500)));
    }

    #[test]
    fn maximum_unconstrained_vbr_still_sets_peak() {
        let (mode, writes) = record(&plan(Profile::TargetBitrate, MAXIMUM_1080P144), |p| {
            matches!(p, Property::RateControlMode(Mode::Cbr | Mode::PeakConstrainedVbr))
        });
        assert_eq!(mode, Some(Mode::UnconstrainedVbr));
        assert!(writes.contains(&Property::MeanBitRate(143_327_000)));
        assert!(writes.contains(&Property::MaxBitRate(214_990_500)));
    }

    #[test]
    fn optional_writes_do_not_reject_a_mode() {
        let (mode, writes) = record(&plan(Profile::TargetBitrate, MAXIMUM_1080P144), |p| {
            matches!(p, Property::BufferSize(_) | Property::MaxBitRate(_))
        });
        assert_eq!(mode, Some(Mode::Cbr));
        assert!(writes.contains(&Property::MeanBitRate(143_327_000)));
    }

    #[test]
    fn other_presets_keep_v017_rate_control() {
        let (mode, writes) = record(&plan(Profile::ConstantQuality, 95_000), |_| false);
        assert_eq!(mode, Some(Mode::Quality));
        assert_eq!(
            writes,
            vec![Property::RateControlMode(Mode::Quality), Property::Quality(93), Property::MeanBitRate(95_000_000)]
        );
        // Quality rejected: budgeted VBR with mean and a 2x peak, as before.
        let (mode, writes) = record(&plan(Profile::ConstantQuality, 95_000), |p| matches!(p, Property::Quality(_)));
        assert_eq!(mode, Some(Mode::PeakConstrainedVbr));
        assert!(writes.contains(&Property::MeanBitRate(95_000_000)));
        assert!(writes.contains(&Property::MaxBitRate(190_000_000)));

        let (mode, writes) = record(&plan(Profile::Fast, 20_000), |_| false);
        assert_eq!(mode, Some(Mode::PeakConstrainedVbr));
        assert!(writes.contains(&Property::MeanBitRate(20_000_000)));
        assert!(writes.contains(&Property::MaxBitRate(40_000_000)));
        assert!(!writes.iter().any(|p| matches!(p, Property::BufferSize(_))));
    }

    #[test]
    fn presets_map_to_profiles() {
        assert_eq!(Profile::for_quality(Quality::Maximum), Profile::TargetBitrate);
        assert_eq!(Profile::for_quality(Quality::High), Profile::ConstantQuality);
        assert_eq!(Profile::for_quality(Quality::Medium), Profile::ConstantQuality);
        assert_eq!(Profile::for_quality(Quality::Custom), Profile::ConstantQuality);
        assert_eq!(Profile::for_quality(Quality::Low), Profile::Fast);
    }

    #[test]
    fn nothing_accepted_reports_none() {
        assert_eq!(apply(&plan(Profile::TargetBitrate, 1_000), |_| false), None);
        assert!(!honors_bitrate(None));
    }
}
