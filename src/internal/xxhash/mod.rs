// Port of upstream internal/xxhash/xxhash.go and xxhash_other.go.
//
// This package originated at github.com/cespare/xxhash and retains the
// copyright notice in licenses/xxhash.txt.

use std::error::Error;
use std::fmt;

const PRIME1: u64 = 11_400_714_785_074_694_791;
const PRIME2: u64 = 14_029_467_366_897_019_727;
const PRIME3: u64 = 1_609_587_929_392_839_161;
const PRIME4: u64 = 9_650_029_242_287_828_579;
const PRIME5: u64 = 2_870_177_450_012_600_261;
const MAGIC: &[u8; 4] = b"xxh\x06";
const MARSHALLED_SIZE: usize = MAGIC.len() + 8 * 5 + 32;

/// Streaming state for the 64-bit variant of xxHash (XXH64).
#[derive(Clone, Debug)]
pub struct Digest {
    v1: u64,
    v2: u64,
    v3: u64,
    v4: u64,
    total: u64,
    memory: [u8; 32],
    used: usize,
}

impl Digest {
    #[must_use]
    pub fn new() -> Self {
        let mut digest = Self {
            v1: 0,
            v2: 0,
            v3: 0,
            v4: 0,
            total: 0,
            memory: [0; 32],
            used: 0,
        };
        digest.reset();
        digest
    }

    pub fn reset(&mut self) {
        self.v1 = PRIME1.wrapping_add(PRIME2);
        self.v2 = PRIME2;
        self.v3 = 0;
        self.v4 = 0_u64.wrapping_sub(PRIME1);
        self.total = 0;
        self.used = 0;
    }

    #[must_use]
    pub const fn size(&self) -> usize {
        8
    }

    #[must_use]
    pub const fn block_size(&self) -> usize {
        32
    }

    pub fn write(&mut self, mut bytes: &[u8]) -> usize {
        let written = bytes.len();
        self.total = self.total.wrapping_add(written as u64);

        if self.used + written < 32 {
            self.memory[self.used..self.used + written].copy_from_slice(bytes);
            self.used += written;
            return written;
        }

        if self.used > 0 {
            let needed = 32 - self.used;
            self.memory[self.used..].copy_from_slice(&bytes[..needed]);
            self.v1 = round(self.v1, read_u64(&self.memory[0..8]));
            self.v2 = round(self.v2, read_u64(&self.memory[8..16]));
            self.v3 = round(self.v3, read_u64(&self.memory[16..24]));
            self.v4 = round(self.v4, read_u64(&self.memory[24..32]));
            bytes = &bytes[needed..];
            self.used = 0;
        }

        if bytes.len() >= 32 {
            let consumed = write_blocks(self, bytes);
            bytes = &bytes[consumed..];
        }

        self.memory[..bytes.len()].copy_from_slice(bytes);
        self.used = bytes.len();
        written
    }

    #[must_use]
    pub fn sum(&self, prefix: &[u8]) -> Vec<u8> {
        let hash = self.sum64();
        let mut result = Vec::with_capacity(prefix.len() + 8);
        result.extend_from_slice(prefix);
        result.extend_from_slice(&hash.to_be_bytes());
        result
    }

    #[must_use]
    pub fn sum64(&self) -> u64 {
        let mut hash = if self.total >= 32 {
            let mut hash = self
                .v1
                .rotate_left(1)
                .wrapping_add(self.v2.rotate_left(7))
                .wrapping_add(self.v3.rotate_left(12))
                .wrapping_add(self.v4.rotate_left(18));
            hash = merge_round(hash, self.v1);
            hash = merge_round(hash, self.v2);
            hash = merge_round(hash, self.v3);
            merge_round(hash, self.v4)
        } else {
            self.v3.wrapping_add(PRIME5)
        };

        hash = hash.wrapping_add(self.total);
        finalize(hash, &self.memory[..self.used])
    }

    #[must_use]
    pub fn marshal_binary(&self) -> [u8; MARSHALLED_SIZE] {
        let mut result = [0; MARSHALLED_SIZE];
        result[..MAGIC.len()].copy_from_slice(MAGIC);
        let mut offset = MAGIC.len();
        for value in [self.v1, self.v2, self.v3, self.v4, self.total] {
            result[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
            offset += 8;
        }
        result[offset..offset + self.used].copy_from_slice(&self.memory[..self.used]);
        result
    }

    /// # Errors
    ///
    /// Returns an error if the identifier or serialized state size is invalid.
    pub fn unmarshal_binary(&mut self, bytes: &[u8]) -> Result<(), StateError> {
        if !bytes.starts_with(MAGIC) {
            return Err(StateError::InvalidIdentifier);
        }
        if bytes.len() != MARSHALLED_SIZE {
            return Err(StateError::InvalidSize);
        }

        let mut offset = MAGIC.len();
        self.v1 = consume_u64(bytes, &mut offset);
        self.v2 = consume_u64(bytes, &mut offset);
        self.v3 = consume_u64(bytes, &mut offset);
        self.v4 = consume_u64(bytes, &mut offset);
        self.total = consume_u64(bytes, &mut offset);
        self.memory.copy_from_slice(&bytes[offset..offset + 32]);
        self.used = usize::try_from(self.total % self.memory.len() as u64).unwrap_or_default();
        Ok(())
    }
}

impl Default for Digest {
    fn default() -> Self {
        Self::new()
    }
}

impl std::hash::Hasher for Digest {
    fn finish(&self) -> u64 {
        self.sum64()
    }

    fn write(&mut self, bytes: &[u8]) {
        Digest::write(self, bytes);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateError {
    InvalidIdentifier,
    InvalidSize,
}

impl fmt::Display for StateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentifier => formatter.write_str("xxhash: invalid hash state identifier"),
            Self::InvalidSize => formatter.write_str("xxhash: invalid hash state size"),
        }
    }
}

impl Error for StateError {}

/// Computes the 64-bit xxHash digest of `bytes`.
#[must_use]
pub fn sum64(mut bytes: &[u8]) -> u64 {
    let length = bytes.len();
    let mut hash;

    if length >= 32 {
        let mut v1 = PRIME1.wrapping_add(PRIME2);
        let mut v2 = PRIME2;
        let mut v3 = 0;
        let mut v4 = 0_u64.wrapping_sub(PRIME1);
        while bytes.len() >= 32 {
            v1 = round(v1, read_u64(&bytes[0..8]));
            v2 = round(v2, read_u64(&bytes[8..16]));
            v3 = round(v3, read_u64(&bytes[16..24]));
            v4 = round(v4, read_u64(&bytes[24..32]));
            bytes = &bytes[32..];
        }
        hash = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        hash = merge_round(hash, v1);
        hash = merge_round(hash, v2);
        hash = merge_round(hash, v3);
        hash = merge_round(hash, v4);
    } else {
        hash = PRIME5;
    }

    hash = hash.wrapping_add(length as u64);
    finalize(hash, bytes)
}

fn write_blocks(digest: &mut Digest, mut bytes: &[u8]) -> usize {
    let original_length = bytes.len();
    let (mut v1, mut v2, mut v3, mut v4) = (digest.v1, digest.v2, digest.v3, digest.v4);
    while bytes.len() >= 32 {
        v1 = round(v1, read_u64(&bytes[0..8]));
        v2 = round(v2, read_u64(&bytes[8..16]));
        v3 = round(v3, read_u64(&bytes[16..24]));
        v4 = round(v4, read_u64(&bytes[24..32]));
        bytes = &bytes[32..];
    }
    (digest.v1, digest.v2, digest.v3, digest.v4) = (v1, v2, v3, v4);
    original_length - bytes.len()
}

fn finalize(mut hash: u64, bytes: &[u8]) -> u64 {
    let mut index = 0;
    while index + 8 <= bytes.len() {
        let k1 = round(0, read_u64(&bytes[index..index + 8]));
        hash ^= k1;
        hash = hash
            .rotate_left(27)
            .wrapping_mul(PRIME1)
            .wrapping_add(PRIME4);
        index += 8;
    }
    if index + 4 <= bytes.len() {
        hash ^= u64::from(read_u32(&bytes[index..index + 4])).wrapping_mul(PRIME1);
        hash = hash
            .rotate_left(23)
            .wrapping_mul(PRIME2)
            .wrapping_add(PRIME3);
        index += 4;
    }
    while index < bytes.len() {
        hash ^= u64::from(bytes[index]).wrapping_mul(PRIME5);
        hash = hash.rotate_left(11).wrapping_mul(PRIME1);
        index += 1;
    }

    hash ^= hash >> 33;
    hash = hash.wrapping_mul(PRIME2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(PRIME3);
    hash ^ (hash >> 32)
}

fn round(accumulator: u64, input: u64) -> u64 {
    accumulator
        .wrapping_add(input.wrapping_mul(PRIME2))
        .rotate_left(31)
        .wrapping_mul(PRIME1)
}

fn merge_round(mut accumulator: u64, value: u64) -> u64 {
    accumulator ^= round(0, value);
    accumulator.wrapping_mul(PRIME1).wrapping_add(PRIME4)
}

fn read_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().expect("xxhash reads eight-byte chunks"))
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("xxhash reads four-byte chunks"))
}

fn consume_u64(bytes: &[u8], offset: &mut usize) -> u64 {
    let value = read_u64(&bytes[*offset..*offset + 8]);
    *offset += 8;
    value
}

#[cfg(test)]
mod tests {
    use super::{Digest, StateError, sum64};

    #[test]
    fn matches_canonical_xxh64_vectors() {
        assert_eq!(sum64(b""), 0xef46_db37_51d8_e999);
        assert_eq!(sum64(b"a"), 0xd24e_c4f1_a98c_6e5b);
        assert_eq!(sum64(b"abc"), 0x44bc_2cf5_ad77_0999);
    }

    #[test]
    fn streaming_matches_one_shot_at_all_block_boundaries() {
        let input: Vec<u8> = (0..=255).collect();
        for length in 0..=input.len() {
            let expected = sum64(&input[..length]);
            for chunk_size in 1..=40 {
                let mut digest = Digest::new();
                for chunk in input[..length].chunks(chunk_size) {
                    assert_eq!(digest.write(chunk), chunk.len());
                }
                assert_eq!(
                    digest.sum64(),
                    expected,
                    "length={length}, chunk={chunk_size}"
                );
            }
        }
    }

    #[test]
    fn marshaled_state_resumes_streaming() {
        let mut first = Digest::new();
        first.write(b"the first part ");
        let state = first.marshal_binary();

        let mut second = Digest::new();
        second.unmarshal_binary(&state).unwrap();
        first.write(b"and the second part");
        second.write(b"and the second part");
        assert_eq!(first.sum64(), second.sum64());
        assert_eq!(first.sum(b"prefix"), second.sum(b"prefix"));
    }

    #[test]
    fn rejects_invalid_marshaled_state() {
        let mut digest = Digest::new();
        assert_eq!(
            digest.unmarshal_binary(b"bad"),
            Err(StateError::InvalidIdentifier)
        );
        assert_eq!(
            digest.unmarshal_binary(b"xxh\x06"),
            Err(StateError::InvalidSize)
        );
    }
}
