use std::sync::{Arc, Mutex, OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource {
    Memory,
    Disk,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub memory: usize,
    pub disk: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Usage {
    pub memory: usize,
    pub disk: usize,
    pub peak_memory: usize,
    pub peak_disk: usize,
}

struct State {
    limits: Limits,
    usage: Usage,
}

/// All clones share one ledger, including old and new documents and queued results.
#[derive(Clone)]
pub struct BudgetManager(Arc<Mutex<State>>);

impl Default for BudgetManager {
    fn default() -> Self {
        static SHARED: OnceLock<BudgetManager> = OnceLock::new();
        SHARED
            .get_or_init(|| {
                Self::new(Limits {
                    // Transitional legacy-index allowances, not the sparse engine target.
                    memory: 8usize.saturating_mul(1024 * 1024 * 1024),
                    disk: 64usize.saturating_mul(1024 * 1024 * 1024),
                })
            })
            .clone()
    }
}

impl BudgetManager {
    pub fn new(limits: Limits) -> Self {
        Self(Arc::new(Mutex::new(State {
            limits,
            usage: Usage::default(),
        })))
    }

    pub fn usage(&self) -> Usage {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).usage
    }

    pub fn reserve(
        &self,
        resource: Resource,
        bytes: usize,
        label: &'static str,
    ) -> Result<Reservation, String> {
        let mut lease = Reservation {
            manager: self.clone(),
            resource,
            bytes: 0,
            label,
        };
        lease.resize(bytes)?;
        Ok(lease)
    }
}

/// Ownership follows the allocation rather than cache membership or a task's lifetime.
pub struct Reservation {
    manager: BudgetManager,
    resource: Resource,
    bytes: usize,
    label: &'static str,
}

impl Reservation {
    pub fn resize(&mut self, bytes: usize) -> Result<(), String> {
        let mut state = self.manager.0.lock().unwrap_or_else(|e| e.into_inner());
        let (used, limit) = match self.resource {
            Resource::Memory => (state.usage.memory, state.limits.memory),
            Resource::Disk => (state.usage.disk, state.limits.disk),
        };
        let next = used
            .checked_sub(self.bytes)
            .and_then(|n| n.checked_add(bytes))
            .ok_or_else(|| format!("Resource budget overflow: {}", self.label))?;
        if next > limit {
            return Err(format!(
                "{:?} budget exceeded for {}: requested total {next} bytes, limit {limit} bytes",
                self.resource, self.label
            ));
        }
        match self.resource {
            Resource::Memory => {
                state.usage.memory = next;
                state.usage.peak_memory = state.usage.peak_memory.max(next);
            }
            Resource::Disk => {
                state.usage.disk = next;
                state.usage.peak_disk = state.usage.peak_disk.max(next);
            }
        }
        self.bytes = bytes;
        Ok(())
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub(crate) fn label(&self) -> &'static str {
        self.label
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let _ = self.resize(0);
    }
}

/// Pre-reserve a Vec's growth including its old/new allocation overlap.
pub(crate) fn reserve_vec<T>(
    values: &mut Vec<T>,
    additional: usize,
    lease: &mut Reservation,
) -> Result<(), String> {
    let needed = values
        .len()
        .checked_add(additional)
        .ok_or("Vector capacity overflow")?;
    if needed <= values.capacity() {
        return Ok(());
    }
    let old = values
        .capacity()
        .checked_mul(std::mem::size_of::<T>())
        .ok_or("Vector byte overflow")?;
    let new = needed
        .checked_mul(std::mem::size_of::<T>())
        .ok_or("Vector byte overflow")?;
    lease.resize(old.checked_add(new).ok_or("Vector byte overflow")?)?;
    if let Err(error) = values.try_reserve_exact(additional) {
        lease.resize(old)?;
        return Err(format!("Cannot allocate vector: {error}"));
    }
    lease.resize(
        values
            .capacity()
            .checked_mul(std::mem::size_of::<T>())
            .ok_or("Vector byte overflow")?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_limits_reject_overlap_and_return_on_last_owner() {
        let manager = BudgetManager::new(Limits {
            memory: 20,
            disk: 30,
        });
        let lease = Arc::new(
            manager
                .reserve(Resource::Memory, 12, "old document")
                .unwrap(),
        );
        let retained = lease.clone();
        drop(lease);
        assert!(
            manager
                .clone()
                .reserve(Resource::Memory, 9, "new document")
                .is_err()
        );
        assert_eq!(manager.usage().memory, 12);
        drop(retained);
        assert_eq!(manager.usage().memory, 0);
        let mut disk = manager.reserve(Resource::Disk, 30, "snapshot").unwrap();
        assert!(disk.resize(31).is_err());
        assert_eq!(manager.usage().disk, 30);
        drop(disk);
        assert_eq!(manager.usage().disk, 0);
    }

    #[test]
    fn concurrent_reservations_cannot_exceed_the_limit() {
        let manager = BudgetManager::new(Limits {
            memory: 100,
            disk: 0,
        });
        let barrier = Arc::new(std::sync::Barrier::new(8));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let manager = manager.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    let lease = manager.reserve(Resource::Memory, 30, "concurrent");
                    barrier.wait();
                    assert!(manager.usage().memory <= 100);
                    drop(lease);
                });
            }
        });
        assert_eq!(manager.usage().memory, 0);
    }
}
