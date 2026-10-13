//! Ordered adaptive match blocks: sparse u16 offsets or an 8 KiB bitmap.
use std::collections::BTreeMap;
const BUDGET: usize = 64 * 1024 * 1024;
#[derive(Clone)]
enum Bits {
    Sparse(Vec<u16>),
    Dense(Box<[u64; 1024]>),
}
#[derive(Clone)]
struct Block {
    bits: Bits,
    count: usize,
}
#[derive(Clone, Default)]
pub struct MatchSet {
    blocks: BTreeMap<u16, Block>,
    len: usize,
    bytes: usize,
    last: Option<u32>,
}
impl MatchSet {
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn insert(&mut self, id: u32) -> Result<(), String> {
        if self.last.is_some_and(|last| id <= last) {
            return Err("Search batches are out of order".into());
        }
        let key = (id >> 16) as u16;
        let offset = id as u16;
        let block_count =
            self.blocks.len() + usize::from(self.last.is_none_or(|last| last >> 16 != key as u32));
        let block = self.blocks.entry(key).or_insert_with(|| Block {
            bits: Bits::Sparse(Vec::new()),
            count: 0,
        });
        let before = match &block.bits {
            Bits::Sparse(v) => v.capacity() * 2,
            Bits::Dense(_) => 8192,
        };
        match &mut block.bits {
            Bits::Sparse(v) => {
                if v.len() == v.capacity() {
                    let extra = v.capacity().max(16).min(4096 - v.len());
                    if self.bytes + extra * 2 + block_count * 128 > BUDGET {
                        return Err(
                            "Search matches exceed 64 MiB memory budget; narrow the search".into(),
                        );
                    }
                    v.reserve_exact(extra);
                }
                v.push(offset);
                if v.len() >= 4096 {
                    let mut bitmap = Box::new([0; 1024]);
                    for &offset in v.iter() {
                        bitmap[offset as usize / 64] |= 1 << (offset % 64);
                    }
                    block.bits = Bits::Dense(bitmap);
                }
            }
            Bits::Dense(bits) => bits[offset as usize / 64] |= 1 << (offset % 64),
        }
        let after = match &block.bits {
            Bits::Sparse(v) => v.capacity() * 2,
            Bits::Dense(_) => 8192,
        };
        self.bytes = self.bytes + after - before;
        block.count += 1;
        self.len += 1;
        self.last = Some(id);
        Ok(())
    }
    pub fn contains(&self, id: u32) -> bool {
        self.blocks
            .get(&((id >> 16) as u16))
            .is_some_and(|block| match &block.bits {
                Bits::Sparse(v) => v.binary_search(&(id as u16)).is_ok(),
                Bits::Dense(bits) => bits[id as u16 as usize / 64] & (1 << (id % 64)) != 0,
            })
    }
    pub fn get(&self, mut index: usize) -> Option<u32> {
        if index >= self.len {
            return None;
        }
        for (&key, block) in &self.blocks {
            if index >= block.count {
                index -= block.count;
                continue;
            }
            let offset = match &block.bits {
                Bits::Sparse(v) => v[index] as u32,
                Bits::Dense(bits) => {
                    let mut result = None;
                    for (word, &value) in bits.iter().enumerate() {
                        let count = value.count_ones() as usize;
                        if index >= count {
                            index -= count;
                            continue;
                        }
                        let mut value = value;
                        for _ in 0..index {
                            value &= value - 1;
                        }
                        result = Some(word as u32 * 64 + value.trailing_zeros());
                        break;
                    }
                    result?
                }
            };
            return Some((key as u32) << 16 | offset);
        }
        None
    }
    pub fn before(&self, id: u32) -> usize {
        let key = (id >> 16) as u16;
        let offset = id as u16 as usize;
        let mut count = self.blocks.range(..key).map(|(_, b)| b.count).sum();
        if let Some(block) = self.blocks.get(&key) {
            count += match &block.bits {
                Bits::Sparse(v) => v.partition_point(|n| (*n as usize) < offset),
                Bits::Dense(bits) => {
                    bits[..offset / 64]
                        .iter()
                        .map(|v| v.count_ones() as usize)
                        .sum::<usize>()
                        + (bits[offset / 64] & ((1u64 << (offset % 64)) - 1)).count_ones() as usize
                }
            };
        }
        count
    }
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.blocks.iter().flat_map(|(&key, block)| {
            let mut index = 0;
            let mut word = 0;
            let mut current = 0u64;
            std::iter::from_fn(move || {
                let offset = match &block.bits {
                    Bits::Sparse(v) => {
                        let value = *v.get(index)? as u32;
                        index += 1;
                        value
                    }
                    Bits::Dense(bits) => {
                        while current == 0 {
                            current = *bits.get(word)?;
                            word += 1;
                        }
                        let bit = current.trailing_zeros();
                        current &= current - 1;
                        (word as u32 - 1) * 64 + bit
                    }
                };
                Some((key as u32) << 16 | offset)
            })
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compressed_blocks_keep_rank_order_and_bound_dense_results() {
        let mut set = MatchSet::default();
        let expected: Vec<u32> = (0..200_000)
            .filter(|n| *n < 100_000 || *n % 97 == 0)
            .collect();
        for &id in &expected {
            set.insert(id).unwrap();
        }
        assert_eq!(set.len(), expected.len());
        assert!(set.bytes() < 32 * 1024);
        for index in [0, 4095, 65_535, 99_999, expected.len() - 1] {
            assert_eq!(set.get(index), Some(expected[index]));
        }
        for id in [0, 1, 65_535, 100_001, 199_999] {
            assert_eq!(set.contains(id), expected.binary_search(&id).is_ok());
            assert_eq!(set.before(id), expected.partition_point(|n| *n < id));
        }
        assert!(set.insert(0).is_err());
    }
}
