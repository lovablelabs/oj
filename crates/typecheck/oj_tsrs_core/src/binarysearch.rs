// BinarySearchUniqueFunc works like [slices.BinarySearchFunc], but avoids extra
// invocations of the comparison function by assuming that only one element
// in the slice could match the target. Also, unlike [slices.BinarySearchFunc],
// the comparison function is passed the current index of the element being
// compared, instead of the target element.
pub fn binary_search_unique_func<E>(
    x: &[E],
    mut cmp: impl FnMut(usize, &E) -> Ordering,
) -> (usize, bool) {
    let n = x.len();
    if n == 0 {
        return (0, false);
    }
    let (mut low, mut high) = (0i64, n as i64 - 1);
    while low <= high {
        let middle = low + ((high - low) >> 1);
        let value = cmp(middle as usize, &x[middle as usize]);
        if value == Ordering::Less {
            low = middle + 1;
        } else if value == Ordering::Greater {
            high = middle - 1;
        } else {
            return (middle as usize, true);
        }
    }
    (low as usize, false)
}
use std::cmp::Ordering;
