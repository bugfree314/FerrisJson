//! Resident records for the progressive index. Source JSON is never stored here.
use super::store::Record;
use crate::engine::budget::{BudgetManager, Reservation, Resource, reserve_vec};

const CHUNK: usize = 4096;

struct Chunk {
    records: Vec<Record>,
    _lease: Reservation,
}

pub(super) struct MemoryRecords {
    buckets: Vec<u64>,
    chunks: Vec<Chunk>,
    count: usize,
    budget: BudgetManager,
    _buckets: Reservation,
    directory: Reservation,
}

impl MemoryRecords {
    pub(super) fn new(budget: &BudgetManager, buckets: usize) -> Result<Self, String> {
        let mut heads = Vec::new();
        let mut lease = budget.reserve(Resource::Memory, 0, "resident index buckets")?;
        reserve_vec(&mut heads, buckets, &mut lease)?;
        heads.resize(buckets, super::NONE);
        Ok(Self {
            buckets: heads,
            chunks: Vec::new(),
            count: 0,
            budget: budget.clone(),
            _buckets: lease,
            directory: budget.reserve(Resource::Memory, 0, "resident chunk directory")?,
        })
    }

    pub(super) fn head(&self, hash: u64) -> u64 {
        self.buckets[(hash % self.buckets.len() as u64) as usize]
    }

    pub(super) fn set_head(&mut self, hash: u64, value: u64) {
        let slot = (hash % self.buckets.len() as u64) as usize;
        self.buckets[slot] = value;
    }

    pub(super) fn append(&mut self, record: Record) -> Result<(), String> {
        let next = self
            .count
            .checked_add(1)
            .ok_or("Resident record count overflow")?;
        if self.count.is_multiple_of(CHUNK) {
            // Only the small chunk directory can move. Existing record blocks stay put.
            reserve_vec(&mut self.chunks, 1, &mut self.directory)?;
            let mut records = Vec::new();
            let mut lease = self
                .budget
                .reserve(Resource::Memory, 0, "resident record block")?;
            reserve_vec(&mut records, CHUNK, &mut lease)?;
            self.chunks.push(Chunk {
                records,
                _lease: lease,
            });
        }
        self.chunks[self.count / CHUNK].records.push(record);
        self.count = next;
        Ok(())
    }

    pub(super) fn get(&self, index: u64) -> Result<Record, String> {
        let index = usize::try_from(index).map_err(|_| "Resident record ID overflow")?;
        self.chunks
            .get(index / CHUNK)
            .and_then(|chunk| chunk.records.get(index % CHUNK))
            .copied()
            .ok_or_else(|| "Invalid resident record ID".into())
    }

    pub(super) fn put(&mut self, index: u64, record: Record) -> Result<(), String> {
        let index = usize::try_from(index).map_err(|_| "Resident record ID overflow")?;
        let slot = self
            .chunks
            .get_mut(index / CHUNK)
            .and_then(|chunk| chunk.records.get_mut(index % CHUNK))
            .ok_or("Invalid resident record ID")?;
        *slot = record;
        Ok(())
    }
}
