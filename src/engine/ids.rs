use super::budget::{BudgetManager, Reservation, Resource, reserve_vec};
use std::ops::Deref;

/// Complete-index adapter IDs, with capacity charged until the last Arc owner drops.
pub struct NodeIds {
    values: Vec<u32>,
    reservation: Reservation,
}

impl NodeIds {
    pub fn new(budget: &BudgetManager) -> Self {
        Self {
            values: Vec::new(),
            reservation: budget.reserve(Resource::Memory, 0, "node IDs").unwrap(),
        }
    }
    pub fn from_slice(budget: &BudgetManager, values: &[u32]) -> Result<Self, String> {
        let mut ids = Self::new(budget);
        ids.reserve_exact(values.len())?;
        for &id in values {
            ids.push(id)?;
        }
        Ok(ids)
    }
    pub fn reserve_exact(&mut self, additional: usize) -> Result<(), String> {
        reserve_vec(&mut self.values, additional, &mut self.reservation)
    }
    pub fn push(&mut self, id: u32) -> Result<(), String> {
        if self.values.len() == self.values.capacity() {
            self.reserve_exact(1)?;
        }
        self.values.push(id);
        Ok(())
    }
    pub fn clear(&mut self) {
        self.values.clear();
    }
    pub fn release(&mut self) {
        self.values = Vec::new();
        self.reservation.resize(0).unwrap();
    }
}
impl Default for NodeIds {
    fn default() -> Self {
        Self::new(&BudgetManager::default())
    }
}
impl Deref for NodeIds {
    type Target = Vec<u32>;
    fn deref(&self) -> &Self::Target {
        &self.values
    }
}
impl std::fmt::Debug for NodeIds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.values.fmt(f)
    }
}
impl PartialEq<Vec<u32>> for NodeIds {
    fn eq(&self, other: &Vec<u32>) -> bool {
        self.values == *other
    }
}
impl PartialEq for NodeIds {
    fn eq(&self, other: &Self) -> bool {
        self.values == other.values
    }
}
impl IntoIterator for NodeIds {
    type Item = u32;
    type IntoIter = OwnedIds;
    fn into_iter(self) -> Self::IntoIter {
        OwnedIds {
            values: self.values.into_iter(),
            _reservation: self.reservation,
        }
    }
}
impl<'a> IntoIterator for &'a NodeIds {
    type Item = &'a u32;
    type IntoIter = std::slice::Iter<'a, u32>;
    fn into_iter(self) -> Self::IntoIter {
        self.values.iter()
    }
}

pub struct OwnedIds {
    values: std::vec::IntoIter<u32>,
    _reservation: Reservation,
}
impl Iterator for OwnedIds {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        self.values.next()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.values.size_hint()
    }
}
