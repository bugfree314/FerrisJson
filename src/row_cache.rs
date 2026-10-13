use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
};

/// Bound string allocation capacity; metadata is separately bounded by 512 entries.
pub struct RowCache {
    entries: HashMap<usize, (String, String)>,
    order: VecDeque<usize>,
    bytes: usize,
    budget: usize,
}
impl Default for RowCache {
    fn default() -> Self {
        Self::new(1024 * 1024)
    }
}
impl RowCache {
    fn new(budget: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            budget,
        }
    }
    pub fn clear(&mut self) {
        *self = Self::new(self.budget);
    }
    pub fn labels(
        &mut self,
        id: usize,
        build: impl FnOnce() -> (String, String),
    ) -> Cow<'_, (String, String)> {
        if self.entries.contains_key(&id) {
            self.order.retain(|&key| key != id);
            self.order.push_back(id);
        } else {
            let labels = build();
            let cost = labels.0.capacity().saturating_add(labels.1.capacity());
            if cost > self.budget {
                return Cow::Owned(labels);
            }
            while self.bytes + cost > self.budget || self.entries.len() >= 512 {
                let old = self.order.pop_front().unwrap();
                let labels = self.entries.remove(&old).unwrap();
                self.bytes -= labels.0.capacity() + labels.1.capacity();
            }
            self.bytes += cost;
            self.entries.insert(id, labels);
            self.order.push_back(id);
        }
        Cow::Borrowed(self.entries.get(&id).unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn budget_lru_and_oversized_rows() {
        let mut cache = RowCache::new(12);
        cache.labels(1, || ("aaaa".into(), "bb".into()));
        cache.labels(2, || ("cccc".into(), "dd".into()));
        cache.labels(1, || panic!("Cache hit must reuse labels"));
        cache.labels(3, || ("eeee".into(), "ff".into()));
        assert!(cache.entries.contains_key(&1));
        assert!(!cache.entries.contains_key(&2));
        assert!(cache.bytes <= cache.budget);
        assert!(matches!(
            cache.labels(4, || ("x".repeat(100), String::new())),
            Cow::Owned(_)
        ));
        assert!(!cache.entries.contains_key(&4));
        cache.clear();
        assert_eq!(cache.bytes, 0);
        assert_eq!(cache.entries.capacity(), 0);
    }
}
