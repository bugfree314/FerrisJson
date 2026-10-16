//! Disk AVL directory. At most 128 stack frames, no resident per-entry map.
use super::{NONE, Store, store::Record};
impl Store {
    fn branch(&self, id: u64, cancel: &dyn Fn() -> bool) -> Result<Record, String> {
        let r = self.get(id, cancel)?;
        if r[0] != 4 || r[5] == 0 || r[5] > 128 {
            return Err("Invalid directory branch".into());
        }
        Ok(r)
    }
    fn stats(&self, id: u64, cancel: &dyn Fn() -> bool) -> Result<(u64, u64, u64), String> {
        if id == NONE {
            return Ok((0, 0, 0));
        }
        let r = self.branch(id, cancel)?;
        Ok((r[5], r[6], r[9]))
    }
    fn refresh(&mut self, id: u64, mut r: Record, cancel: &dyn Fn() -> bool) -> Result<(), String> {
        let (lh, lw, lb) = self.stats(r[3], cancel)?;
        let (rh, rw, rb) = self.stats(r[4], cancel)?;
        r[5] = 1 + lh.max(rh);
        r[6] = lw
            .checked_add(r[7])
            .and_then(|n| n.checked_add(rw))
            .ok_or("Directory weight overflow")?;
        r[9] = lb
            .checked_add(r[10])
            .and_then(|n| n.checked_add(rb))
            .ok_or("Directory baseline overflow")?;
        self.put(id, r, cancel)
    }
    fn rotate(&mut self, id: u64, left: bool, cancel: &dyn Fn() -> bool) -> Result<u64, String> {
        let mut a = self.branch(id, cancel)?;
        let child = if left { a[4] } else { a[3] };
        let mut b = self.branch(child, cancel)?;
        if left {
            a[4] = b[3];
            b[3] = id;
        } else {
            a[3] = b[4];
            b[4] = id;
        }
        self.refresh(id, a, cancel)?;
        self.refresh(child, b, cancel)?;
        Ok(child)
    }
    /// In-order cursor: each directory edge is visited once instead of a rank
    /// lookup from the root for every child. Only use with a stable directory.
    pub(crate) fn directory_next(
        &self,
        path: &mut [u64; 128],
        depth: &mut usize,
        next: &mut u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Option<Record>, String> {
        while *next != NONE {
            if *depth == path.len() {
                return Err("Directory depth limit".into());
            }
            path[*depth] = *next;
            *depth += 1;
            *next = self.branch(*next, cancel)?[3];
        }
        if *depth == 0 {
            return Ok(None);
        }
        *depth -= 1;
        let r = self.branch(path[*depth], cancel)?;
        *next = r[4];
        Ok(Some(r))
    }
    pub(crate) fn directory_put(
        &mut self,
        root: u64,
        entry: [u64; 5],
        cancel: &dyn Fn() -> bool,
    ) -> Result<u64, String> {
        self.insert_directory(root, entry, 0, cancel)
    }
    fn insert_directory(
        &mut self,
        id: u64,
        v: [u64; 5],
        depth: u32,
        cancel: &dyn Fn() -> bool,
    ) -> Result<u64, String> {
        super::check(cancel)?;
        if depth >= 128 {
            return Err("Directory depth/cycle limit".into());
        }
        if id == NONE {
            let mut r = [0; 12];
            r[0] = 4;
            r[1] = v[0];
            r[2] = v[1];
            r[3] = NONE;
            r[4] = NONE;
            r[5] = 1;
            r[6] = v[2];
            r[7] = v[2];
            r[8] = v[3];
            r[9] = v[4];
            r[10] = v[4];
            return self.append(r, cancel);
        }
        let mut r = self.branch(id, cancel)?;
        if v[0] == r[1] {
            r[2] = v[1];
            r[7] = v[2];
            r[8] = v[3];
            r[10] = v[4];
        } else {
            let side = if v[0] < r[1] { 3 } else { 4 };
            r[side] = self.insert_directory(r[side], v, depth + 1, cancel)?;
        }
        self.refresh(id, r, cancel)?;
        let (lh, _, _) = self.stats(r[3], cancel)?;
        let (rh, _, _) = self.stats(r[4], cancel)?;
        if lh > rh + 1 {
            let child = self.branch(r[3], cancel)?;
            if self.stats(child[4], cancel)?.0 > self.stats(child[3], cancel)?.0 {
                r[3] = self.rotate(r[3], true, cancel)?;
                self.refresh(id, r, cancel)?;
            }
            return self.rotate(id, false, cancel);
        }
        if rh > lh + 1 {
            let child = self.branch(r[4], cancel)?;
            if self.stats(child[3], cancel)?.0 > self.stats(child[4], cancel)?.0 {
                r[4] = self.rotate(r[4], false, cancel)?;
                self.refresh(id, r, cancel)?;
            }
            return self.rotate(id, true, cancel);
        }
        Ok(id)
    }
    pub(crate) fn directory_find(
        &self,
        mut id: u64,
        key: u64,
        by_prefix: bool,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Option<Record>, String> {
        let mut result = None;
        for _ in 0..128 {
            if id == NONE {
                return Ok(result);
            }
            let r = self.branch(id, cancel)?;
            let k = if by_prefix { r[8] } else { r[1] };
            if k <= key {
                result = Some(r);
                id = r[4];
            } else {
                id = r[3];
            }
        }
        Err("Directory cycle/depth limit".into())
    }
    pub(crate) fn directory_sums(
        &self,
        root: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(u64, u64), String> {
        let (_, a, b) = self.stats(root, cancel)?;
        Ok((a, b))
    }
    pub(crate) fn directory_prefix(
        &self,
        mut id: u64,
        key: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(u64, u64), String> {
        let (mut actual, mut baseline) = (0u64, 0u64);
        for _ in 0..128 {
            if id == NONE {
                return Ok((actual, baseline));
            }
            let r = self.branch(id, cancel)?;
            if r[1] < key {
                let (_, a, b) = self.stats(r[3], cancel)?;
                actual = actual
                    .checked_add(a)
                    .and_then(|n| n.checked_add(r[7]))
                    .ok_or("Prefix overflow")?;
                baseline = baseline
                    .checked_add(b)
                    .and_then(|n| n.checked_add(r[10]))
                    .ok_or("Prefix overflow")?;
                id = r[4];
            } else {
                id = r[3];
            }
        }
        Err("Directory cycle/depth limit".into())
    }
    pub(crate) fn directory_weight(
        &self,
        mut id: u64,
        mut rank: u64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(Record, u64), String> {
        for _ in 0..128 {
            if id == NONE {
                return Err("Directory rank out of range".into());
            }
            let r = self.branch(id, cancel)?;
            let (_, left, _) = self.stats(r[3], cancel)?;
            if rank < left {
                id = r[3];
            } else if rank - left < r[7] {
                return Ok((r, rank - left));
            } else {
                rank -= left + r[7];
                id = r[4];
            }
        }
        Err("Directory cycle/depth limit".into())
    }
}
