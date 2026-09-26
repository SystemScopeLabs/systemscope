//! The process-mode pure core (`docs/m3-design.md` §6.3, §6.5–§6.7, §8): the kernel
//! operations of a kernel with processes, from a [`ProcessPlan`] or a [`DiskBoot`], over the
//! same abstract memory interface as [`crate::core`], with no runtime.
//!
//! # Operations
//!
//! An `ENTER` of the trap frame address starts `Boot` on the first entry and `Trap` on
//! every later one (§6.3); any other value starts the failure shutdown, as in M3.4a. An
//! operation is a chain of **stages**, each a fixed list of accesses:
//!
//! ```text
//! Boot (plan):  Create(pid 1) → Create(pid 2) → … → Dispatch(head) | Shutdown
//! Boot (disk):  Command → Poll → … → Ack → Staged (block 0)                 (§8.1)
//!               → [Command → Poll → … → Ack → Staged → … → Load] per entry  (§8.2, §8.3)
//!               → Dispatch(head) | Shutdown
//! Trap:  ReadFrame → Return
//!                  → Walk → … → Output → … → Return          (write)
//!                  → Dispatch(head) | Shutdown               (yield, exit, fault kill)
//! ```
//!
//! - **Create** builds one process's address space ([`Space`]): zero, copy, map. Its
//!   frames are reserved when the stage starts, all at once, so pool exhaustion skips the
//!   image with an `os.process.create` error before anything is written, and boot
//!   continues (§6.4). When its last PTE is written, the process is admitted `Ready` at
//!   the queue's tail with its initial context.
//! - **Command**, **Poll**, **Ack** run one block controller READ into staging (§8.2):
//!   `LBA`, `MEM_ADDR` (the staging base), `BLOCK_COUNT`, `COMMAND = READ`; then one
//!   `STATUS` read per stage until `DONE`; then `ACK`, whatever `ERROR` says. The first
//!   READ is block 0; each later one is the next entry's blocks.
//! - **Staged** reads the first bytes of staging: block 0, or an executable's headers.
//!   - Block 0 is checked with `parse_exec_table` for the configured disk and staging
//!     (§8.1). An invalid table, or a controller error reading it, shuts down with reason
//!     1 and a detail naming the check; a valid one is traced as `os.boot` and its entries
//!     are loaded in order.
//!   - An executable's first 52 bytes (its ELF header, or the whole file if shorter) go
//!     to `parse_user_elf32` with the file length and the image range (§8.3). If it needs
//!     more (`PrefixTooShort { needed }`), the next Staged reads the first `needed` bytes
//!     and parses again. A valid image is created (**Load**); a rejected one, like a
//!     controller error on its READ, fails that entry's creation with an
//!     `os.process.create` error, and boot continues with the next entry (§8.2).
//! - **Load** is a Create for a disk entry: the same zero, copy, and map, from the
//!   `UserImage` the kernel's own parse made. PID = entry index + 1.
//! - **ReadFrame** reads the whole trap frame. Then the kernel decides (§6.5–§6.7):
//!   - `SPP = S`: shut down with reason 1;
//!   - `scause = 8` (`ecall` from U): a syscall, decoded from `a7` ([`Syscall`]):
//!     - `getpid`, an unsupported number, and a `write` whose `fd`, count, or range ends
//!       it at once: **Return** of the result;
//!     - `write` of `n > 0` bytes: **Walk** every page of the buffer, then **Output**
//!       the bytes, then **Return** `n`; a page the walk refuses returns `-EFAULT`
//!       before any byte is output;
//!     - `sched_yield`: the frame becomes the caller's context with `a0 = 0` and
//!       `sepc + 4`, the caller goes to the queue's tail, and the head is dispatched;
//!     - `exit` and `exit_group`: the caller becomes `Exited { status: a0 }`, its frames
//!       are freed, and the head is dispatched;
//!   - any other cause: the process becomes `Faulted { scause, sepc, stval }`, its
//!     frames are freed, and the head is dispatched.
//! - **Walk** reads one PTE of the caller's page table ([`walk_step`]).
//! - **Output** reads one chunk of the buffer (at most 16 bytes, crossing no page) from
//!   the physical page the walk found, then writes its bytes to UART TX one at a time.
//! - **Return** writes the result to the frame's `a0`, then `sepc + 4` to its `sepc`.
//!   No other word of the frame changes, and the caller keeps running.
//! - **Dispatch** writes the head's context (`x1`–`x31`, `sepc`, `sstatus`), then its
//!   `satp` and `action = Resume`, into the trap frame. The trampoline does the rest.
//! - **Shutdown** writes `action = Shutdown` and the reason. With an empty queue the
//!   reason is 0 only when every boot image (every table entry) became a process and
//!   every process exited with status 0 (§6.6).
//!
//! Every metadata change happens at a stage boundary, in the same step as the completion
//! that ends the stage; within a stage, the only kernel state that moves is the step and
//! the working data. A syscall is therefore decided exactly once, when its frame read
//! completes, and each of its effects (a byte at the UART, a word of the frame, a queue
//! entry, an exit) is one access or one boundary that a restore never repeats.
//!
//! # Working data
//!
//! A disk boot's stages hold the validated executable table (their [`Cursor`]), and a Load
//! also the headers it parsed; a Staged holds the staging bytes read so far; a Poll the
//! `STATUS` word; a Create or Load the bytes of the copy between its read and its write; a ReadFrame the
//! frame bytes read so far; a Walk the PTE it read; an Output the chunk it read, until its
//! last byte reaches the UART; a Dispatch the context it is writing, which has left the
//! PCB (the process is `Running`) and has not all reached the frame yet. A write's
//! buffer, the pages the walk has found, and its progress live in its stages. All of it
//! is dropped when the operation ends (§6.8). Across operations a disk boot keeps only the
//! number of table entries: the PID bound and the clean-shutdown rule (§17.3).

use systemscope_contracts::trace::Value;

use systemscope_elf::{
    BLOCK_SIZE, ExecTable, UserElfError, UserImage, parse_exec_table, parse_user_elf32,
};

use crate::boot::{
    ACK, ACK_DONE, BLOCK_COUNT, COMMAND, Cursor, LBA, MEM_ADDR, OP_READ, STATUS, STATUS_DONE,
    STATUS_ERROR_SHIFT,
};
use crate::config::{FRAME_ACTION, FRAME_BYTES, KernelConfig};
use crate::core::{Access, Completion, CoreError, chunks};
use crate::image::{BootImage, DiskBoot, ProcessPlan, UserLayout, perm_str};
use crate::process::{CONTEXT_BYTES, Context, Pcb, ProcState, Processes, Violation};
use crate::pte;
use crate::space::{Space, frames_needed, megapage_conflict};
use crate::syscall::{
    A0, Buffer, EBADF, EFAULT, ENOSYS, FRAME_A0, FRAME_A1, FRAME_A2, FRAME_A7, SYS_WRITE, Syscall,
    WRITE_MAX, WalkStep, neg, pte_address, walk_step,
};

/// The offset of `sstatus` in the trap frame.
pub const FRAME_SSTATUS: u64 = 0x80;
/// The offset of `scause` in the trap frame.
pub const FRAME_SCAUSE: u64 = 0x84;
/// The offset of `stval` in the trap frame.
pub const FRAME_STVAL: u64 = 0x88;
/// The offset of `satp` in the trap frame.
pub const FRAME_SATP: u64 = 0x8C;
/// `sstatus.SPP`.
pub const SSTATUS_SPP: u32 = 1 << 8;
/// `scause` of an `ecall` from U-mode.
pub const ECALL_FROM_U: u32 = 8;

/// Trace kind of the start of `Boot` (§6.8): `entries`.
pub const BOOT_KIND: &str = "os.boot";
/// Trace kind of a creation (§6.8): `pid`, `entry`, `root`, `error` (empty on success).
pub const CREATE_KIND: &str = "os.process.create";
/// Trace kind of a mapped segment (§6.8): `pid`, `va`, `memsz`, `perms`.
pub const SEGMENT_KIND: &str = "os.load.segment";
/// Trace kind of a dispatch (§6.8): `from` (0 for none), `to`.
pub const SWITCH_KIND: &str = "os.process.switch";
/// Trace kind of a fault kill (§6.8): `pid`, `cause`, `epc`, `tval`.
pub const FAULT_KIND: &str = "os.process.fault";
/// Trace kind of a decoded syscall (§6.8): `pid`, `nr`, `a0`, `a1`, `a2`.
pub const SYSCALL_ENTER_KIND: &str = "os.syscall.enter";
/// Trace kind of a syscall's result (§6.8): `pid`, `nr`, `ret` (the `a0` bit pattern).
pub const SYSCALL_EXIT_KIND: &str = "os.syscall.exit";
/// Trace kind of an `exit` or `exit_group` (§6.8): `pid`, `status` (`I64`).
pub const EXIT_KIND: &str = "os.process.exit";

/// Where a kernel with processes is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Life {
    /// No `ENTER` yet.
    AwaitBoot,
    /// Booted or booting.
    Up,
    /// A shutdown has been decided.
    Down,
}

/// A trace record an operation asks the component to emit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    /// The trace kind.
    pub kind: &'static str,
    /// The fields.
    pub fields: Vec<(&'static str, Value)>,
}

/// One stage of a process-mode operation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Stage {
    /// Building process `pid` in `space`, whose frames are reserved for it.
    Create {
        /// The PID being created.
        pid: u32,
        /// Its address space.
        space: Space,
    },
    /// Reading the trap frame.
    ReadFrame,
    /// Writing `context` and `satp` for `pid`, which is `Running`.
    Dispatch {
        /// The PID being dispatched.
        pid: u32,
        /// Its context, on its way to the frame.
        context: Context,
        /// Its `satp`.
        satp: u32,
    },
    /// Writing `action = Shutdown` and `reason`.
    Shutdown {
        /// 0 or 1 (§7.4).
        reason: u32,
    },
    /// Reading the level-`level` PTE for page `pages.len()` of a `write` buffer from the
    /// table at `table`.
    Walk {
        /// The buffer.
        buffer: Buffer,
        /// The physical page of each buffer page found so far.
        pages: Vec<u32>,
        /// The PPN of the table the PTE is in: the caller's root at level 1.
        table: u32,
        /// 1 or 0.
        level: u8,
    },
    /// Outputting the chunk of a checked `write` buffer that starts `done` bytes in.
    Output {
        /// The buffer.
        buffer: Buffer,
        /// The physical page of every buffer page.
        pages: Vec<u32>,
        /// The bytes already at the UART.
        done: u32,
    },
    /// Writing a syscall's result to `a0`, then `sepc + 4`.
    Return {
        /// The caller's `sepc`, the `ecall`'s address.
        sepc: u32,
        /// The result, as the `a0` bit pattern.
        value: u32,
    },
    /// Programming the block controller for the cursor's transfer: `LBA`, `MEM_ADDR`,
    /// `BLOCK_COUNT`, then `COMMAND = READ`.
    Command {
        /// Where boot is.
        cursor: Cursor,
    },
    /// Reading `STATUS` once.
    Poll {
        /// Where boot is.
        cursor: Cursor,
    },
    /// Writing `ACK` after `DONE`, with the transfer's `ERROR` code.
    Ack {
        /// Where boot is.
        cursor: Cursor,
        /// `STATUS.ERROR` at `DONE`: 0 for success.
        error: u8,
    },
    /// Reading the first `len` bytes of staging.
    Staged {
        /// Where boot is.
        cursor: Cursor,
        /// The number of bytes: 512 for block 0, else the executable's header bytes.
        len: u32,
    },
    /// Building the process of the cursor's entry (PID = index + 1) in `space`, whose
    /// frames are reserved for it, from the executable in staging.
    Load {
        /// Where boot is.
        cursor: Cursor,
        /// The header bytes `parse_user_elf32` accepted.
        headers: Vec<u8>,
        /// The mapping plan it made from them.
        image: UserImage,
        /// The address space.
        space: Space,
    },
}

impl Stage {
    /// The name inspect uses.
    pub fn name(&self) -> &'static str {
        match self {
            Stage::Create { .. } => "create",
            Stage::ReadFrame => "read_frame",
            Stage::Dispatch { .. } => "dispatch",
            Stage::Shutdown { .. } => "shutdown",
            Stage::Walk { .. } => "walk",
            Stage::Output { .. } => "output",
            Stage::Return { .. } => "return",
            Stage::Command { .. } => "blk_command",
            Stage::Poll { .. } => "blk_poll",
            Stage::Ack { .. } => "blk_ack",
            Stage::Staged { .. } => "read_staging",
            Stage::Load { .. } => "load",
        }
    }
}

/// A running process-mode operation: its stage, the accesses of the stage completed, and
/// the working data.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProcOp {
    stage: Stage,
    step: u32,
    data: Vec<u8>,
}

fn frame_chunks(config: &KernelConfig) -> Vec<(u64, u64)> {
    chunks(u64::from(config.trap_frame), FRAME_BYTES)
}

fn dispatch_chunks(config: &KernelConfig) -> Vec<(u64, u64)> {
    let base = u64::from(config.trap_frame);
    let mut out = chunks(base, CONTEXT_BYTES as u64);
    out.extend(chunks(base + FRAME_SATP, 8));
    out
}

fn shutdown_chunks(config: &KernelConfig) -> Vec<(u64, u64)> {
    chunks(u64::from(config.trap_frame) + FRAME_ACTION, 8)
}

fn staged_chunks(config: &KernelConfig, len: u32) -> Vec<(u64, u64)> {
    chunks(config.staging.base, u64::from(len))
}

/// The bytes the first `step` accesses of a read of `chunks` return.
fn read_len(chunks: &[(u64, u64)], step: u32) -> usize {
    chunks[..step as usize].iter().map(|c| c.1 as usize).sum()
}

/// The physical address of `va` in `buffer`, whose pages are at `pages`.
fn physical(buffer: &Buffer, pages: &[u32], va: u32) -> u64 {
    let page = ((va >> 12) - (buffer.buf >> 12)) as usize;
    u64::from(pages[page]) * crate::core::PAGE + u64::from(va & 0xFFF)
}

fn word(frame: &[u8], offset: u64) -> u32 {
    let at = offset as usize;
    u32::from_le_bytes(frame[at..at + 4].try_into().expect("4 bytes"))
}

/// `scause`'s name, spelled as the CPU's `rv32.trap` records spell exceptions.
pub fn cause_name(scause: u32) -> String {
    let name = match scause {
        0 => "InstructionAddressMisaligned",
        1 => "InstructionAccessFault",
        2 => "IllegalInstruction",
        3 => "Breakpoint",
        4 => "LoadAddressMisaligned",
        5 => "LoadAccessFault",
        6 => "StoreAddressMisaligned",
        7 => "StoreAccessFault",
        8 => "EnvironmentCallFromU",
        9 => "EnvironmentCallFromS",
        12 => "InstructionPageFault",
        13 => "LoadPageFault",
        15 => "StorePageFault",
        other => return format!("scause {other:#x}"),
    };
    name.to_owned()
}

impl ProcOp {
    /// An operation at the start of `stage`.
    pub fn new(stage: Stage) -> ProcOp {
        ProcOp {
            stage,
            step: 0,
            data: Vec::new(),
        }
    }

    /// An operation at `step` of `stage` with `data`, or `None` if no run reaches it: a
    /// step at or past the stage's last access, or working data of another length than
    /// the step requires.
    pub fn from_parts(
        config: &KernelConfig,
        stage: Stage,
        step: u32,
        data: Vec<u8>,
    ) -> Option<ProcOp> {
        let op = ProcOp { stage, step, data };
        (step < op.steps(config) && op.data.len() == op.data_len(config, step)).then_some(op)
    }

    /// The stage.
    pub fn stage(&self) -> &Stage {
        &self.stage
    }

    /// The number of the stage's accesses completed.
    pub fn step(&self) -> u32 {
        self.step
    }

    /// The working data.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// The number of accesses of the stage.
    pub fn steps(&self, config: &KernelConfig) -> u32 {
        let n = match &self.stage {
            Stage::Create { space, .. } | Stage::Load { space, .. } => return space.steps(),
            Stage::ReadFrame => frame_chunks(config).len(),
            Stage::Dispatch { .. } => dispatch_chunks(config).len(),
            Stage::Shutdown { .. } => shutdown_chunks(config).len(),
            Stage::Walk { .. } => 1,
            Stage::Output { buffer, done, .. } => 1 + buffer.chunk(*done).1 as usize,
            Stage::Return { .. } => 2,
            Stage::Command { .. } => 4,
            Stage::Poll { .. } | Stage::Ack { .. } => 1,
            Stage::Staged { len, .. } => staged_chunks(config, *len).len(),
        };
        u32::try_from(n).expect("a bounded number of accesses")
    }

    /// The working data length at `step`.
    fn data_len(&self, config: &KernelConfig, step: u32) -> usize {
        match &self.stage {
            Stage::Create { space, .. } | Stage::Load { space, .. } => space.data_len(step),
            Stage::ReadFrame => read_len(&frame_chunks(config), step),
            Stage::Staged { len, .. } => read_len(&staged_chunks(config, *len), step),
            Stage::Output { buffer, done, .. } if step > 0 => buffer.chunk(*done).1 as usize,
            Stage::Dispatch { .. }
            | Stage::Shutdown { .. }
            | Stage::Walk { .. }
            | Stage::Output { .. }
            | Stage::Return { .. }
            | Stage::Command { .. }
            | Stage::Poll { .. }
            | Stage::Ack { .. } => 0,
        }
    }

    /// The access due at the current step, or `None` once the stage has finished.
    pub fn access(&self, config: &KernelConfig) -> Option<Access> {
        let step = self.step as usize;
        let written = |chunks: Vec<(u64, u64)>, base: u64, bytes: &[u8]| {
            let (addr, len) = *chunks.get(step)?;
            let at = (addr - base) as usize;
            Some(Access::Write {
                addr,
                data: bytes[at..at + len as usize].to_vec(),
            })
        };
        let frame = u64::from(config.trap_frame);
        match &self.stage {
            Stage::Create { space, .. } | Stage::Load { space, .. } => {
                space.access(self.step, &self.data)
            }
            Stage::ReadFrame => {
                let (addr, len) = *frame_chunks(config).get(step)?;
                Some(Access::Read {
                    addr,
                    len: len as u32,
                })
            }
            Stage::Dispatch { context, satp, .. } => {
                let mut bytes = vec![0; FRAME_BYTES as usize];
                bytes[..CONTEXT_BYTES].copy_from_slice(&context.to_frame());
                let at = FRAME_SATP as usize;
                bytes[at..at + 4].copy_from_slice(&satp.to_le_bytes());
                // action = Resume (0) at FRAME_ACTION is already zero.
                written(dispatch_chunks(config), frame, &bytes)
            }
            Stage::Shutdown { reason } => {
                let mut bytes = [0u8; 8];
                bytes[..4].copy_from_slice(&1u32.to_le_bytes());
                bytes[4..].copy_from_slice(&reason.to_le_bytes());
                written(shutdown_chunks(config), frame + FRAME_ACTION, &bytes)
            }
            Stage::Walk {
                buffer,
                pages,
                table,
                level,
            } => (step == 0).then(|| Access::Read {
                addr: pte_address(*table, buffer.page_va(pages.len()), *level),
                len: 4,
            }),
            Stage::Output {
                buffer,
                pages,
                done,
            } => {
                let (va, len) = buffer.chunk(*done);
                match step {
                    0 => Some(Access::Read {
                        addr: physical(buffer, pages, va),
                        len,
                    }),
                    _ if step <= len as usize => Some(Access::Write {
                        addr: config.uart_tx,
                        data: vec![self.data[step - 1]],
                    }),
                    _ => None,
                }
            }
            Stage::Return { sepc, value } => {
                let (offset, word) = match step {
                    0 => (FRAME_A0, *value),
                    1 => (crate::config::FRAME_SEPC, sepc.wrapping_add(4)),
                    _ => return None,
                };
                Some(Access::Write {
                    addr: frame + offset,
                    data: word.to_le_bytes().to_vec(),
                })
            }
            Stage::Command { cursor } => {
                let (lba, blocks) = cursor.transfer();
                let staging = config.staging.base as u32;
                let (offset, value) = [
                    (LBA, lba),
                    (MEM_ADDR, staging),
                    (BLOCK_COUNT, blocks),
                    (COMMAND, OP_READ),
                ]
                .get(step)
                .copied()?;
                Some(Access::Write {
                    addr: config.blk.base + offset,
                    data: value.to_le_bytes().to_vec(),
                })
            }
            Stage::Poll { .. } => (step == 0).then(|| Access::Read {
                addr: config.blk.base + STATUS,
                len: 4,
            }),
            Stage::Ack { .. } => (step == 0).then(|| Access::Write {
                addr: config.blk.base + ACK,
                data: ACK_DONE.to_le_bytes().to_vec(),
            }),
            Stage::Staged { len, .. } => {
                let (addr, len) = *staged_chunks(config, *len).get(step)?;
                Some(Access::Read {
                    addr,
                    len: len as u32,
                })
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
                if data.len() != len as usize {
                    return Err(CoreError::DataLength);
                }
                self.data.extend_from_slice(&data);
            }
            (Access::Write { .. }, Completion::Written) => {}
            _ => return Err(CoreError::WrongKind),
        }
        self.step += 1;
        if self.step < self.steps(config) && self.data.len() != self.data_len(config, self.step) {
            // A copy's write drops the bytes it wrote.
            self.data.clear();
        }
        Ok(())
    }

    /// Whether every access of the stage has completed.
    pub fn is_finished(&self, config: &KernelConfig) -> bool {
        self.step >= self.steps(config)
    }
}

/// Where a kernel's processes come from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// Staged, host-validated images (M3.4b).
    Plan(ProcessPlan),
    /// The boot disk, through the block controller (M3.6).
    Disk(DiskBoot),
}

impl Source {
    /// The user layout.
    pub fn layout(&self) -> &UserLayout {
        match self {
            Source::Plan(plan) => &plan.layout,
            Source::Disk(disk) => &disk.layout,
        }
    }
}

/// The process-mode kernel metadata: the process source, the number of boot images, the
/// processes, and where the kernel is in its life.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Model {
    /// Where the processes come from.
    pub source: Source,
    /// The number of boot images, the PID bound: the plan's images, or the executable
    /// table's entries once boot has validated it (0 before).
    pub entries: usize,
    /// The processes and frames.
    pub procs: Processes,
    /// Where the kernel is in its life.
    pub life: Life,
}

fn note(kind: &'static str, fields: Vec<(&'static str, Value)>) -> Note {
    Note { kind, fields }
}

fn shutdown_note(reason: u32, detail: String) -> Note {
    note(
        crate::kernel::SHUTDOWN_KIND,
        vec![
            ("reason", Value::U64(u64::from(reason))),
            ("detail", Value::Str(detail)),
        ],
    )
}

fn exit_note(pid: u32, nr: u32, ret: u32) -> Note {
    note(
        SYSCALL_EXIT_KIND,
        vec![
            ("pid", Value::U64(u64::from(pid))),
            ("nr", Value::U64(u64::from(nr))),
            ("ret", Value::U64(u64::from(ret))),
        ],
    )
}

fn create_error(pid: u32, entry: u32, error: &str) -> Note {
    note(
        CREATE_KIND,
        vec![
            ("pid", Value::U64(u64::from(pid))),
            ("entry", Value::U64(u64::from(entry))),
            ("root", Value::U64(0)),
            ("error", Value::Str(error.to_owned())),
        ],
    )
}

impl Model {
    /// A model awaiting boot, with the processes `procs` (none, and an all-free pool).
    pub fn new(source: Source, procs: Processes) -> Model {
        let entries = match &source {
            Source::Plan(plan) => plan.images.len(),
            Source::Disk(_) => 0,
        };
        Model {
            source,
            entries,
            procs,
            life: Life::AwaitBoot,
        }
    }

    /// The name of the operation an `ENTER` of `value` starts now: `boot`, `trap`, or
    /// `shutdown`.
    pub fn op_name(&self, config: &KernelConfig, value: u32) -> &'static str {
        match self.life {
            _ if value != config.trap_frame => "shutdown",
            Life::AwaitBoot => "boot",
            Life::Up | Life::Down => "trap",
        }
    }

    /// Starts the operation of an `ENTER` of `value` (§6.3). A kernel bug, such as an
    /// entry after the shutdown or with no process running, is a [`Violation`].
    pub fn start(
        &mut self,
        config: &KernelConfig,
        value: u32,
        notes: &mut Vec<Note>,
    ) -> Result<ProcOp, Violation> {
        if value != config.trap_frame {
            if self.life == Life::Down {
                return Err(Violation("ENTER after the shutdown"));
            }
            self.life = Life::Down;
            notes.push(shutdown_note(
                1,
                "ENTER value is not the trap frame".to_owned(),
            ));
            return Ok(ProcOp::new(Stage::Shutdown { reason: 1 }));
        }
        match self.life {
            Life::AwaitBoot => {
                self.life = Life::Up;
                if let Source::Disk(_) = self.source {
                    return Ok(ProcOp::new(Stage::Command {
                        cursor: Cursor::table(),
                    }));
                }
                notes.push(note(
                    BOOT_KIND,
                    vec![("entries", Value::U64(self.entries as u64))],
                ));
                self.next_creation(config, 0, notes)
            }
            Life::Up if self.procs.current().is_some() => Ok(ProcOp::new(Stage::ReadFrame)),
            Life::Up => Err(Violation("ENTER with no process running")),
            Life::Down => Err(Violation("ENTER after the shutdown")),
        }
    }

    /// Moves past the finished stage of `op`: the next stage, or `None` when the
    /// operation has finished.
    pub fn advance(
        &mut self,
        config: &KernelConfig,
        op: ProcOp,
        notes: &mut Vec<Note>,
    ) -> Result<Option<ProcOp>, Violation> {
        match op.stage {
            Stage::Create { pid, space } => {
                let Source::Plan(plan) = &self.source else {
                    return Err(Violation("a plan creation without a plan"));
                };
                let image = plan.images[pid as usize - 1].image.clone();
                self.admit(pid, &image, &space, notes)?;
                self.next_creation(config, pid as usize, notes).map(Some)
            }
            Stage::Load {
                cursor,
                image,
                space,
                ..
            } => {
                self.admit(cursor.pid(), &image, &space, notes)?;
                self.next_load(cursor.table, cursor.index + 1, notes)
                    .map(Some)
            }
            Stage::Command { cursor } => Ok(Some(ProcOp::new(Stage::Poll { cursor }))),
            Stage::Poll { cursor } => {
                let status = word(&op.data, 0);
                let stage = if status & STATUS_DONE != 0 {
                    Stage::Ack {
                        cursor,
                        error: (status >> STATUS_ERROR_SHIFT) as u8,
                    }
                } else {
                    Stage::Poll { cursor }
                };
                Ok(Some(ProcOp::new(stage)))
            }
            Stage::Ack { cursor, error } => self.acked(cursor, error, notes).map(Some),
            Stage::Staged { cursor, len } => {
                self.staged(config, cursor, len, op.data, notes).map(Some)
            }
            Stage::ReadFrame => self.decide(&op.data, notes).map(Some),
            Stage::Walk {
                buffer,
                mut pages,
                level,
                ..
            } => {
                let pte = word(&op.data, 0);
                let va = buffer.page_va(pages.len());
                let stage = match walk_step(pte, level, va) {
                    WalkStep::Next(table) => Stage::Walk {
                        buffer,
                        pages,
                        table,
                        level: 0,
                    },
                    WalkStep::Page(ppn) => {
                        pages.push(ppn);
                        if pages.len() == buffer.pages() {
                            Stage::Output {
                                buffer,
                                pages,
                                done: 0,
                            }
                        } else {
                            Stage::Walk {
                                buffer,
                                pages,
                                table: self.running_root()?,
                                level: 1,
                            }
                        }
                    }
                    WalkStep::Fault => {
                        return self
                            .ret(SYS_WRITE, buffer.sepc, neg(EFAULT), notes)
                            .map(Some);
                    }
                };
                Ok(Some(ProcOp::new(stage)))
            }
            Stage::Output {
                buffer,
                pages,
                done,
            } => {
                let done = done + buffer.chunk(done).1;
                if done == buffer.n {
                    return self.ret(SYS_WRITE, buffer.sepc, buffer.n, notes).map(Some);
                }
                Ok(Some(ProcOp::new(Stage::Output {
                    buffer,
                    pages,
                    done,
                })))
            }
            Stage::Dispatch { .. } | Stage::Shutdown { .. } | Stage::Return { .. } => Ok(None),
        }
    }

    /// The running process's root table.
    fn running_root(&self) -> Result<u32, Violation> {
        let pid = self
            .procs
            .current()
            .ok_or(Violation("a syscall with no process running"))?;
        Ok(self
            .procs
            .pcb(pid)
            .ok_or(Violation("running PID has no PCB"))?
            .root)
    }

    /// Returns `value` from syscall `nr` to the running process, which called it at
    /// `sepc`: the `os.syscall.exit` record and the Return stage.
    fn ret(
        &self,
        nr: u32,
        sepc: u32,
        value: u32,
        notes: &mut Vec<Note>,
    ) -> Result<ProcOp, Violation> {
        let pid = self
            .procs
            .current()
            .ok_or(Violation("a syscall with no process running"))?;
        notes.push(exit_note(pid, nr, value));
        Ok(ProcOp::new(Stage::Return { sepc, value }))
    }

    /// The syscall in `frame` (§6.5), decided once, when the frame read completes.
    fn syscall(&mut self, frame: &[u8], notes: &mut Vec<Note>) -> Result<ProcOp, Violation> {
        let pid = self
            .procs
            .current()
            .ok_or(Violation("a syscall with no process running"))?;
        let sepc = word(frame, crate::config::FRAME_SEPC);
        let nr = word(frame, FRAME_A7);
        let (a0, a1, a2) = (
            word(frame, FRAME_A0),
            word(frame, FRAME_A1),
            word(frame, FRAME_A2),
        );
        notes.push(note(
            SYSCALL_ENTER_KIND,
            vec![
                ("pid", Value::U64(u64::from(pid))),
                ("nr", Value::U64(u64::from(nr))),
                ("a0", Value::U64(u64::from(a0))),
                ("a1", Value::U64(u64::from(a1))),
                ("a2", Value::U64(u64::from(a2))),
            ],
        ));
        match Syscall::decode(nr) {
            Syscall::GetPid => self.ret(nr, sepc, pid, notes),
            Syscall::Unsupported(_) => self.ret(nr, sepc, neg(ENOSYS), notes),
            Syscall::Write => {
                if a0 != 1 && a0 != 2 {
                    return self.ret(nr, sepc, neg(EBADF), notes);
                }
                let n = a2.min(WRITE_MAX);
                if n == 0 {
                    return self.ret(nr, sepc, 0, notes);
                }
                // A range past 2^32 is not a range of the address space; it would need the
                // top page, which no address space maps.
                let Some(buffer) = Buffer::new(sepc, a1, n) else {
                    return self.ret(nr, sepc, neg(EFAULT), notes);
                };
                Ok(ProcOp::new(Stage::Walk {
                    buffer,
                    pages: Vec::new(),
                    table: self.running_root()?,
                    level: 1,
                }))
            }
            Syscall::SchedYield => {
                let mut context = Context::from_frame(frame);
                context.regs[A0] = 0;
                context.pc = sepc.wrapping_add(4);
                notes.push(exit_note(pid, nr, 0));
                self.procs.yield_current(context)?;
                self.dispatch_next(pid, notes)
            }
            Syscall::Exit | Syscall::ExitGroup => {
                let status = a0 as i32;
                self.procs.end_current(ProcState::Exited { status })?;
                notes.push(note(
                    EXIT_KIND,
                    vec![
                        ("pid", Value::U64(u64::from(pid))),
                        ("status", Value::I64(i64::from(status))),
                    ],
                ));
                self.dispatch_next(pid, notes)
            }
        }
    }

    /// The first image from `from` on that can be created, reserving its frames; or, when
    /// none is left, the first dispatch.
    fn next_creation(
        &mut self,
        config: &KernelConfig,
        from: usize,
        notes: &mut Vec<Note>,
    ) -> Result<ProcOp, Violation> {
        let Source::Plan(plan) = &self.source else {
            return Err(Violation("a plan creation without a plan"));
        };
        for index in from..plan.images.len() {
            let pid = index as u32 + 1;
            let boot = &plan.images[index];
            if megapage_conflict(&boot.image, config) {
                notes.push(create_error(
                    pid,
                    boot.image.entry,
                    "a segment is in a megapage slot",
                ));
                continue;
            }
            let n = frames_needed(&boot.image, &plan.layout);
            let Some(frames) = self.procs.frames_mut().reserve(pid, n) else {
                notes.push(create_error(pid, boot.image.entry, "frame pool exhausted"));
                continue;
            };
            let space = Space::build(config, &plan.layout, boot, &frames);
            return Ok(ProcOp::new(Stage::Create { pid, space }));
        }
        self.dispatch_next(0, notes)
    }

    /// Admits the created process `pid` of `image`, built in `space`, and traces its
    /// segments and creation.
    fn admit(
        &mut self,
        pid: u32,
        image: &UserImage,
        space: &Space,
        notes: &mut Vec<Note>,
    ) -> Result<(), Violation> {
        let context = Context::initial(image.entry, self.source.layout().stack_top);
        self.procs.admit(Pcb::new(pid, space, context))?;
        for s in &image.segments {
            notes.push(note(
                SEGMENT_KIND,
                vec![
                    ("pid", Value::U64(u64::from(pid))),
                    ("va", Value::U64(u64::from(s.vaddr))),
                    ("memsz", Value::U64(u64::from(s.memsz))),
                    ("perms", Value::Str(perm_str(s.perms))),
                ],
            ));
        }
        notes.push(note(
            CREATE_KIND,
            vec![
                ("pid", Value::U64(u64::from(pid))),
                ("entry", Value::U64(u64::from(image.entry))),
                ("root", Value::U64(u64::from(space.root))),
                ("error", Value::Str(String::new())),
            ],
        ));
        Ok(())
    }

    /// Decides a failure shutdown with reason 1 and `detail`.
    fn fail(&mut self, detail: String, notes: &mut Vec<Note>) -> ProcOp {
        self.life = Life::Down;
        notes.push(shutdown_note(1, detail));
        ProcOp::new(Stage::Shutdown { reason: 1 })
    }

    /// Loads table entry `index` and the ones after it; or, when none is left, the first
    /// dispatch. The table is dropped with the last entry.
    fn next_load(
        &mut self,
        table: Option<ExecTable>,
        index: usize,
        notes: &mut Vec<Note>,
    ) -> Result<ProcOp, Violation> {
        match table {
            Some(table) if index < table.entries.len() => Ok(ProcOp::new(Stage::Command {
                cursor: Cursor {
                    table: Some(table),
                    index,
                },
            })),
            _ => self.dispatch_next(0, notes),
        }
    }

    /// The transfer of `cursor` is acknowledged with `error` (§8.2).
    fn acked(
        &mut self,
        cursor: Cursor,
        error: u8,
        notes: &mut Vec<Note>,
    ) -> Result<ProcOp, Violation> {
        if error == 0 {
            let len = cursor.first_read();
            return Ok(ProcOp::new(Stage::Staged { cursor, len }));
        }
        if cursor.table.is_none() {
            let detail = format!("block controller error {error} reading the executable table");
            return Ok(self.fail(detail, notes));
        }
        notes.push(create_error(
            cursor.pid(),
            0,
            &format!("block controller error {error}"),
        ));
        self.next_load(cursor.table, cursor.index + 1, notes)
    }

    /// The first `len` staging bytes of `cursor`'s transfer are `data`: block 0 or an
    /// executable's headers (§8.1, §8.3).
    fn staged(
        &mut self,
        config: &KernelConfig,
        cursor: Cursor,
        len: u32,
        data: Vec<u8>,
        notes: &mut Vec<Note>,
    ) -> Result<ProcOp, Violation> {
        let Source::Disk(disk) = self.source else {
            return Err(Violation("a disk read without a disk"));
        };
        let Some(entry) = cursor.entry() else {
            let block: [u8; BLOCK_SIZE] = data
                .try_into()
                .map_err(|_| Violation("block 0 read is not one block"))?;
            let staging = DiskBoot::staging_size(config);
            return match parse_exec_table(&block, disk.capacity_blocks, staging) {
                Err(e) => Ok(self.fail(format!("invalid executable table: {e}"), notes)),
                Ok(table) => {
                    self.entries = table.entries.len();
                    notes.push(note(
                        BOOT_KIND,
                        vec![("entries", Value::U64(self.entries as u64))],
                    ));
                    self.next_load(Some(table), 0, notes)
                }
            };
        };
        let pid = cursor.pid();
        let next = cursor.index + 1;
        let image = match parse_user_elf32(&data, entry.byte_len, disk.layout.image_range()) {
            Ok(image) => image,
            Err(UserElfError::PrefixTooShort { needed }) if needed > len => {
                return Ok(ProcOp::new(Stage::Staged {
                    cursor,
                    len: needed,
                }));
            }
            Err(e) => {
                notes.push(create_error(pid, 0, &e.to_string()));
                return self.next_load(cursor.table, next, notes);
            }
        };
        if megapage_conflict(&image, config) {
            notes.push(create_error(
                pid,
                image.entry,
                "a segment is in a megapage slot",
            ));
            return self.next_load(cursor.table, next, notes);
        }
        let n = frames_needed(&image, &disk.layout);
        let Some(frames) = self.procs.frames_mut().reserve(pid, n) else {
            notes.push(create_error(pid, image.entry, "frame pool exhausted"));
            return self.next_load(cursor.table, next, notes);
        };
        let boot = BootImage {
            staged: config.staging.base as u32,
            file_len: entry.byte_len,
            image,
        };
        let space = Space::build(config, &disk.layout, &boot, &frames);
        Ok(ProcOp::new(Stage::Load {
            cursor,
            headers: data,
            image: boot.image,
            space,
        }))
    }

    /// Dispatches the queue's head, or shuts down when the queue is empty (§6.6).
    fn dispatch_next(&mut self, from: u32, notes: &mut Vec<Note>) -> Result<ProcOp, Violation> {
        match self.procs.schedule_next()? {
            Some((pid, context)) => {
                let root = self.procs.pcb(pid).expect("scheduled").root;
                notes.push(note(
                    SWITCH_KIND,
                    vec![
                        ("from", Value::U64(u64::from(from))),
                        ("to", Value::U64(u64::from(pid))),
                    ],
                ));
                Ok(ProcOp::new(Stage::Dispatch {
                    pid,
                    context,
                    satp: pte::satp(root),
                }))
            }
            None => {
                let reason = self.empty_queue_reason();
                self.life = Life::Down;
                notes.push(shutdown_note(reason, "the run queue is empty".to_owned()));
                Ok(ProcOp::new(Stage::Shutdown { reason }))
            }
        }
    }

    /// The shutdown reason for an empty queue: 0 only when every image became a process
    /// and every process exited with status 0.
    pub fn empty_queue_reason(&self) -> u32 {
        let pcbs = self.procs.pcbs();
        let clean = pcbs.len() == self.entries
            && pcbs
                .iter()
                .all(|p| p.state == ProcState::Exited { status: 0 });
        u32::from(!clean)
    }

    /// The decision after a trap (§6.6, §6.7), from the frame bytes read.
    fn decide(&mut self, frame: &[u8], notes: &mut Vec<Note>) -> Result<ProcOp, Violation> {
        let sstatus = word(frame, FRAME_SSTATUS);
        let scause = word(frame, FRAME_SCAUSE);
        let sepc = word(frame, crate::config::FRAME_SEPC);
        let stval = word(frame, FRAME_STVAL);
        if sstatus & SSTATUS_SPP != 0 {
            self.life = Life::Down;
            notes.push(shutdown_note(
                1,
                format!("trap from S-mode: {}", cause_name(scause)),
            ));
            return Ok(ProcOp::new(Stage::Shutdown { reason: 1 }));
        }
        if scause == ECALL_FROM_U {
            return self.syscall(frame, notes);
        }
        let pid = self.procs.end_current(ProcState::Faulted {
            cause: scause,
            epc: sepc,
            tval: stval,
        })?;
        notes.push(note(
            FAULT_KIND,
            vec![
                ("pid", Value::U64(u64::from(pid))),
                ("cause", Value::Str(cause_name(scause))),
                ("epc", Value::U64(u64::from(sepc))),
                ("tval", Value::U64(u64::from(stval))),
            ],
        ));
        self.dispatch_next(pid, notes)
    }
}
