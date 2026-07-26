// Port of upstream internal/helpers/bitset.go.

/// A fixed-size set of bits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BitSet {
    entries: Vec<u8>,
}

impl Default for BitSet {
    fn default() -> Self {
        Self::new(0)
    }
}

impl BitSet {
    #[must_use]
    pub fn new(bit_count: usize) -> Self {
        Self {
            entries: vec![0; bit_count.div_ceil(8)],
        }
    }

    /// # Panics
    ///
    /// Panics when `bit` is outside the size passed to [`Self::new`], matching
    /// the bounds behavior of the upstream Go implementation.
    #[must_use]
    pub fn has_bit(&self, bit: usize) -> bool {
        (self.entries[bit / 8] & (1 << (bit & 7))) != 0
    }

    /// # Panics
    ///
    /// Panics when `bit` is outside the size passed to [`Self::new`], matching
    /// the bounds behavior of the upstream Go implementation.
    pub fn set_bit(&mut self, bit: usize) {
        self.entries[bit / 8] |= 1 << (bit & 7);
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::BitSet;

    #[test]
    fn bit_set_matches_upstream_layout() {
        let mut bits = BitSet::new(17);
        bits.set_bit(0);
        bits.set_bit(7);
        bits.set_bit(8);
        bits.set_bit(16);

        assert!(bits.has_bit(0));
        assert!(bits.has_bit(7));
        assert!(bits.has_bit(8));
        assert!(bits.has_bit(16));
        assert!(!bits.has_bit(1));
        assert_eq!(bits.as_bytes(), &[0x81, 0x01, 0x01]);
        assert_eq!(bits, bits.clone());
    }
}
