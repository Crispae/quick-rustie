//! Fixed-length bitset over the tokens of one sentence.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenSet {
    bits: Vec<u64>,
    n: usize,
}

impl TokenSet {
    pub fn new(n: usize) -> Self {
        Self {
            bits: vec![0; n.div_ceil(64)],
            n,
        }
    }

    pub fn full(n: usize) -> Self {
        let mut s = Self::new(n);
        for w in &mut s.bits {
            *w = u64::MAX;
        }
        s.mask_tail();
        s
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    #[inline]
    pub fn set(&mut self, i: usize) {
        if i < self.n {
            self.bits[i / 64] |= 1u64 << (i % 64);
        }
    }

    #[inline]
    pub fn get(&self, i: usize) -> bool {
        i < self.n && self.bits[i / 64] & (1u64 << (i % 64)) != 0
    }

    pub fn clear(&mut self) {
        self.bits.iter_mut().for_each(|w| *w = 0);
    }

    pub fn any(&self) -> bool {
        self.bits.iter().any(|w| *w != 0)
    }

    /// True when any token in `[start, end)` is set.
    pub fn any_in(&self, start: usize, end: usize) -> bool {
        (start..end.min(self.n)).any(|i| self.get(i))
    }

    pub fn or_assign(&mut self, other: &Self) {
        for (a, b) in self.bits.iter_mut().zip(&other.bits) {
            *a |= *b;
        }
    }

    pub fn and_assign(&mut self, other: &Self) {
        for (a, b) in self.bits.iter_mut().zip(&other.bits) {
            *a &= *b;
        }
    }

    pub fn negate(&mut self) {
        for w in &mut self.bits {
            *w = !*w;
        }
        self.mask_tail();
    }

    pub fn iter_ones(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.n).filter(move |&i| self.get(i))
    }

    fn mask_tail(&mut self) {
        let rem = self.n % 64;
        if rem != 0 {
            if let Some(last) = self.bits.last_mut() {
                *last &= (1u64 << rem) - 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negate_masks_tail() {
        let mut s = TokenSet::new(70);
        s.set(3);
        s.negate();
        assert!(!s.get(3));
        assert!(s.get(69));
        assert!(!s.get(70));
        assert_eq!(s.iter_ones().count(), 69);
    }

    #[test]
    fn any_in_range() {
        let mut s = TokenSet::new(10);
        s.set(4);
        assert!(s.any_in(3, 5));
        assert!(!s.any_in(5, 10));
        assert!(!s.any_in(0, 4));
    }
}
