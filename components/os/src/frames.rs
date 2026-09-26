//! The frame allocator (`docs/m3-design.md` §6.4, §11.2).
//!
//! The pool is the configured frame-pool range, a whole number of 4 KiB frames, outside
//! the firmware, the kernel megapage, and every device window: the allocator hands out
//! nothing else. Allocation is **lowest-free-first**, so the frames a process gets depend
//! only on which frames are free, never on history or a clock.
//!
//! Each frame records its owner, the PID it was reserved for (0 when free). The bitmap
//! of §6.8 is the set of frames with an owner; the owners make the ownership invariant
//! checkable: every allocated frame belongs to exactly one live process or to the
//! process being created, and a frame is freed only by its owner.

use crate::core::PAGE;

/// A frame operation that would break the allocator's invariants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// The frame is not in the pool.
    OutsidePool(u32),
    /// The frame is free, or owned by another PID.
    NotOwned(u32),
}

/// The pool's frames and their owners.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Frames {
    /// The first frame's PPN.
    base: u32,
    /// The owner of frame `base + i`: 0 when free, else a PID.
    owners: Vec<u32>,
}

impl Frames {
    /// An all-free pool of the frames in `base..base + size` (page-aligned bytes).
    pub fn new(base: u64, size: u64) -> Frames {
        Frames {
            base: (base / PAGE) as u32,
            owners: vec![0; (size / PAGE) as usize],
        }
    }

    /// The number of frames in the pool.
    pub fn count(&self) -> usize {
        self.owners.len()
    }

    /// The number of free frames.
    pub fn free_count(&self) -> usize {
        self.owners.iter().filter(|&&o| o == 0).count()
    }

    fn index(&self, ppn: u32) -> Option<usize> {
        let i = ppn.checked_sub(self.base)? as usize;
        (i < self.owners.len()).then_some(i)
    }

    /// The owner of `ppn`: `None` outside the pool, `Some(0)` when free.
    pub fn owner(&self, ppn: u32) -> Option<u32> {
        self.index(ppn).map(|i| self.owners[i])
    }

    /// The frames `pid` owns, ascending.
    pub fn owned_by(&self, pid: u32) -> Vec<u32> {
        (self.base..)
            .zip(&self.owners)
            .filter(|&(_, &o)| o == pid)
            .map(|(ppn, _)| ppn)
            .collect()
    }

    /// Reserves `n` frames for `pid`, lowest free first, and returns their PPNs in
    /// ascending order; or, if fewer than `n` are free, reserves nothing and returns
    /// `None`. `pid` is never 0.
    pub fn reserve(&mut self, pid: u32, n: usize) -> Option<Vec<u32>> {
        assert_ne!(pid, 0, "PID 0 marks a free frame");
        let free: Vec<usize> = self
            .owners
            .iter()
            .enumerate()
            .filter(|&(_, &o)| o == 0)
            .map(|(i, _)| i)
            .take(n)
            .collect();
        if free.len() < n {
            return None;
        }
        for &i in &free {
            self.owners[i] = pid;
        }
        Some(free.iter().map(|&i| self.base + i as u32).collect())
    }

    /// Frees every frame in `ppns`, which must all be owned by `pid`; frees nothing on an
    /// error.
    pub fn release(&mut self, pid: u32, ppns: &[u32]) -> Result<(), FrameError> {
        for &ppn in ppns {
            match self.owner(ppn) {
                None => return Err(FrameError::OutsidePool(ppn)),
                Some(o) if o != pid || pid == 0 => return Err(FrameError::NotOwned(ppn)),
                Some(_) => {}
            }
        }
        for &ppn in ppns {
            let i = self.index(ppn).expect("checked above");
            self.owners[i] = 0;
        }
        Ok(())
    }

    /// The bitmap of §6.8: bit `i % 8` of byte `i / 8` is set when frame `i` is allocated.
    pub fn bitmap(&self) -> Vec<u8> {
        let mut bits = vec![0u8; self.owners.len().div_ceil(8)];
        for (i, &o) in self.owners.iter().enumerate() {
            if o != 0 {
                bits[i / 8] |= 1 << (i % 8);
            }
        }
        bits
    }

    /// Whether frame `i` is set in `bitmap`.
    pub fn bit(bitmap: &[u8], i: usize) -> bool {
        bitmap.get(i / 8).is_some_and(|b| b >> (i % 8) & 1 == 1)
    }

    /// The pool's first PPN.
    pub fn base(&self) -> u32 {
        self.base
    }

    /// Sets the owners from a restored snapshot; the caller has checked them.
    pub(crate) fn from_owners(base: u32, owners: Vec<u32>) -> Frames {
        Frames { base, owners }
    }
}
