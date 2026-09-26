//! The kernel's pure core (`docs/m3-design.md` §6.3): what an operation sends next and how
//! a completion advances it, over an abstract memory interface, with no runtime.
//!
//! The component only moves messages: it asks [`Operation::access`] for the access due
//! at the current step, sends it, and hands its response to [`Operation::complete`]. When
//! `access` returns `None`, the operation has finished and the held `ENTER` is answered.
//!
//! # Operations (M3.4a)
//!
//! M3.4a has no processes and no syscalls. An `ENTER` starts one of two prototype
//! operations, chosen by the value written (§6.3):
//!
//! - [`Op::Script`], when the value is the configured trap frame address: the scripted
//!   gate operation of §17. It reads the trap frame, writes it back with `sepc + 4`, and
//!   writes one byte, [`SCRIPT_BYTE`], to UART TX.
//! - [`Op::Shutdown`], for any other value, a guest bug: it writes `action = Shutdown`
//!   and `reason = 1` (system failure) into the trap frame (§7.4).
//!
//! Every read and write of the frame is split into accesses of at most [`MAX_ACCESS`]
//! bytes that do not cross a 4 KiB page, in address order.
//!
//! # Working data
//!
//! The bytes a `Script` read from the frame are kernel-owned state while it runs (§6.8):
//! after read *i* they are the first *i* chunks, during the writes the whole frame as read,
//! and they are dropped with the last frame write. Nothing survives the operation.

use crate::config::{FRAME_ACTION, FRAME_BYTES, FRAME_SEPC, KernelConfig};

/// The largest single kernel access (§6.3).
pub const MAX_ACCESS: u64 = 16;
/// The page an access may not cross.
pub const PAGE: u64 = 4096;
/// The byte the scripted operation writes to UART TX.
pub const SCRIPT_BYTE: u8 = b'k';
/// The `reason` a `Shutdown` operation writes: system failure (§7.4).
pub const SHUTDOWN_REASON: u32 = 1;

/// A prototype operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    /// The scripted gate operation: read the frame, write it back with `sepc + 4`, write
    /// [`SCRIPT_BYTE`] to UART TX.
    Script,
    /// A bad `ENTER` value: write `action = Shutdown` and `reason = 1` into the frame.
    Shutdown,
}

impl Op {
    /// The name inspect and the trace use.
    pub fn name(self) -> &'static str {
        match self {
            Op::Script => "script",
            Op::Shutdown => "shutdown",
        }
    }
}

/// One kernel bus access.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    /// Read `len` bytes at `addr`.
    Read {
        /// The physical address.
        addr: u64,
        /// The length, 1 to [`MAX_ACCESS`].
        len: u32,
    },
    /// Write `data` at `addr`.
    Write {
        /// The physical address.
        addr: u64,
        /// The bytes, 1 to [`MAX_ACCESS`].
        data: Vec<u8>,
    },
}

impl Access {
    /// The physical address.
    pub fn addr(&self) -> u64 {
        match self {
            Access::Read { addr, .. } | Access::Write { addr, .. } => *addr,
        }
    }

    /// The length in bytes.
    pub fn len(&self) -> u64 {
        match self {
            Access::Read { len, .. } => u64::from(*len),
            Access::Write { data, .. } => data.len() as u64,
        }
    }

    /// Whether the access is empty; never true for an access an operation produces.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The successful response to the access an operation sent last.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Completion {
    /// A read's data.
    Data(Vec<u8>),
    /// A write is done.
    Written,
}

/// A completion that does not answer the operation's current access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreError {
    /// The operation has finished: nothing is outstanding.
    Finished,
    /// A write's completion for a read, or the reverse.
    WrongKind,
    /// A read's data is not the length requested.
    DataLength,
}

/// `base..base + len` split into accesses of at most [`MAX_ACCESS`] bytes that do not
/// cross a page, in address order, as `(addr, len)`.
pub fn chunks(base: u64, len: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let (mut addr, end) = (base, base + len);
    while addr < end {
        let page_end = (addr / PAGE + 1) * PAGE;
        let next = end.min(page_end).min(addr + MAX_ACCESS);
        out.push((addr, next - addr));
        addr = next;
    }
    out
}

/// A running operation: what it is, how many of its accesses have completed, and its
/// working data.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Operation {
    op: Op,
    step: u32,
    data: Vec<u8>,
}

impl Operation {
    /// The operation an `ENTER` of `value` starts (§6.3).
    pub fn start(config: &KernelConfig, value: u32) -> Operation {
        let op = if value == config.trap_frame {
            Op::Script
        } else {
            Op::Shutdown
        };
        Operation {
            op,
            step: 0,
            data: Vec::new(),
        }
    }

    /// An operation at `step` with `data`, or `None` if no run reaches it: a step at or
    /// past the last access, or working data of another length than the step requires.
    pub fn from_parts(
        config: &KernelConfig,
        op: Op,
        step: u32,
        data: Vec<u8>,
    ) -> Option<Operation> {
        let op = Operation { op, step, data };
        (step < op.steps(config) && op.data.len() as u64 == op.data_len(config)).then_some(op)
    }

    /// The operation.
    pub fn op(&self) -> Op {
        self.op
    }

    /// The number of accesses completed.
    pub fn step(&self) -> u32 {
        self.step
    }

    /// The working data.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    fn frame_chunks(config: &KernelConfig) -> Vec<(u64, u64)> {
        chunks(u64::from(config.trap_frame), FRAME_BYTES)
    }

    fn shutdown_chunks(config: &KernelConfig) -> Vec<(u64, u64)> {
        chunks(u64::from(config.trap_frame) + FRAME_ACTION, 8)
    }

    /// The number of accesses the operation makes.
    pub fn steps(&self, config: &KernelConfig) -> u32 {
        let n = match self.op {
            Op::Script => 2 * Self::frame_chunks(config).len() + 1,
            Op::Shutdown => Self::shutdown_chunks(config).len(),
        };
        u32::try_from(n).expect("an operation has a handful of accesses")
    }

    /// The working data length the current step requires.
    fn data_len(&self, config: &KernelConfig) -> u64 {
        let chunks = Self::frame_chunks(config);
        let step = self.step as usize;
        match self.op {
            Op::Script if step < chunks.len() => chunks[..step].iter().map(|c| c.1).sum(),
            Op::Script if step < 2 * chunks.len() => FRAME_BYTES,
            Op::Script | Op::Shutdown => 0,
        }
    }

    /// The access due at the current step, or `None` once the operation has finished.
    pub fn access(&self, config: &KernelConfig) -> Option<Access> {
        let step = self.step as usize;
        match self.op {
            Op::Script => {
                let chunks = Self::frame_chunks(config);
                let n = chunks.len();
                if step < n {
                    let (addr, len) = chunks[step];
                    let len = u32::try_from(len).expect("chunks are at most 16 bytes");
                    Some(Access::Read { addr, len })
                } else if step < 2 * n {
                    let (addr, len) = chunks[step - n];
                    let mut frame = self.data.clone();
                    let at = FRAME_SEPC as usize;
                    let sepc = u32::from_le_bytes(frame[at..at + 4].try_into().expect("4 bytes"));
                    frame[at..at + 4].copy_from_slice(&sepc.wrapping_add(4).to_le_bytes());
                    let offset = (addr - u64::from(config.trap_frame)) as usize;
                    let data = frame[offset..offset + len as usize].to_vec();
                    Some(Access::Write { addr, data })
                } else if step == 2 * n {
                    Some(Access::Write {
                        addr: config.uart_tx,
                        data: vec![SCRIPT_BYTE],
                    })
                } else {
                    None
                }
            }
            Op::Shutdown => {
                let chunks = Self::shutdown_chunks(config);
                let (addr, len) = *chunks.get(step)?;
                let mut words = [0u8; 8];
                words[..4].copy_from_slice(&1u32.to_le_bytes());
                words[4..].copy_from_slice(&SHUTDOWN_REASON.to_le_bytes());
                let base = u64::from(config.trap_frame) + FRAME_ACTION;
                let offset = (addr - base) as usize;
                let data = words[offset..offset + len as usize].to_vec();
                Some(Access::Write { addr, data })
            }
        }
    }

    /// Advances past the current access, which `completion` answers. Changes nothing on
    /// an error.
    pub fn complete(
        &mut self,
        config: &KernelConfig,
        completion: Completion,
    ) -> Result<(), CoreError> {
        let access = self.access(config).ok_or(CoreError::Finished)?;
        match (access, completion) {
            (Access::Read { len, .. }, Completion::Data(data)) => {
                if data.len() as u64 != u64::from(len) {
                    return Err(CoreError::DataLength);
                }
                self.data.extend_from_slice(&data);
            }
            (Access::Write { .. }, Completion::Written) => {}
            _ => return Err(CoreError::WrongKind),
        }
        self.step += 1;
        if self.data.len() as u64 != self.data_len(config) {
            // Only the last frame write of a `Script` drops the frame.
            self.data = Vec::new();
        }
        Ok(())
    }

    /// Whether every access has completed.
    pub fn is_finished(&self, config: &KernelConfig) -> bool {
        self.step >= self.steps(config)
    }
}
