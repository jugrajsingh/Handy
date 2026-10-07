use serde::Deserialize;
use thiserror::Error;

/// Native synthesis speeds accepted by a model's shipping policy.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct NativeWindow {
    pub min: f32,
    pub max: f32,
}
impl NativeWindow {
    /// Validates ordered finite bounds within the supported native interval.
    pub fn valid(self) -> bool {
        (0.5..=2.0).contains(&self.min) && (self.min..=2.0).contains(&self.max)
    }
}
/// Applies native synthesis and one exact stretch pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpeedPlan {
    pub requested: f32,
    pub native: f32,
    pub stretch: f32,
}
#[derive(Debug, Error, Clone, PartialEq)]
pub enum SpeedError {
    #[error("invalid audio controls")]
    Controls,
}
/// Plans a user speed on the 0.1 grid against validated model bounds.
pub fn plan(requested: f32, window: NativeWindow) -> Result<SpeedPlan, SpeedError> {
    if !(0.5..=2.0).contains(&requested)
        || (requested * 10.0 - (requested * 10.0).round()).abs() > 0.0001
        || !window.valid()
    {
        return Err(SpeedError::Controls);
    }
    let native = requested.clamp(window.min, window.max);
    let stretch = restretch(native, requested).ok_or(SpeedError::Controls)?;
    Ok(SpeedPlan {
        requested,
        native,
        stretch,
    })
}
/// Returns the exact supported ratio for existing native PCM.
pub fn restretch(native: f32, requested: f32) -> Option<f32> {
    if !(0.5..=2.0).contains(&native) || !(0.5..=2.0).contains(&requested) {
        return None;
    }
    let ratio = requested / native;
    (0.5..=2.0).contains(&ratio).then_some(ratio)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plan_multiplies_back_to_requested_for_every_grid_speed() {
        for entry in crate::catalog::entries().unwrap() {
            assert_eq!(entry.native_speed, NativeWindow { min: 1.0, max: 1.0 });
            for step in 5..=20 {
                let requested = step as f32 / 10.0;
                let p = plan(requested, entry.native_speed).unwrap();
                assert!((p.native * p.stretch - requested).abs() < 1e-6);
            }
        }
        for window in [
            NativeWindow { min: 0.8, max: 1.3 },
            NativeWindow { min: 1.0, max: 1.1 },
        ] {
            for step in 5..=20 {
                let requested = step as f32 / 10.0;
                let p = plan(requested, window).unwrap();
                assert_eq!(p.native, requested.clamp(window.min, window.max));
                assert!((p.native * p.stretch - requested).abs() < 1e-6);
            }
        }
    }
    #[test]
    fn unity_window_is_pure_stretch() {
        for step in 5..=20 {
            let requested = step as f32 / 10.0;
            assert_eq!(
                plan(requested, NativeWindow { min: 1.0, max: 1.0 }).unwrap(),
                SpeedPlan {
                    requested,
                    native: 1.0,
                    stretch: requested
                }
            );
        }
    }
    #[test]
    fn restretch_out_of_range_is_none() {
        assert_eq!(restretch(2.0, 0.5), None);
        assert_eq!(restretch(1.1, 2.0), Some(2.0 / 1.1));
        for value in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert_eq!(restretch(value, 1.0), None);
            assert_eq!(restretch(1.0, value), None);
        }
    }
    #[test]
    fn invalid_controls_and_windows_are_rejected() {
        let unity = NativeWindow { min: 1.0, max: 1.0 };
        for speed in [0.4, 2.1, 1.05, f32::NAN, f32::INFINITY] {
            assert!(plan(speed, unity).is_err());
        }
        for window in [
            NativeWindow { min: 1.2, max: 1.0 },
            NativeWindow { min: 0.0, max: 1.0 },
            NativeWindow {
                min: 1.0,
                max: f32::NAN,
            },
            NativeWindow { min: 1.0, max: 2.1 },
        ] {
            assert!(plan(1.0, window).is_err());
        }
    }
}
