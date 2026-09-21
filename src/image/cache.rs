use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Shared accounting for JPEG/RGBA buffers, GPU textures, and staging uploads.
/// Native decoder bookkeeping and small application objects are not included.
#[derive(Debug)]
struct BudgetState {
    used: usize,
    epoch: u64,
}

#[derive(Debug)]
pub struct MemoryBudget {
    limit: usize,
    used: Mutex<BudgetState>,
    changed: Condvar,
}

impl MemoryBudget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: Mutex::new(BudgetState { used: 0, epoch: 0 }),
            changed: Condvar::new(),
        })
    }

    pub fn limit(&self) -> usize {
        self.limit
    }
    pub fn used(&self) -> usize {
        lock(&self.used).used
    }
    pub fn available(&self) -> usize {
        self.limit.saturating_sub(self.used())
    }

    pub fn try_reserve(self: &Arc<Self>, bytes: usize) -> Option<MemoryLease> {
        let mut used = lock(&self.used);
        let next = used
            .used
            .checked_add(bytes)
            .filter(|next| *next <= self.limit)?;
        used.used = next;
        Some(MemoryLease {
            budget: Arc::clone(self),
            bytes,
        })
    }

    pub fn epoch(&self) -> u64 {
        lock(&self.used).epoch
    }

    /// Budget releases and loader navigation/cancellation advance the epoch.
    /// Wait for one of those events instead of waking a full cache every second.
    /// Checking under the same mutex as release also covers a notification that
    /// arrives between the caller's failed reservation and this wait.
    pub fn wait_for_change(&self, previous_epoch: u64) {
        let mut used = lock(&self.used);
        while used.epoch == previous_epoch {
            used = self
                .changed
                .wait(used)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    pub fn wake_waiters(&self) {
        let mut used = lock(&self.used);
        used.epoch = used.epoch.wrapping_add(1);
        drop(used);
        self.changed.notify_all();
    }

    fn release(&self, bytes: usize) {
        let mut used = lock(&self.used);
        used.used = used.used.saturating_sub(bytes);
        used.epoch = used.epoch.wrapping_add(1);
        drop(used);
        self.changed.notify_all();
    }
}

/// Move, never clone, a reservation along with the allocation it covers.
#[derive(Debug)]
pub struct MemoryLease {
    budget: Arc<MemoryBudget>,
    bytes: usize,
}

impl MemoryLease {
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn split_off(&mut self, bytes: usize) -> Option<Self> {
        if bytes > self.bytes {
            return None;
        }
        self.bytes -= bytes;
        Some(Self {
            budget: Arc::clone(&self.budget),
            bytes,
        })
    }

    pub fn resize(&mut self, bytes: usize) -> bool {
        if bytes <= self.bytes {
            self.budget.release(self.bytes - bytes);
            self.bytes = bytes;
            return true;
        }
        let extra = bytes - self.bytes;
        let mut used = lock(&self.budget.used);
        let Some(next) = used
            .used
            .checked_add(extra)
            .filter(|next| *next <= self.budget.limit)
        else {
            return false;
        };
        used.used = next;
        self.bytes = bytes;
        true
    }
}

impl Drop for MemoryLease {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

pub struct DecodedImage {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub original_width: u32,
    pub original_height: u32,
    pub scale: u32,
    // Field order matters: the pixels are freed before their reservation.
    _lease: MemoryLease,
}

impl std::fmt::Debug for DecodedImage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DecodedImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("original_width", &self.original_width)
            .field("original_height", &self.original_height)
            .field("scale", &self.scale)
            .field("bytes", &self.bytes())
            .finish()
    }
}

impl DecodedImage {
    pub(crate) fn new(
        pixels: Vec<u8>,
        width: u32,
        height: u32,
        original_width: u32,
        original_height: u32,
        scale: u32,
        mut lease: MemoryLease,
    ) -> Result<Self, String> {
        if !lease.resize(pixels.capacity()) {
            drop(pixels);
            return Err("decoded allocation exceeded reserved memory".into());
        }
        Ok(Self {
            pixels,
            width,
            height,
            original_width,
            original_height,
            scale,
            _lease: lease,
        })
    }

    pub fn bytes(&self) -> usize {
        self.pixels.capacity()
    }
}

struct Entry {
    image: Arc<DecodedImage>,
    touched: u64,
    prefetched: bool,
}

/// No hidden ownership: eviction frees memory only if the cache owns the last
/// Arc. An upload in progress keeps its memory lease until it finishes.
pub(crate) struct DecodedCache {
    entries: HashMap<usize, Entry>,
    clock: u64,
    budget: Arc<MemoryBudget>,
}

impl DecodedCache {
    pub fn new(budget: Arc<MemoryBudget>) -> Self {
        Self {
            entries: HashMap::new(),
            clock: 0,
            budget,
        }
    }

    pub fn contains(&self, index: usize) -> bool {
        self.entries.contains_key(&index)
    }
    pub fn bytes(&self) -> usize {
        self.entries.values().map(|entry| entry.image.bytes()).sum()
    }
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Preserve decoded allocations across list removals/reordering. Only
    /// cached paths enter the temporary map; a 10k-photo list is scanned once
    /// without creating a second map containing every filename.
    pub fn rekey(&mut self, old_files: &[PathBuf], new_files: &[PathBuf]) {
        if new_files.is_empty() {
            self.clear();
            return;
        }
        if self.entries.is_empty() {
            return;
        }
        let mut by_path: HashMap<&Path, Entry> = HashMap::with_capacity(self.entries.len());
        for (index, entry) in self.entries.drain() {
            let Some(path) = old_files.get(index) else {
                continue;
            };
            match by_path.get_mut(path.as_path()) {
                Some(previous) if previous.touched < entry.touched => *previous = entry,
                Some(_) => {}
                None => {
                    by_path.insert(path.as_path(), entry);
                }
            }
        }
        for (index, path) in new_files.iter().enumerate() {
            if let Some(entry) = by_path.remove(path.as_path()) {
                // Move the complete entry: no pixel copy, new reservation,
                // changed prefetch flag, or artificial LRU touch.
                self.entries.insert(index, entry);
            }
        }
    }

    pub fn remove(&mut self, index: usize) {
        self.entries.remove(&index);
    }

    pub fn get(&mut self, index: usize) -> Option<Arc<DecodedImage>> {
        self.clock = self.clock.saturating_add(1);
        let entry = self.entries.get_mut(&index)?;
        entry.touched = self.clock;
        Some(Arc::clone(&entry.image))
    }

    pub fn take_prefetched(&mut self, index: usize) -> bool {
        self.entries
            .get_mut(&index)
            .is_some_and(|entry| std::mem::take(&mut entry.prefetched))
    }

    pub fn insert(&mut self, index: usize, image: Arc<DecodedImage>, prefetched: bool) {
        self.clock = self.clock.saturating_add(1);
        self.entries.insert(
            index,
            Entry {
                image,
                touched: self.clock,
                prefetched,
            },
        );
    }

    fn evict_one(&mut self, keep: Option<usize>) -> usize {
        let victim = self
            .entries
            .iter()
            .filter(|(index, entry)| Some(**index) != keep && Arc::strong_count(&entry.image) == 1)
            .min_by_key(|(_, entry)| entry.touched)
            .map(|(&index, _)| index);
        if let Some(victim) = victim {
            let bytes = self.entries[&victim].image.bytes();
            self.entries.remove(&victim);
            bytes
        } else {
            0
        }
    }

    pub fn evict_bytes(&mut self, bytes: usize, keep: Option<usize>) -> usize {
        let mut freed: usize = 0;
        while freed < bytes {
            let released = self.evict_one(keep);
            if released == 0 {
                break;
            }
            freed = freed.saturating_add(released);
        }
        freed
    }

    pub fn evict_for(&mut self, bytes: usize, keep: Option<usize>) -> bool {
        while self.budget.available() < bytes {
            if self.evict_one(keep) == 0 {
                return false;
            }
        }
        true
    }

    pub fn evict_all_except(&mut self, keep: Option<usize>) -> usize {
        self.evict_bytes(usize::MAX, keep)
    }

    pub fn trim_to(&mut self, bytes: usize, keep: Option<usize>) {
        while self.bytes() > bytes {
            if self.evict_one(keep) == 0 {
                break;
            }
        }
    }

    pub fn retain_indices(&mut self, indices: &[usize]) {
        self.entries.retain(|index, _| indices.contains(index));
    }

    /// A nearer neighbor may replace a less useful cached neighbor. A distant
    /// speculative request can never evict a higher-priority image.
    pub fn evict_lower_priority(&mut self, priorities: &[usize], candidate: usize) -> usize {
        let Some(candidate_rank) = priorities.iter().position(|index| *index == candidate) else {
            return 0;
        };
        let victim = self
            .entries
            .iter()
            .filter(|(_, entry)| Arc::strong_count(&entry.image) == 1)
            .filter_map(|(&index, _)| {
                let rank = priorities
                    .iter()
                    .position(|wanted| *wanted == index)
                    .unwrap_or(usize::MAX);
                (rank > candidate_rank).then_some((index, rank))
            })
            .max_by_key(|(_, rank)| *rank)
            .map(|(index, _)| index);
        if let Some(victim) = victim {
            let bytes = self.entries[&victim].image.bytes();
            self.entries.remove(&victim);
            bytes
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, thread, time::Duration};

    #[test]
    fn budget_wait_observes_a_release_before_waiting_without_a_lost_wakeup() {
        let budget = MemoryBudget::new(16);
        let lease = budget.try_reserve(16).unwrap();
        let epoch = budget.epoch();
        drop(lease);
        let worker_budget = Arc::clone(&budget);
        let (send, receive) = mpsc::channel();
        let worker = thread::spawn(move || {
            worker_budget.wait_for_change(epoch);
            send.send(()).unwrap();
        });
        receive.recv_timeout(Duration::from_secs(1)).unwrap();
        worker.join().unwrap();
        assert_eq!(budget.available(), 16);
    }

    #[test]
    fn budget_wait_ignores_notifications_until_the_epoch_changes() {
        let budget = MemoryBudget::new(16);
        let epoch = budget.epoch();
        let worker_budget = Arc::clone(&budget);
        let (started_send, started_receive) = mpsc::channel();
        let (send, receive) = mpsc::channel();
        let worker = thread::spawn(move || {
            started_send.send(()).unwrap();
            worker_budget.wait_for_change(epoch);
            send.send(()).unwrap();
        });
        started_receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        for _ in 0..3 {
            // A bare notification models a spurious condition-variable wake.
            budget.changed.notify_all();
            assert!(matches!(
                receive.recv_timeout(Duration::from_millis(15)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
        }
        // Navigation/cancellation must wake the worker even without freeing bytes.
        budget.wake_waiters();
        receive.recv_timeout(Duration::from_secs(1)).unwrap();
        worker.join().unwrap();
    }

    fn image(budget: &Arc<MemoryBudget>, bytes: usize) -> Arc<DecodedImage> {
        Arc::new(
            DecodedImage::new(
                vec![0; bytes],
                1,
                1,
                1,
                1,
                1,
                budget.try_reserve(bytes).unwrap(),
            )
            .unwrap(),
        )
    }

    #[test]
    fn leases_bound_total_and_follow_external_ownership() {
        let budget = MemoryBudget::new(100);
        let mut lease = budget.try_reserve(80).unwrap();
        assert!(budget.try_reserve(21).is_none());
        let split = lease.split_off(30).unwrap();
        assert_eq!(budget.used(), 80);
        drop(split);
        assert_eq!(budget.used(), 50);
        assert!(!lease.resize(101));
        assert!(lease.resize(100));
        drop(lease);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn lru_evicts_oldest_unheld_image_and_protects_current() {
        let budget = MemoryBudget::new(100);
        let mut cache = DecodedCache::new(Arc::clone(&budget));
        cache.insert(0, image(&budget, 20), false);
        cache.insert(1, image(&budget, 20), true);
        cache.insert(2, image(&budget, 20), true);
        let held = cache.get(0).unwrap();
        assert!(cache.evict_for(60, Some(2)));
        assert!(!cache.contains(1));
        assert!(cache.contains(0));
        assert!(cache.contains(2));
        assert_eq!(cache.evict_all_except(Some(2)), 0);
        drop(held);
        assert_eq!(cache.evict_all_except(Some(2)), 20);
        assert_eq!(budget.used(), 20);
        assert!(cache.take_prefetched(2));
        assert!(!cache.take_prefetched(2));
        cache.clear();
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn rekey_preserves_pixels_leases_prefetch_flags_and_lru_order() {
        let budget = MemoryBudget::new(100);
        let mut cache = DecodedCache::new(Arc::clone(&budget));
        let old: Vec<_> = ["a.ARW", "b.ARW", "c.ARW"].map(PathBuf::from).into();
        cache.insert(0, image(&budget, 10), false);
        cache.insert(1, image(&budget, 20), true);
        cache.insert(2, image(&budget, 30), true);
        let removed_but_held = cache.get(0).unwrap();
        drop(cache.get(1)); // B is most recently used; C must be evicted first.
        let b = Arc::as_ptr(&cache.entries[&1].image);
        let c = Arc::as_ptr(&cache.entries[&2].image);
        let touched = [cache.entries[&1].touched, cache.entries[&2].touched];
        let clock = cache.clock;
        let new = [old[2].clone(), old[1].clone()];
        cache.rekey(&old, &new);
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(Arc::as_ptr(&cache.entries[&0].image), c);
        assert_eq!(Arc::as_ptr(&cache.entries[&1].image), b);
        assert_eq!(
            [cache.entries[&1].touched, cache.entries[&0].touched],
            touched
        );
        assert_eq!(cache.clock, clock);
        assert_eq!(cache.bytes(), 50);
        // Removing the cache entry cannot pretend an uploader's Arc was freed.
        assert_eq!(budget.used(), 60);
        drop(removed_but_held);
        assert_eq!(budget.used(), 50);
        assert!(cache.take_prefetched(0));
        assert!(cache.take_prefetched(1));
        assert_eq!(cache.evict_bytes(1, None), 30);
        assert!(!cache.contains(0));
        assert!(cache.contains(1));
        assert_eq!(budget.used(), 20);
        cache.rekey(&new, &[]);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn rekey_drops_other_folders_and_invalid_old_indices() {
        let budget = MemoryBudget::new(32);
        let mut cache = DecodedCache::new(Arc::clone(&budget));
        cache.insert(0, image(&budget, 8), false);
        cache.insert(99, image(&budget, 8), false);
        cache.rekey(
            &[PathBuf::from("old/DSC00001.ARW")],
            &[PathBuf::from("new/DSC00001.ARW")],
        );
        assert_eq!(cache.bytes(), 0);
        assert_eq!(budget.used(), 0);
    }
}
