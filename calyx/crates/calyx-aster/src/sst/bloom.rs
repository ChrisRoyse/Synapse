//! Small deterministic Bloom filter for SST point-lookups.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BloomFilter {
    bit_count: u64,
    hash_count: u32,
    bits: Vec<u8>,
}

impl BloomFilter {
    pub fn from_keys<'a>(keys: impl IntoIterator<Item = &'a [u8]>) -> Self {
        let keys: Vec<_> = keys.into_iter().collect();
        let bit_count = ((keys.len().max(1) * 16).next_power_of_two() as u64).max(64);
        let hash_count = 3;
        let mut filter = Self {
            bit_count,
            hash_count,
            bits: vec![0; bit_count.div_ceil(8) as usize],
        };
        for key in keys {
            filter.insert(key);
        }
        filter
    }

    pub fn may_contain(&self, key: &[u8]) -> bool {
        (0..self.hash_count).all(|round| self.bit_is_set(self.bit_index(key, round)))
    }

    pub(super) fn estimated_heap_bytes(&self) -> usize {
        self.bits.capacity()
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.bit_count.to_le_bytes());
        out.extend_from_slice(&self.hash_count.to_le_bytes());
        out.extend_from_slice(&(self.bits.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.bits);
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 16 {
            return None;
        }
        let bit_count = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
        let hash_count = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
        let byte_len = u32::from_le_bytes(bytes[12..16].try_into().ok()?) as usize;
        let bits = bytes.get(16..16 + byte_len)?.to_vec();
        if bit_count == 0 || hash_count == 0 {
            return None;
        }
        if bits.len() != bit_count.div_ceil(8) as usize {
            return None;
        }
        Some(Self {
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
        let mut input = Vec::with_capacity(key.len() + 4);
        input.extend_from_slice(key);
        input.extend_from_slice(&round.to_le_bytes());
        let hash = blake3::hash(&input);
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
