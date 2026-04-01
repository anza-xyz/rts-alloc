pub const NUM_SIZE_CLASSES: usize = 5;
/// Size classes must be sub powers of two
pub const SIZE_CLASSES: [u32; NUM_SIZE_CLASSES] = [256, 512, 1024, 2048, 4096];
pub const MIN_SIZE: u32 = SIZE_CLASSES[0];
pub const MAX_SIZE: u32 = SIZE_CLASSES[NUM_SIZE_CLASSES - 1];
const BASE_SHIFT: u32 = SIZE_CLASSES[0].trailing_zeros();

const _: () = assert!(
    NUM_SIZE_CLASSES < 256,
    "NUM_SIZE_CLASSES must be less than 256"
);

pub fn size_class_index(size: u32) -> Option<usize> {
    if size > MAX_SIZE {
        return None;
    }
    // SAFETY: size <= MAX_SIZE.
    Some(unsafe { size_class_index_unchecked(size) })
}

/// Returns the size class index for a given `size`.
///
/// # Safety
/// - The `size` must be less than or equal to `MAX_SIZE`.
#[inline]
pub unsafe fn size_class_index_unchecked(size: u32) -> usize {
    size.next_power_of_two()
        .trailing_zeros()
        .saturating_sub(BASE_SHIFT) as usize
}

/// Returns the size class for a given `index`.
///
/// # Panics
/// - If `index` is out of bounds.
#[inline]
pub fn size_class(index: usize) -> u32 {
    SIZE_CLASSES[index]
}

/// Returns the size class for a given `index`.
///
/// # Safety
/// - `index` must be less than `NUM_SIZE_CLASSES`.
#[inline]
pub unsafe fn size_class_unchecked(index: usize) -> u32 {
    *SIZE_CLASSES.get_unchecked(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_size_class_index() {
        for (i, &size) in SIZE_CLASSES.iter().enumerate() {
            assert_eq!(size_class_index(size - 1), Some(i));
            assert_eq!(size_class_index(size), Some(i));
        }
        assert!(size_class_index(MAX_SIZE + 1).is_none());
    }

    #[test]
    fn test_size_class() {
        for (i, &size) in SIZE_CLASSES.iter().enumerate() {
            assert_eq!(size_class(i), size);
        }
    }
}
