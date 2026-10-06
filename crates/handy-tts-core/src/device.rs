use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default, specta::Type)]
pub enum Device {
    #[default]
    Cpu,
    Gpu,
    Npu,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
#[derive(specta::Type)]
pub enum RunOn {
    #[default]
    Auto,
    Cpu,
    Gpu,
    Npu,
}
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct DeviceRating {
    pub supported: bool,
    pub placement: BTreeMap<String, Device>,
    pub first_audio_p95_s: Option<f64>,
    pub rtf_1x: Option<f64>,
    pub rtf_2x: Option<f64>,
    pub load_s_cold: Option<f64>,
    pub load_s_cached: Option<f64>,
    pub speed_score: Option<f64>,
    pub recommended: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct ModelRating {
    pub quality: u8,
    pub devices: BTreeMap<Device, DeviceRating>,
}
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct Ratings {
    pub schema_version: u32,
    pub metadata: BTreeMap<String, String>,
    pub models: BTreeMap<String, ModelRating>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DeviceError {
    #[error("tts.errors.deviceUnavailable")]
    Unavailable,
    #[error("tts.errors.deviceNotSupported")]
    Unsupported,
    #[error("tts.errors.npuDriverTooOld")]
    NpuDriverTooOld,
    #[error("tts.errors.prepareFailed")]
    CompileFailed,
    #[error("tts.errors.invalidRatings")]
    InvalidRatings,
}
impl Ratings {
    pub fn validate(&self, model_ids: &[&str]) -> Result<(), DeviceError> {
        if self.schema_version != 1 || self.models.len() != model_ids.len() {
            return Err(DeviceError::InvalidRatings);
        }
        for id in model_ids {
            let m = self.models.get(*id).ok_or(DeviceError::InvalidRatings)?;
            if m.quality > 10 || m.devices.len() != 3 {
                return Err(DeviceError::InvalidRatings);
            }
            let mut recommended = 0;
            for device in [Device::Cpu, Device::Gpu, Device::Npu] {
                let d = m.devices.get(&device).ok_or(DeviceError::InvalidRatings)?;
                if d.supported {
                    for value in [
                        d.first_audio_p95_s,
                        d.rtf_1x,
                        d.rtf_2x,
                        d.load_s_cold,
                        d.load_s_cached,
                        d.speed_score,
                    ] {
                        if !value.is_some_and(|v| v.is_finite() && v >= 0.0) {
                            return Err(DeviceError::InvalidRatings);
                        }
                    }
                    if d.speed_score.is_some_and(|v| v > 10.0) || d.placement.is_empty() {
                        return Err(DeviceError::InvalidRatings);
                    }
                } else if d.recommended {
                    return Err(DeviceError::InvalidRatings);
                }
                recommended += usize::from(d.recommended);
            }
            if recommended != 1 {
                return Err(DeviceError::InvalidRatings);
            }
            let ordered = candidates(
                RunOn::Auto,
                &[Device::Cpu, Device::Gpu, Device::Npu],
                m,
                &[],
            )?;
            let winner = ordered.first().ok_or(DeviceError::InvalidRatings)?;
            if !m.devices.get(winner).is_some_and(|d| d.recommended) {
                return Err(DeviceError::InvalidRatings);
            }
        }
        Ok(())
    }
}
fn tie(device: Device) -> u8 {
    match device {
        Device::Cpu => 0,
        Device::Npu => 1,
        Device::Gpu => 2,
    }
}
pub fn candidates(
    choice: RunOn,
    available: &[Device],
    model: &ModelRating,
    invalid: &[Device],
) -> Result<Vec<Device>, DeviceError> {
    let explicit = match choice {
        RunOn::Auto => None,
        RunOn::Cpu => Some(Device::Cpu),
        RunOn::Gpu => Some(Device::Gpu),
        RunOn::Npu => Some(Device::Npu),
    };
    if let Some(device) = explicit {
        if !available.contains(&device) {
            return Err(DeviceError::Unavailable);
        }
        if invalid.contains(&device) {
            return Err(if device == Device::Npu {
                DeviceError::NpuDriverTooOld
            } else {
                DeviceError::CompileFailed
            });
        }
        if !model.devices.get(&device).is_some_and(|r| r.supported) {
            return Err(DeviceError::Unsupported);
        }
        return Ok(vec![device]);
    }
    let mut devices: Vec<_> = available
        .iter()
        .copied()
        .filter(|d| !invalid.contains(d) && model.devices.get(d).is_some_and(|r| r.supported))
        .collect();
    devices.sort_by(|a, b| {
        let score = |d: &Device| {
            model
                .devices
                .get(d)
                .and_then(|r| r.speed_score)
                .unwrap_or(0.0)
        };
        score(b)
            .total_cmp(&score(a))
            .then_with(|| tie(*a).cmp(&tie(*b)))
    });
    devices.dedup();
    if devices.is_empty() {
        Err(DeviceError::Unavailable)
    } else {
        Ok(devices)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
#[derive(specta::Type)]
pub enum Preparation {
    Idle,
    Preparing {
        device: Device,
        completed: u32,
        total: u32,
        graph: String,
    },
    Ready {
        device: Device,
    },
    Failed {
        device: Device,
        error: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadResult {
    pub device: Device,
    pub fallback_from: Option<Device>,
}
pub fn load_with(
    choice: RunOn,
    available: &[Device],
    rating: &ModelRating,
    invalid: &[Device],
    mut compile: impl FnMut(Device) -> Result<(), DeviceError>,
) -> Result<LoadResult, DeviceError> {
    let ordered = candidates(choice, available, rating, invalid)?;
    let first = ordered[0];
    let mut last = DeviceError::CompileFailed;
    for device in ordered {
        match compile(device) {
            Ok(()) => {
                return Ok(LoadResult {
                    device,
                    fallback_from: if device == first { None } else { Some(first) },
                });
            }
            Err(error) => {
                last = error;
                if choice != RunOn::Auto {
                    return Err(error);
                }
            }
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rating() -> ModelRating {
        ModelRating {
            quality: 9,
            devices: [(Device::Cpu, 6.0), (Device::Gpu, 8.0), (Device::Npu, 9.0)]
                .into_iter()
                .map(|(d, s)| {
                    (
                        d,
                        DeviceRating {
                            supported: true,
                            placement: BTreeMap::from([("main".into(), d)]),
                            first_audio_p95_s: Some(1.0),
                            rtf_1x: Some(0.2),
                            rtf_2x: Some(0.3),
                            load_s_cold: Some(20.0),
                            load_s_cached: Some(2.0),
                            speed_score: Some(s),
                            recommended: d == Device::Npu,
                        },
                    )
                })
                .collect(),
        }
    }
    #[test]
    fn auto_uses_highest_supported_visible_device_and_falls_back_once() {
        let r = rating();
        let all = [Device::Cpu, Device::Gpu, Device::Npu];
        let mut attempted = vec![];
        let result = load_with(RunOn::Auto, &all, &r, &[], |d| {
            attempted.push(d);
            if d == Device::Npu {
                Err(DeviceError::NpuDriverTooOld)
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(attempted, [Device::Npu, Device::Gpu]);
        assert_eq!(
            result,
            LoadResult {
                device: Device::Gpu,
                fallback_from: Some(Device::Npu)
            }
        );
        assert_eq!(
            candidates(RunOn::Auto, &all, &r, &[Device::Npu]).unwrap(),
            [Device::Gpu, Device::Cpu]
        );
        assert_eq!(
            candidates(RunOn::Auto, &[Device::Cpu], &r, &[]).unwrap(),
            [Device::Cpu]
        );
    }
    #[test]
    fn explicit_failure_preserves_choice_and_never_falls_back() {
        let r = rating();
        let mut attempted = vec![];
        assert_eq!(
            load_with(RunOn::Npu, &[Device::Cpu, Device::Npu], &r, &[], |d| {
                attempted.push(d);
                Err(DeviceError::NpuDriverTooOld)
            }),
            Err(DeviceError::NpuDriverTooOld)
        );
        assert_eq!(attempted, [Device::Npu]);
        assert_eq!(
            candidates(RunOn::Gpu, &[Device::Cpu], &r, &[]),
            Err(DeviceError::Unavailable)
        );
        let mut unsupported = r;
        unsupported.devices.get_mut(&Device::Npu).unwrap().supported = false;
        assert_eq!(
            candidates(RunOn::Npu, &[Device::Npu], &unsupported, &[]),
            Err(DeviceError::Unsupported)
        );
    }
    #[test]
    fn ratings_recommend_highest_supported_score_with_power_tie_order() {
        for (scores, supported, expected) in [
            ([2.0, 9.0, 5.0], [true, true, true], Device::Gpu),
            ([8.0, 8.0, 8.0], [true, true, true], Device::Cpu),
            ([2.0, 8.0, 8.0], [true, true, true], Device::Npu),
            ([2.0, 9.0, 5.0], [true, false, true], Device::Npu),
        ] {
            for recommended in [Device::Cpu, Device::Gpu, Device::Npu] {
                let mut model = rating();
                for (index, device) in [Device::Cpu, Device::Gpu, Device::Npu]
                    .into_iter()
                    .enumerate()
                {
                    let row = model.devices.get_mut(&device).unwrap();
                    row.speed_score = Some(scores[index]);
                    row.supported = supported[index];
                    row.recommended = device == recommended;
                }
                let ratings = Ratings {
                    schema_version: 1,
                    metadata: BTreeMap::new(),
                    models: BTreeMap::from([("fixture".into(), model)]),
                };
                assert_eq!(
                    ratings.validate(&["fixture"]),
                    if recommended == expected {
                        Ok(())
                    } else {
                        Err(DeviceError::InvalidRatings)
                    },
                    "scores={scores:?}, supported={supported:?}, recommended={recommended:?}"
                );
            }
        }
    }
    #[test]
    fn tie_order_and_ratings_schema_are_deterministic() {
        let mut r = rating();
        for (device, d) in &mut r.devices {
            d.speed_score = Some(8.0);
            d.recommended = *device == Device::Cpu;
        }
        assert_eq!(
            candidates(
                RunOn::Auto,
                &[Device::Gpu, Device::Npu, Device::Cpu],
                &r,
                &[]
            )
            .unwrap(),
            [Device::Cpu, Device::Npu, Device::Gpu]
        );
        let mut ratings = Ratings {
            schema_version: 1,
            metadata: BTreeMap::new(),
            models: BTreeMap::from([("fixture".into(), r)]),
        };
        assert_eq!(ratings.validate(&["fixture"]), Ok(()));
        ratings.schema_version = 2;
        assert_eq!(
            ratings.validate(&["fixture"]),
            Err(DeviceError::InvalidRatings)
        );
        ratings.schema_version = 1;
        assert_eq!(
            ratings.validate(&["missing"]),
            Err(DeviceError::InvalidRatings)
        );
        ratings
            .models
            .get_mut("fixture")
            .unwrap()
            .devices
            .get_mut(&Device::Cpu)
            .unwrap()
            .rtf_2x = Some(f64::NAN);
        assert_eq!(
            ratings.validate(&["fixture"]),
            Err(DeviceError::InvalidRatings)
        );
    }
}
