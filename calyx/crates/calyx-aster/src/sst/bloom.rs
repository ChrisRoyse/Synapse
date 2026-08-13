//! Small deterministic Bloom filter for SST point-lookups.

use calyx_core::{CalyxError, Result};

const BITS_PER_KEY: usize = 16;
const MIN_BIT_COUNT: usize = 64;
const HASH_COUNT: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BloomFilter {
    bit_count: u64,
    hash_count: u32,
    bits: Vec<u8>,
}

impl BloomFilter {
    pub fn from_keys<'a, I>(keys: I) -> Result<Self>
    where
        I: IntoIterator<Item = &'a [u8]>,
        I::IntoIter: ExactSizeIterator,
    {
        let keys = keys.into_iter();
        let requested_bits = keys
            .len()
            .max(1)
            .checked_mul(BITS_PER_KEY)
            .ok_or_else(|| CalyxError::disk_pressure("SST bloom filter bit count overflow"))?;
        let bit_count = requested_bits
            .checked_next_power_of_two()
            .ok_or_else(|| CalyxError::disk_pressure("SST bloom filter power-of-two overflow"))?
            .max(MIN_BIT_COUNT);
        let byte_count = bit_count.div_ceil(8);
        u32::try_from(byte_count).map_err(|_| {
            CalyxError::disk_pressure("SST bloom filter byte length exceeds its u32 encoding")
        })?;
        let bit_count = bit_count as u64;
        let hash_count = HASH_COUNT;
        let mut bits = Vec::new();
        bits.try_reserve_exact(byte_count)
            .map_err(|error| CalyxError {
                code: "CALYX_ASTER_SST_BLOOM_ALLOC",
                message: format!(
                    "could not reserve {byte_count} bytes while constructing an SST bloom filter: {error}"
                ),
                remediation: "free host memory or reduce the immutable flush batch; SST creation fails closed instead of aborting the daemon",
            })?;
        bits.resize(byte_count, 0);
        let mut filter = Self {
            bit_count,
            hash_count,
            bits,
        };
        for key in keys {
            filter.insert(key);
        }
        Ok(filter)
    }

    pub fn may_contain(&self, key: &[u8]) -> bool {
        (0..self.hash_count).all(|round| self.bit_is_set(self.bit_index(key, round)))
    }

    pub(super) fn estimated_heap_bytes(&self) -> usize {
        self.bits.capacity()
    }

    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let byte_len = u32::try_from(self.bits.len()).map_err(|_| {
            CalyxError::disk_pressure("SST bloom filter byte length exceeds its u32 encoding")
        })?;
        out.extend_from_slice(&self.bit_count.to_le_bytes());
        out.extend_from_slice(&self.hash_count.to_le_bytes());
        out.extend_from_slice(&byte_len.to_le_bytes());
        out.extend_from_slice(&self.bits);
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 16 {
            return Err(CalyxError::aster_corrupt_shard(
                "SST bloom filter header is truncated",
            ));
        }
        let bit_count = u64::from_le_bytes(bytes[0..8].try_into().expect("bloom bit count"));
        let hash_count = u32::from_le_bytes(bytes[8..12].try_into().expect("bloom hash count"));
        let byte_len =
            u32::from_le_bytes(bytes[12..16].try_into().expect("bloom byte length")) as usize;
        if bit_count < MIN_BIT_COUNT as u64 || !bit_count.is_power_of_two() {
            return Err(CalyxError::aster_corrupt_shard(
                "SST bloom filter bit count is below the format minimum or not a power of two",
            ));
        }
        if hash_count != HASH_COUNT {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST bloom filter hash-round count is {hash_count}, expected {HASH_COUNT}"
            )));
        }
        let expected_byte_len = usize::try_from(bit_count.div_ceil(8)).map_err(|_| {
            CalyxError::aster_corrupt_shard("SST bloom filter bit count exceeds usize")
        })?;
        if byte_len != expected_byte_len {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST bloom filter length disagrees with its bit count: declared={byte_len} expected={expected_byte_len}"
            )));
        }
        let encoded_len = 16_usize.checked_add(byte_len).ok_or_else(|| {
            CalyxError::aster_corrupt_shard("SST bloom filter encoded length overflow")
        })?;
        if bytes.len() != encoded_len {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST bloom filter section length mismatch: actual={} expected={encoded_len}",
                bytes.len()
            )));
        }
        let mut bits = Vec::new();
        bits.try_reserve_exact(byte_len).map_err(|error| CalyxError {
            code: "CALYX_ASTER_SST_BLOOM_ALLOC",
            message: format!(
                "could not reserve {byte_len} bytes for a validated SST bloom filter: {error}"
            ),
            remediation: "free host memory or repair the oversized/corrupt SST; the vault refuses to open instead of aborting the daemon",
        })?;
        bits.extend_from_slice(&bytes[16..]);
        Ok(Self {
            bit_count,
            hash_count,
            bits,
        })
    }

    fn insert(&mut self, key: &[u8]) {
        for round in 0..self.hash_count {
            let index = self.bit_index(key, round);
            self.set_bit(index);
        }
    }

    fn bit_index(&self, key: &[u8], round: u32) -> u64 {
        let mut hasher = blake3::Hasher::new();
        hasher.update(key);
        hasher.update(&round.to_le_bytes());
        let hash = hasher.finalize();
        u64::from_le_bytes(hash.as_bytes()[0..8].try_into().expect("hash width")) % self.bit_count
    }

    fn set_bit(&mut self, index: u64) {
        let byte = (index / 8) as usize;
        let bit = (index % 8) as u8;
        self.bits[byte] |= 1 << bit;
    }

    fn bit_is_set(&self, index: u64) -> bool {
        let byte = (index / 8) as usize;
        let bit = (index % 8) as u8;
        self.bits[byte] & (1 << bit) != 0
    }
}
