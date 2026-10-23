//! FxHash (the rustc/Firefox hasher): multiply-xor over 8-byte chunks. Internal
//! maps only — no iteration order reaches output (design-core §7 invariant).

use std::hash::{BuildHasherDefault, Hasher};

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

pub type FxBuildHasher = BuildHasherDefault<FxHasher>;
pub type FxHashMap<K, V> = std::collections::HashMap<K, V, FxBuildHasher>;
pub type FxHashSet<K> = std::collections::HashSet<K, FxBuildHasher>;

#[derive(Default)]
pub struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (chunks, tail) = bytes.as_chunks::<8>();
        for chunk in chunks {
            self.add(u64::from_le_bytes(*chunk));
        }
        if !tail.is_empty() {
            let mut word = [0u8; 8];
            word[..tail.len()].copy_from_slice(tail);
            self.add(u64::from_le_bytes(word));
        }
    }

    #[inline]
    fn write_u8(&mut self, n: u8) {
        self.add(u64::from(n));
    }

    #[inline]
    fn write_u32(&mut self, n: u32) {
        self.add(u64::from(n));
    }

    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.add(n);
    }

    #[inline]
    fn write_usize(&mut self, n: usize) {
        self.add(n as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_stable_and_distinguish() {
        let mut a = FxHasher::default();
        std::hash::Hash::hash("color", &mut a);
        let mut b = FxHasher::default();
        std::hash::Hash::hash("color", &mut b);
        let mut c = FxHasher::default();
        std::hash::Hash::hash("colour", &mut c);
        assert_eq!(a.finish(), b.finish());
        assert_ne!(a.finish(), c.finish());
        // str Hash writes the bytes plus a 0xff terminator: no prefix collisions.
        let mut d = FxHasher::default();
        std::hash::Hash::hash("col", &mut d);
        let mut e = FxHasher::default();
        std::hash::Hash::hash("col\0or", &mut e);
        assert_ne!(d.finish(), e.finish());
    }
}
