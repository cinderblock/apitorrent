//! A fixed-length bitfield in the wire format: piece 0 is the high bit of byte 0.

use std::fmt;

#[derive(Clone, PartialEq, Eq)]
pub struct Bitfield {
    bytes: Vec<u8>,
    len: u32,
    ones: u32,
}

impl Bitfield {
    pub fn new(len: u32) -> Self {
        Self {
            bytes: vec![0; len.div_ceil(8) as usize],
            len,
            ones: 0,
        }
    }

    pub fn full(len: u32) -> Self {
        let mut b = Self {
            bytes: vec![0xff; len.div_ceil(8) as usize],
            len,
            ones: len,
        };
        b.clear_spare_bits();
        b
    }

    /// Parse a wire bitfield for `len` pieces. Returns `None` if the length is wrong or a
    /// spare bit past the end is set, both of which BEP 3 says to treat as a protocol error.
    pub fn from_bytes(bytes: &[u8], len: u32) -> Option<Self> {
        if bytes.len() != len.div_ceil(8) as usize {
            return None;
        }
        let mut b = Self {
            bytes: bytes.to_vec(),
            len,
            ones: 0,
        };
        let before: Vec<u8> = b.bytes.clone();
        b.clear_spare_bits();
        if b.bytes != before {
            return None;
        }
        b.ones = b.bytes.iter().map(|x| x.count_ones()).sum();
        Some(b)
    }

    fn clear_spare_bits(&mut self) {
        let spare = self.len.div_ceil(8) * 8 - self.len;
        if spare > 0
            && let Some(last) = self.bytes.last_mut()
        {
            *last &= 0xffu8 << spare;
        }
    }

    pub fn len(&self) -> u32 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, i: u32) -> bool {
        i < self.len && self.bytes[(i / 8) as usize] & (0x80 >> (i % 8)) != 0
    }

    /// Set bit `i`. Returns whether it changed.
    ///
    /// # Panics
    ///
    /// Panics if `i` is out of range: callers validate piece indices from the network first.
    pub fn set(&mut self, i: u32) -> bool {
        assert!(i < self.len, "bit {i} out of range {}", self.len);
        let byte = &mut self.bytes[(i / 8) as usize];
        let mask = 0x80 >> (i % 8);
        let changed = *byte & mask == 0;
        *byte |= mask;
        if changed {
            self.ones += 1;
        }
        changed
    }

    /// Clear bit `i`. Returns whether it changed.
    pub fn clear(&mut self, i: u32) -> bool {
        assert!(i < self.len, "bit {i} out of range {}", self.len);
        let byte = &mut self.bytes[(i / 8) as usize];
        let mask = 0x80 >> (i % 8);
        let changed = *byte & mask != 0;
        *byte &= !mask;
        if changed {
            self.ones -= 1;
        }
        changed
    }

    pub fn count_ones(&self) -> u32 {
        self.ones
    }

    pub fn is_full(&self) -> bool {
        self.ones == self.len
    }

    pub fn none_set(&self) -> bool {
        self.ones == 0
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn iter_ones(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.len).filter(|&i| self.get(i))
    }
}

impl fmt::Debug for Bitfield {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Bitfield({}/{})", self.ones, self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        let mut b = Bitfield::new(10);
        assert!(b.set(0));
        assert!(!b.set(0));
        assert!(b.set(9));
        assert_eq!(b.as_bytes(), [0x80, 0x40]);
        assert_eq!(b.count_ones(), 2);
        assert!(b.clear(0));
        assert_eq!(b.iter_ones().collect::<Vec<_>>(), [9]);
        assert!(!b.get(100));
    }

    #[test]
    fn full_clears_spare_bits() {
        let b = Bitfield::full(10);
        assert_eq!(b.as_bytes(), [0xff, 0xc0]);
        assert!(b.is_full());
    }

    #[test]
    fn wire_validation() {
        assert!(Bitfield::from_bytes(&[0xff, 0xc0], 10).is_some());
        // A spare bit set past the end.
        assert!(Bitfield::from_bytes(&[0xff, 0xe0], 10).is_none());
        // Wrong length.
        assert!(Bitfield::from_bytes(&[0xff], 10).is_none());
        assert_eq!(
            Bitfield::from_bytes(&[0xff, 0xc0], 10)
                .unwrap()
                .count_ones(),
            10
        );
    }
}
