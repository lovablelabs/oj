//! Preserves Go's comparison-call order when it affects checker history.
//!
//! Go standard library `slices` functions ported with Go's exact algorithm (go1.27 `slices/sort.go`, `slices/zsortanyfunc.go`), for comparators whose sequence of calls is observable: the checker's `CompareTypes` / `compareSymbols` assign symbol ids lazily as a last resort, so which pairs a sort or search compares decides which symbols get ids first.
//!
//!
//! Rust's `sort_by` / `binary_search_by` return the same results for a total order but compare different
//! pairs.

use std::cmp::Ordering;

/// Go `slices.BinarySearchFunc`.
#[inline]
pub fn binary_search_func<E, T: ?Sized>(
    x: &[E],
    target: &T,
    mut cmp: impl FnMut(&E, &T) -> Ordering,
) -> (usize, bool) {
    let n = x.len();
    let (mut i, mut j) = (0usize, n);
    while i < j {
        let h = usize::midpoint(i, j);
        if cmp(&x[h], target) == Ordering::Less {
            i = h + 1;
        } else {
            j = h;
        }
    }
    (i, i < n && cmp(&x[i], target) == Ordering::Equal)
}

/// Go `slices.SortStableFunc`.
pub fn sort_stable_func<E>(data: &mut [E], mut cmp: impl FnMut(&E, &E) -> Ordering) {
    let n = data.len();
    stable_cmp_func(data, n, &mut cmp);
}

fn insertion_sort_cmp_func<E>(
    data: &mut [E],
    a: usize,
    b: usize,
    cmp: &mut impl FnMut(&E, &E) -> Ordering,
) {
    for i in a + 1..b {
        let mut j = i;
        while j > a && cmp(&data[j], &data[j - 1]) == Ordering::Less {
            data.swap(j, j - 1);
            j -= 1;
        }
    }
}

fn swap_range_cmp_func<E>(data: &mut [E], a: usize, b: usize, n: usize) {
    for i in 0..n {
        data.swap(a + i, b + i);
    }
}

fn stable_cmp_func<E>(data: &mut [E], n: usize, cmp: &mut impl FnMut(&E, &E) -> Ordering) {
    let mut block_size = 20;
    let (mut a, mut b) = (0, block_size);
    while b <= n {
        insertion_sort_cmp_func(data, a, b, cmp);
        a = b;
        b += block_size;
    }
    insertion_sort_cmp_func(data, a, n, cmp);
    while block_size < n {
        a = 0;
        b = 2 * block_size;
        while b <= n {
            sym_merge_cmp_func(data, a, a + block_size, b, cmp);
            a = b;
            b += 2 * block_size;
        }
        let m = a + block_size;
        if m < n {
            sym_merge_cmp_func(data, a, m, n, cmp);
        }
        block_size *= 2;
    }
}

fn sym_merge_cmp_func<E>(
    data: &mut [E],
    a: usize,
    m: usize,
    b: usize,
    cmp: &mut impl FnMut(&E, &E) -> Ordering,
) {
    if m - a == 1 {
        let (mut i, mut j) = (m, b);
        while i < j {
            let h = usize::midpoint(i, j);
            if cmp(&data[h], &data[a]) == Ordering::Less {
                i = h + 1;
            } else {
                j = h;
            }
        }
        let mut k = a;
        while k + 1 < i {
            data.swap(k, k + 1);
            k += 1;
        }
        return;
    }
    if b - m == 1 {
        let (mut i, mut j) = (a, m);
        while i < j {
            let h = usize::midpoint(i, j);
            if cmp(&data[m], &data[h]) == Ordering::Less {
                j = h;
            } else {
                i = h + 1;
            }
        }
        let mut k = m;
        while k > i {
            data.swap(k, k - 1);
            k -= 1;
        }
        return;
    }
    let mid = usize::midpoint(a, b);
    let n = mid + m;
    let (mut start, mut r) = if m > mid { (n - b, mid) } else { (a, m) };
    let p = n - 1;
    while start < r {
        let c = usize::midpoint(start, r);
        if cmp(&data[p - c], &data[c]) == Ordering::Less {
            r = c;
        } else {
            start = c + 1;
        }
    }
    let end = n - start;
    if start < m && m < end {
        rotate_cmp_func(data, start, m, end);
    }
    if a < start && start < mid {
        sym_merge_cmp_func(data, a, start, mid, cmp);
    }
    if mid < end && end < b {
        sym_merge_cmp_func(data, mid, end, b, cmp);
    }
}

fn rotate_cmp_func<E>(data: &mut [E], a: usize, m: usize, b: usize) {
    let mut i = m - a;
    let mut j = b - m;
    while i != j {
        if i > j {
            swap_range_cmp_func(data, m - i, m, j);
            i -= j;
        } else {
            swap_range_cmp_func(data, m - i, m + j - i, i);
            j -= i;
        }
    }
    swap_range_cmp_func(data, m - i, m, i);
}

/// Go `slices.SortFunc` (pdqsort; not stable, and the order of equal elements is Go's).
pub fn sort_func<E>(x: &mut [E], mut cmp: impl FnMut(&E, &E) -> Ordering) {
    let n = x.len();
    pdqsort_cmp_func(x, 0, n, bits_len(n), &mut cmp);
}

fn bits_len(n: usize) -> usize {
    n.bit_width() as usize
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortedHint {
    Unknown,
    Increasing,
    Decreasing,
}

struct Xorshift(u64);

impl Xorshift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn next_power_of_two(length: usize) -> usize {
    1 << bits_len(length)
}

fn sift_down_cmp_func<E>(
    data: &mut [E],
    lo: usize,
    hi: usize,
    first: usize,
    cmp: &mut impl FnMut(&E, &E) -> Ordering,
) {
    let mut root = lo;
    loop {
        let mut child = 2 * root + 1;
        if child >= hi {
            break;
        }
        if child + 1 < hi && cmp(&data[first + child], &data[first + child + 1]) == Ordering::Less {
            child += 1;
        }
        if cmp(&data[first + root], &data[first + child]) != Ordering::Less {
            return;
        }
        data.swap(first + root, first + child);
        root = child;
    }
}

fn heap_sort_cmp_func<E>(
    data: &mut [E],
    a: usize,
    b: usize,
    cmp: &mut impl FnMut(&E, &E) -> Ordering,
) {
    let first = a;
    let lo = 0;
    let hi = b - a;

    for i in (0..=(hi - 1) / 2).rev() {
        sift_down_cmp_func(data, i, hi, first, cmp);
    }

    for i in (0..hi).rev() {
        data.swap(first, first + i);
        sift_down_cmp_func(data, lo, i, first, cmp);
    }
}

fn pdqsort_cmp_func<E>(
    data: &mut [E],
    mut a: usize,
    mut b: usize,
    mut limit: usize,
    cmp: &mut impl FnMut(&E, &E) -> Ordering,
) {
    const MAX_INSERTION: usize = 12;

    let mut was_balanced = true;
    let mut was_partitioned = true;

    loop {
        let length = b - a;

        if length <= MAX_INSERTION {
            insertion_sort_cmp_func(data, a, b, cmp);
            return;
        }

        if limit == 0 {
            heap_sort_cmp_func(data, a, b, cmp);
            return;
        }

        if !was_balanced {
            break_patterns_cmp_func(data, a, b);
            limit -= 1;
        }

        let (mut pivot, mut hint) = choose_pivot_cmp_func(data, a, b, cmp);
        if hint == SortedHint::Decreasing {
            reverse_range_cmp_func(data, a, b);
            pivot = (b - 1) - (pivot - a);
            hint = SortedHint::Increasing;
        }

        if was_balanced
            && was_partitioned
            && hint == SortedHint::Increasing
            && partial_insertion_sort_cmp_func(data, a, b, cmp)
        {
            return;
        }

        if a > 0 && (cmp(&data[a - 1], &data[pivot]) != Ordering::Less) {
            let mid = partition_equal_cmp_func(data, a, b, pivot, cmp);
            a = mid;
            continue;
        }

        let (mid, already_partitioned) = partition_cmp_func(data, a, b, pivot, cmp);
        was_partitioned = already_partitioned;

        let (left_len, right_len) = (mid - a, b - mid);
        let balance_threshold = length / 8;
        if left_len < right_len {
            was_balanced = left_len >= balance_threshold;
            pdqsort_cmp_func(data, a, mid, limit, cmp);
            a = mid + 1;
        } else {
            was_balanced = right_len >= balance_threshold;
            pdqsort_cmp_func(data, mid + 1, b, limit, cmp);
            b = mid;
        }
    }
}

fn partition_cmp_func<E>(
    data: &mut [E],
    a: usize,
    b: usize,
    pivot: usize,
    cmp: &mut impl FnMut(&E, &E) -> Ordering,
) -> (usize, bool) {
    data.swap(a, pivot);
    let (mut i, mut j) = (a as isize + 1, b as isize - 1);

    while i <= j && cmp(&data[i as usize], &data[a]) == Ordering::Less {
        i += 1;
    }
    while i <= j && (cmp(&data[j as usize], &data[a]) != Ordering::Less) {
        j -= 1;
    }
    if i > j {
        data.swap(j as usize, a);
        return (j as usize, true);
    }
    data.swap(i as usize, j as usize);
    i += 1;
    j -= 1;

    loop {
        while i <= j && cmp(&data[i as usize], &data[a]) == Ordering::Less {
            i += 1;
        }
        while i <= j && (cmp(&data[j as usize], &data[a]) != Ordering::Less) {
            j -= 1;
        }
        if i > j {
            break;
        }
        data.swap(i as usize, j as usize);
        i += 1;
        j -= 1;
    }
    data.swap(j as usize, a);
    (j as usize, false)
}

fn partition_equal_cmp_func<E>(
    data: &mut [E],
    a: usize,
    b: usize,
    pivot: usize,
    cmp: &mut impl FnMut(&E, &E) -> Ordering,
) -> usize {
    data.swap(a, pivot);
    let (mut i, mut j) = (a as isize + 1, b as isize - 1);

    loop {
        while i <= j && (cmp(&data[a], &data[i as usize]) != Ordering::Less) {
            i += 1;
        }
        while i <= j && cmp(&data[a], &data[j as usize]) == Ordering::Less {
            j -= 1;
        }
        if i > j {
            break;
        }
        data.swap(i as usize, j as usize);
        i += 1;
        j -= 1;
    }
    i as usize
}

fn partial_insertion_sort_cmp_func<E>(
    data: &mut [E],
    a: usize,
    b: usize,
    cmp: &mut impl FnMut(&E, &E) -> Ordering,
) -> bool {
    const MAX_STEPS: usize = 5;
    const SHORTEST_SHIFTING: usize = 50;
    let mut i = a + 1;
    for _ in 0..MAX_STEPS {
        i += (i..b)
            .take_while(|&index| cmp(&data[index], &data[index - 1]) != Ordering::Less)
            .count();

        if i == b {
            return true;
        }

        if b - a < SHORTEST_SHIFTING {
            return false;
        }

        data.swap(i, i - 1);

        // Go shifts down to index 1, not a.
        if i - a >= 2 {
            let mut j = i - 1;
            while j >= 1 {
                if cmp(&data[j], &data[j - 1]) != Ordering::Less {
                    break;
                }
                data.swap(j, j - 1);
                j -= 1;
            }
        }
        if b - i >= 2 {
            let mut j = i + 1;
            while j < b {
                if cmp(&data[j], &data[j - 1]) != Ordering::Less {
                    break;
                }
                data.swap(j, j - 1);
                j += 1;
            }
        }
    }
    false
}

fn break_patterns_cmp_func<E>(data: &mut [E], a: usize, b: usize) {
    let length = b - a;
    if length >= 8 {
        let mut random = Xorshift(length as u64);
        let modulus = next_power_of_two(length);

        let mut idx = a + (length / 4) * 2 - 1;
        while idx <= a + (length / 4) * 2 + 1 {
            let mut other = (random.next() as usize) & (modulus - 1);
            if other >= length {
                other -= length;
            }
            data.swap(idx, a + other);
            idx += 1;
        }
    }
}

fn choose_pivot_cmp_func<E>(
    data: &[E],
    a: usize,
    b: usize,
    cmp: &mut impl FnMut(&E, &E) -> Ordering,
) -> (usize, SortedHint) {
    const SHORTEST_NINTHER: usize = 50;
    const MAX_SWAPS: usize = 4 * 3;

    let l = b - a;

    let mut swaps = 0;
    let mut i = a + l / 4;
    let mut j = a + l / 4 * 2;
    let mut k = a + l / 4 * 3;

    if l >= 8 {
        if l >= SHORTEST_NINTHER {
            i = median_adjacent_cmp_func(data, i, &mut swaps, cmp);
            j = median_adjacent_cmp_func(data, j, &mut swaps, cmp);
            k = median_adjacent_cmp_func(data, k, &mut swaps, cmp);
        }
        j = median_cmp_func(data, i, j, k, &mut swaps, cmp);
    }

    match swaps {
        0 => (j, SortedHint::Increasing),
        MAX_SWAPS => (j, SortedHint::Decreasing),
        _ => (j, SortedHint::Unknown),
    }
}

fn order2_cmp_func<E>(
    data: &[E],
    a: usize,
    b: usize,
    swaps: &mut usize,
    cmp: &mut impl FnMut(&E, &E) -> Ordering,
) -> (usize, usize) {
    if cmp(&data[b], &data[a]) == Ordering::Less {
        *swaps += 1;
        return (b, a);
    }
    (a, b)
}

fn median_cmp_func<E>(
    data: &[E],
    a: usize,
    b: usize,
    c: usize,
    swaps: &mut usize,
    cmp: &mut impl FnMut(&E, &E) -> Ordering,
) -> usize {
    let (a, b) = order2_cmp_func(data, a, b, swaps, cmp);
    let (b, c) = order2_cmp_func(data, b, c, swaps, cmp);
    let (_a, b) = order2_cmp_func(data, a, b, swaps, cmp);
    let _ = c;
    b
}

fn median_adjacent_cmp_func<E>(
    data: &[E],
    a: usize,
    swaps: &mut usize,
    cmp: &mut impl FnMut(&E, &E) -> Ordering,
) -> usize {
    median_cmp_func(data, a - 1, a, a + 1, swaps, cmp)
}

fn reverse_range_cmp_func<E>(data: &mut [E], a: usize, b: usize) {
    let mut i = a;
    let mut j = b - 1;
    while i < j {
        data.swap(i, j);
        i += 1;
        j -= 1;
    }
}
