//! Processes: the PCB table, the run queue, the running PID, and the frames they own
//! (`docs/m3-design.md` §6.4, §6.6, §6.8).
//!
//! # Ownership of a context
//!
//! A process's register context has exactly one authoritative home at any time:
//!
//! - **Running:** the hart owns it while the process executes; after a trap, the trap
//!   frame in RAM does, until the kernel decides. The PCB holds **no** copy.
//! - **Ready:** the PCB's `context` owns it: `x1`–`x31`, `pc` (the `sepc` to resume at),
//!   and `sstatus`.
//! - **Exited / Faulted:** nobody; the process never runs again.
//!
//! The kernel moves a context from the trap frame into the PCB when the process is
//! switched away from, and out of the PCB into the trap frame when it is dispatched
//! (§6.6). RAM always owns the page tables and user pages; the kernel owns only the
//! metadata here: the PCBs, the queue, the running PID, and the frame owners.
//!
//! # PIDs
//!
//! A PID is the boot image's index + 1 (§6.4), so PIDs are canonical: the same plan gives
//! the same PIDs, never from a counter, a clock, or a hash. The table is a `Vec` in
//! ascending PID order.
//!
//! # Invariants
//!
//! [`Processes::check`] states them all; restore relies on it, and every transition here
//! preserves it. At most one process is `Running`, and it is the current PID; every
//! queue entry is a distinct `Ready` process, and every `Ready` process is queued; only a
//! `Ready` process holds a context; terminal processes own no frames; and the allocated
//! frames are exactly the live processes' frames plus those of the creation in flight.

use std::collections::VecDeque;

use systemscope_elf::Perms;

use crate::frames::Frames;
use crate::space::{Region, Space};

/// A process's lifecycle state (§6.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProcState {
    /// Runnable, in the run queue.
    Ready,
    /// On the hart.
    Running,
    /// Ended with `status`. Only M3.5's `exit` produces it.
    Exited {
        /// The exit status.
        status: i32,
    },
    /// Killed by a trap from U-mode other than `ecall` (§6.7).
    Faulted {
        /// `scause`.
        cause: u32,
        /// `sepc`.
        epc: u32,
        /// `stval`.
        tval: u32,
    },
}

impl ProcState {
    /// Whether the process can never run again.
    pub fn is_terminal(self) -> bool {
        matches!(self, ProcState::Exited { .. } | ProcState::Faulted { .. })
    }

    /// The name inspect uses.
    pub fn name(self) -> &'static str {
        match self {
            ProcState::Ready => "ready",
            ProcState::Running => "running",
            ProcState::Exited { .. } => "exited",
            ProcState::Faulted { .. } => "faulted",
        }
    }
}

/// A saved register context (§6.4): what the trap frame's first 0x84 bytes hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Context {
    /// `x1`–`x31`.
    pub regs: [u32; 31],
    /// The `sepc` to resume at.
    pub pc: u32,
    /// `sstatus`.
    pub sstatus: u32,
}

/// The bytes of the trap frame a context fills: `x1`–`x31`, `sepc`, `sstatus`.
pub const CONTEXT_BYTES: usize = 0x84;
/// The index of `sp` (`x2`) in [`Context::regs`].
const SP: usize = 1;

impl Context {
    /// A new process's context (§6.6): all zero but `sp = STACK_TOP` and `pc = entry`;
    /// `sstatus` 0, so `SPP = U` and `SPIE = SUM = MXR = 0`.
    pub fn initial(entry: u32, stack_top: u32) -> Context {
        let mut regs = [0; 31];
        regs[SP] = stack_top;
        Context {
            regs,
            pc: entry,
            sstatus: 0,
        }
    }

    /// The context as the trap frame lays it out.
    pub fn to_frame(&self) -> [u8; CONTEXT_BYTES] {
        let mut out = [0; CONTEXT_BYTES];
        let words = self.regs.iter().chain([&self.pc, &self.sstatus]);
        for (chunk, w) in out.as_chunks_mut::<4>().0.iter_mut().zip(words) {
            chunk.copy_from_slice(&w.to_le_bytes());
        }
        out
    }

    /// The context in the first [`CONTEXT_BYTES`] of a trap frame.
    pub fn from_frame(frame: &[u8]) -> Context {
        let word = |i: usize| u32::from_le_bytes(frame[4 * i..4 * i + 4].try_into().expect("4"));
        Context {
            regs: std::array::from_fn(word),
            pc: word(31),
            sstatus: word(32),
        }
    }
}

/// A process control block (§6.4).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Pcb {
    /// The PID.
    pub pid: u32,
    /// The lifecycle state.
    pub state: ProcState,
    /// The saved context; present exactly when `Ready`.
    pub context: Option<Context>,
    /// The root table's PPN. Kept after the process ends, as history.
    pub root: u32,
    /// The page-table frames: the root, then the level-0 tables. Empty once terminal.
    pub tables: Vec<u32>,
    /// The mapped regions in ascending VA order. Empty once terminal.
    pub regions: Vec<Region>,
}

impl Pcb {
    /// A `Ready` process with its new context, built in `space`.
    pub fn new(pid: u32, space: &Space, context: Context) -> Pcb {
        Pcb {
            pid,
            state: ProcState::Ready,
            context: Some(context),
            root: space.root,
            tables: space.tables.clone(),
            regions: space.regions.clone(),
        }
    }

    /// Every frame the process owns: its tables, then its regions' frames.
    pub fn frames(&self) -> Vec<u32> {
        let pages = self.regions.iter().flat_map(|r| r.frames.iter().copied());
        self.tables.iter().copied().chain(pages).collect()
    }
}

/// A change that would break an invariant: a kernel bug, never a guest error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Violation(pub &'static str);

/// The process table, the run queue, the running PID, and the frame pool.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Processes {
    pcbs: Vec<Pcb>,
    queue: VecDeque<u32>,
    current: Option<u32>,
    frames: Frames,
}

impl Processes {
    /// No process, an empty queue, and an all-free pool.
    pub fn new(frames: Frames) -> Processes {
        Processes {
            pcbs: Vec::new(),
            queue: VecDeque::new(),
            current: None,
            frames,
        }
    }

    /// Everything, from a decoded snapshot; the caller checks it.
    pub(crate) fn from_parts(
        pcbs: Vec<Pcb>,
        queue: VecDeque<u32>,
        current: Option<u32>,
        frames: Frames,
    ) -> Processes {
        Processes {
            pcbs,
            queue,
            current,
            frames,
        }
    }

    /// The PCBs in PID order.
    pub fn pcbs(&self) -> &[Pcb] {
        &self.pcbs
    }

    /// The PCB of `pid`.
    pub fn pcb(&self, pid: u32) -> Option<&Pcb> {
        self.pcbs.iter().find(|p| p.pid == pid)
    }

    fn pcb_mut(&mut self, pid: u32) -> Option<&mut Pcb> {
        self.pcbs.iter_mut().find(|p| p.pid == pid)
    }

    /// The run queue, head first.
    pub fn queue(&self) -> &VecDeque<u32> {
        &self.queue
    }

    /// The running PID.
    pub fn current(&self) -> Option<u32> {
        self.current
    }

    /// The frame pool.
    pub fn frames(&self) -> &Frames {
        &self.frames
    }

    /// The frame pool, to reserve frames for a creation.
    pub fn frames_mut(&mut self) -> &mut Frames {
        &mut self.frames
    }

    /// Adds a newly created `Ready` process at the queue's tail (§6.6). Its PID must be
    /// above every PID in the table, and its frames reserved for it.
    pub fn admit(&mut self, pcb: Pcb) -> Result<(), Violation> {
        if self.pcbs.last().is_some_and(|p| p.pid >= pcb.pid) {
            return Err(Violation("admitted PID is not above every PID"));
        }
        if pcb.state != ProcState::Ready || pcb.context.is_none() {
            return Err(Violation("admitted process is not Ready with a context"));
        }
        let mut owned = pcb.frames();
        owned.sort_unstable();
        if owned != self.frames.owned_by(pcb.pid) {
            return Err(Violation(
                "admitted process's frames are not reserved for it",
            ));
        }
        self.queue.push_back(pcb.pid);
        self.pcbs.push(pcb);
        Ok(())
    }

    /// Switches away from the running process (§6.6): it becomes `Ready` with `context`,
    /// at the queue's tail. Returns its PID.
    pub fn yield_current(&mut self, context: Context) -> Result<u32, Violation> {
        let pid = self
            .current
            .ok_or(Violation("yield with no running process"))?;
        let pcb = self
            .pcb_mut(pid)
            .ok_or(Violation("running PID has no PCB"))?;
        pcb.state = ProcState::Ready;
        pcb.context = Some(context);
        self.queue.push_back(pid);
        self.current = None;
        Ok(pid)
    }

    /// Ends the running process with `state`, which is terminal, and frees its frames
    /// (§6.6). Returns its PID.
    pub fn end_current(&mut self, state: ProcState) -> Result<u32, Violation> {
        if !state.is_terminal() {
            return Err(Violation("a process can only end in a terminal state"));
        }
        let pid = self.current.ok_or(Violation("no running process to end"))?;
        let pcb = self
            .pcb_mut(pid)
            .ok_or(Violation("running PID has no PCB"))?;
        let frames = pcb.frames();
        pcb.state = state;
        pcb.context = None;
        pcb.tables.clear();
        pcb.regions.clear();
        self.frames
            .release(pid, &frames)
            .map_err(|_| Violation("a process frame is not owned by it"))?;
        self.current = None;
        Ok(pid)
    }

    /// The cooperative scheduler (§6.6): takes the queue's head, makes it `Running`, and
    /// returns its PID and the context to dispatch, which leaves the PCB (the trap frame
    /// owns it next). `None` when the queue is empty. Nothing may be running.
    pub fn schedule_next(&mut self) -> Result<Option<(u32, Context)>, Violation> {
        if self.current.is_some() {
            return Err(Violation("scheduling while a process runs"));
        }
        let Some(pid) = self.queue.pop_front() else {
            return Ok(None);
        };
        let pcb = self
            .pcb_mut(pid)
            .ok_or(Violation("queued PID has no PCB"))?;
        let context = pcb
            .context
            .take()
            .ok_or(Violation("queued process has no context"))?;
        if pcb.state != ProcState::Ready {
            return Err(Violation("queued process is not Ready"));
        }
        pcb.state = ProcState::Running;
        self.current = Some(pid);
        Ok(Some((pid, context)))
    }

    /// Checks every invariant of the table, with `creating` the PID and frames of the
    /// creation in flight, if any. `images` is the number of boot images, the PID bound.
    pub fn check(&self, images: usize, creating: Option<(u32, &[u32])>) -> Result<(), Violation> {
        let v = |why| Err(Violation(why));
        let mut last = 0;
        for p in &self.pcbs {
            if p.pid <= last || p.pid as usize > images {
                return v("PIDs not ascending, or outside 1..=images");
            }
            last = p.pid;
            if (p.state == ProcState::Ready) != p.context.is_some() {
                return v("a context on a process that is not Ready, or none on a Ready one");
            }
            if p.state.is_terminal() != p.tables.is_empty()
                || p.state.is_terminal() != p.regions.is_empty()
            {
                return v("a terminal process with frames, or a live one without tables");
            }
            if !p.state.is_terminal() && p.tables[0] != p.root {
                return v("a live process whose root is not its first table");
            }
        }
        let running: Vec<u32> = self
            .pcbs
            .iter()
            .filter(|p| p.state == ProcState::Running)
            .map(|p| p.pid)
            .collect();
        if running.len() > 1 {
            return v("two Running processes");
        }
        if running.first().copied() != self.current {
            return v("the current PID is not the Running process");
        }
        let mut seen = Vec::new();
        for &pid in &self.queue {
            if seen.contains(&pid) {
                return v("a PID queued twice");
            }
            seen.push(pid);
            if self.pcb(pid).map(|p| p.state) != Some(ProcState::Ready) {
                return v("a queue entry that is not a Ready process");
            }
        }
        if self
            .pcbs
            .iter()
            .any(|p| p.state == ProcState::Ready && !self.queue.contains(&p.pid))
        {
            return v("a Ready process missing from the queue");
        }
        if let Some((pid, _)) = creating
            && (pid == 0 || self.pcb(pid).is_some() || pid as usize > images)
        {
            return v("the creation in flight has a used or invalid PID");
        }
        let mut expected = vec![0u32; self.frames.count()];
        let base = self.frames.base();
        let owners = self
            .pcbs
            .iter()
            .map(|p| (p.pid, p.frames()))
            .chain(creating.map(|(pid, f)| (pid, f.to_vec())));
        for (pid, frames) in owners {
            for ppn in frames {
                let slot = ppn
                    .checked_sub(base)
                    .and_then(|i| expected.get_mut(i as usize));
                match slot {
                    Some(o) if *o == 0 => *o = pid,
                    Some(_) => return v("a frame owned twice"),
                    None => return v("a process frame outside the pool"),
                }
            }
        }
        for (i, &o) in expected.iter().enumerate() {
            if self.frames.owner(base + i as u32) != Some(o) {
                return v("an allocated frame no live process owns, or the reverse");
            }
        }
        Ok(())
    }
}

/// The permission bits' inverse of [`crate::image::perm_bits`], or `None` for bits
/// outside `R`/`W`/`X`.
pub fn perms_from_bits(bits: u8) -> Option<Perms> {
    (bits & !0b1110 == 0).then_some(Perms {
        read: bits & 2 != 0,
        write: bits & 4 != 0,
        execute: bits & 8 != 0,
    })
}
