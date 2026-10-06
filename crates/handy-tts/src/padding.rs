use anyhow::{Result, ensure};
pub fn pad<T: Default + Clone>(shape: &[i64], target: &[i64], data: &[T]) -> Result<Vec<T>> {
    ensure!(
        shape.len() == target.len() && shape.iter().zip(target).all(|(a, b)| *a >= 0 && *a <= *b),
        "static shape would truncate input"
    );
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
    let inner = shape[axis + 1..].iter().product::<i64>() as usize;
    let block = shape[axis] as usize * inner;
    let kept = length as usize * inner;
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
