use anyhow::{Context, Result, ensure};
#[derive(Debug)]
pub struct StaticCapacity;
impl std::fmt::Display for StaticCapacity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("static input capacity exceeded")
    }
}
impl std::error::Error for StaticCapacity {}
/// Static text and latent-frame capacity of one compiled Supertonic graph class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShapeClass {
    pub text: i64,
    pub frames: i64,
}

/// Selects the smallest class covering both original input dimensions.
pub fn class_index(classes: &[ShapeClass], text: i64, frames: i64) -> Result<usize> {
    ensure!(text >= 0 && frames >= 0, "invalid class dimensions");
    classes
        .iter()
        .position(|class| text <= class.text && frames <= class.frames)
        .ok_or_else(|| StaticCapacity.into())
}

pub fn pad<T: Default + Clone>(shape: &[i64], target: &[i64], data: &[T]) -> Result<Vec<T>> {
    ensure!(
        shape.len() == target.len() && shape.iter().chain(target).all(|d| *d >= 0),
        "invalid static shape"
    );
    if shape
        .iter()
        .zip(target)
        .any(|(actual, fixed)| actual > fixed)
    {
        return Err(StaticCapacity.into());
    }
    let product = |s: &[i64]| {
        s.iter()
            .try_fold(1usize, |n, d| n.checked_mul(*d as usize))
            .ok_or_else(|| anyhow::anyhow!("shape overflow"))
    };
    ensure!(
        product(shape)? == data.len(),
        "shape and input size mismatch"
    );
    let mut result = vec![T::default(); product(target)?];
    for (linear, value) in data.iter().enumerate() {
        let mut remainder = linear;
        let mut destination = 0usize;
        let mut stride = 1usize;
        for (actual, fixed) in shape.iter().zip(target).rev() {
            ensure!(*actual > 0, "zero input dimension cannot contain data");
            destination += (remainder % (*actual as usize)) * stride;
            remainder /= *actual as usize;
            stride *= *fixed as usize;
        }
        result[destination] = value.clone();
    }
    Ok(result)
}
pub fn crop_axis<T: Clone>(
    shape: &mut [i64],
    axis: usize,
    length: i64,
    data: &[T],
) -> Result<Vec<T>> {
    ensure!(
        axis < shape.len() && length >= 0 && length <= shape[axis],
        "invalid output crop"
    );
    let product = |dimensions: &[i64]| {
        dimensions
            .iter()
            .try_fold(1usize, |count, &dimension| {
                count.checked_mul(usize::try_from(dimension).ok()?)
            })
            .context("invalid or overflowing output shape")
    };
    ensure!(
        product(shape)? == data.len(),
        "shape and output size mismatch"
    );
    let inner = product(&shape[axis + 1..])?;
    let block = product(&shape[axis..])?;
    let kept = (length as usize)
        .checked_mul(inner)
        .context("output crop overflow")?;
    ensure!(block > 0 || data.is_empty(), "empty shape has output data");
    let result = if block == 0 {
        vec![]
    } else {
        data.chunks(block)
            .flat_map(|row| row[..kept].iter().cloned())
            .collect()
    };
    shape[axis] = length;
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn follow_up_crop_rejects_mismatched_buffers_without_panicking() {
        for data in [&[1, 2, 3, 4, 5][..], &[1, 2, 3, 4][..], &[0; 12][..]] {
            let mut shape = [1, 2, 4];
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crop_axis(&mut shape, 2, 2, data)
            }));
            assert!(
                result.is_ok(),
                "crop must return an error instead of panicking"
            );
            assert!(result.unwrap().is_err());
            assert_eq!(shape, [1, 2, 4]);
        }
        for mut shape in [vec![1, -1, 4], vec![i64::MAX, i64::MAX, 4]] {
            assert!(crop_axis::<u8>(&mut shape, 2, 2, &[]).is_err());
        }
        let mut shape = [1, 0, 4];
        assert!(crop_axis::<u8>(&mut shape, 2, 2, &[]).unwrap().is_empty());
    }

    #[test]
    fn static_shapes_zero_pad_without_truncation_and_crop_owned_output() {
        assert_eq!(
            pad(&[2, 2], &[3, 4], &[1, 2, 3, 4]).unwrap(),
            [1, 2, 0, 0, 3, 4, 0, 0, 0, 0, 0, 0]
        );
        assert!(pad(&[2, 3], &[2, 2], &[0; 6]).is_err());
        assert!(pad(&[2], &[2, 2], &[0; 2]).is_err());
        assert!(
            pad::<f32>(&[1, 0, 32], &[1, 0, 32], &[])
                .unwrap()
                .is_empty()
        );
        let mut shape = vec![1, 2, 4];
        let data = [1, 2, 0, 0, 3, 4, 0, 0];
        assert_eq!(crop_axis(&mut shape, 2, 2, &data).unwrap(), [1, 2, 3, 4]);
        assert_eq!(shape, [1, 2, 2]);
    }
}
