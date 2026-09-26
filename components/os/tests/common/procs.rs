//! Test support for the process model (`docs/m3-design.md` §6.4, §6.6, §8.3). Test-only.
//!
//! - [`elf32`] writes minimal ELF32 RISC-V executables from the gABI layout, so tests
//!   make user images with any segment shape and hand them to the real
//!   `parse_user_elf32`, as the kernel's callers do.
//! - [`PageMem`] is a paged byte memory the harness answers kernel accesses from.
//! - [`walk`] is an independent Sv32 walker written from the privileged specification. It
//!   shares no code with the kernel's PTE encoder or the CPU's walker: it is the oracle
//!   for what the page tables the kernel wrote mean.
//! - [`Harness`] drives a kernel with processes through `MockCtx`, one access at a time,
//!   and can fault any access.
//! - [`model`] is an independent model of frame reservation and the address-space
//!   layout: which frames boot should give each image, lowest free first.

#![allow(dead_code)]

use std::collections::BTreeMap;

use systemscope_contracts::event::Phase;
use systemscope_contracts::protocol::mem_v1::{MemFault, MemMsg, ReadOutcome, TxnId, WriteOutcome};
use systemscope_contracts::time::ClockDomainId;
use systemscope_contracts::trace::Value;
use systemscope_elf::parse_user_elf32;
use systemscope_os::kernel::{GATE_PORT, ISSUE, MEM_PORT};
use systemscope_os::{BootImage, KernelConfig, ModeledKernel, ProcessPlan, UserLayout};

use super::layout::*;
use super::{MockCtx, Traced};

/// `p_flags` bits.
pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;

/// One `PT_LOAD`: `(vaddr, flags, file bytes, memsz)`.
#[derive(Clone, Debug)]
pub struct Seg {
    pub vaddr: u32,
    pub flags: u32,
    pub data: Vec<u8>,
    pub memsz: u32,
}

impl Seg {
    pub fn new(vaddr: u32, flags: u32, data: Vec<u8>, memsz: u32) -> Seg {
        Seg {
            vaddr,
            flags,
            data,
            memsz,
        }
    }
}

/// An ELF32 little-endian RISC-V `ET_EXEC` with `segs` as `PT_LOAD`s in order, their
/// bytes after the program-header table, each 4-byte aligned.
pub fn elf32(entry: u32, segs: &[Seg]) -> Vec<u8> {
    let phoff = 52u32;
    let mut offset = phoff + 32 * segs.len() as u32;
    let mut out = Vec::new();
    out.extend_from_slice(b"\x7fELF");
    out.extend_from_slice(&[1, 1, 1, 0]);
    out.extend_from_slice(&[0; 8]);
    let h16 = |o: &mut Vec<u8>, v: u16| o.extend_from_slice(&v.to_le_bytes());
    let h32 = |o: &mut Vec<u8>, v: u32| o.extend_from_slice(&v.to_le_bytes());
    h16(&mut out, 2); // ET_EXEC
    h16(&mut out, 243); // EM_RISCV
    h32(&mut out, 1);
    h32(&mut out, entry);
    h32(&mut out, phoff);
    h32(&mut out, 0); // e_shoff
    h32(&mut out, 0); // e_flags
    h16(&mut out, 52);
    h16(&mut out, 32);
    h16(&mut out, segs.len() as u16);
    h16(&mut out, 0);
    h16(&mut out, 0);
    h16(&mut out, 0);
    let mut offsets = Vec::new();
    for s in segs {
        offset = offset.next_multiple_of(4);
        offsets.push(offset);
        offset += s.data.len() as u32;
    }
    for (s, &off) in segs.iter().zip(&offsets) {
        for v in [
            1,
            off,
            s.vaddr,
            s.vaddr,
            s.data.len() as u32,
            s.memsz,
            s.flags,
            0x1000,
        ] {
            h32(&mut out, v);
        }
    }
    for (s, &off) in segs.iter().zip(&offsets) {
        out.resize(off as usize, 0);
        out.extend_from_slice(&s.data);
    }
    out
}

/// The boot image of `bytes` staged at `staged`, validated by `parse_user_elf32` with
/// the M3 layout's image range.
pub fn boot_image(bytes: &[u8], staged: u32) -> BootImage {
    boot_image_in(bytes, staged, &UserLayout::M3)
}

/// [`boot_image`] with another layout.
pub fn boot_image_in(bytes: &[u8], staged: u32, layout: &UserLayout) -> BootImage {
    let image = parse_user_elf32(bytes, bytes.len() as u32, layout.image_range()).unwrap();
    BootImage {
        staged,
        file_len: bytes.len() as u32,
        image,
    }
}

/// Words as little-endian bytes.
pub fn words(ws: &[u32]) -> Vec<u8> {
    ws.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// A two-segment program: `text` R+X at `0x0001_0000` (entry there) and `data` R+W at
/// `0x0001_1000` with `bss` more zero bytes.
pub fn two_segment(text: &[u32], data: &[u8], bss: u32) -> Vec<u8> {
    elf32(
        0x0001_0000,
        &[
            Seg::new(0x0001_0000, PF_R | PF_X, words(text), 4 * text.len() as u32),
            Seg::new(
                0x0001_1000,
                PF_R | PF_W,
                data.to_vec(),
                data.len() as u32 + bss,
            ),
        ],
    )
}

/// Staging addresses: image `i` at `STAGING + i · 64 KiB`.
pub fn staged_at(i: usize) -> u32 {
    STAGING as u32 + (i as u32) * 0x1_0000
}

/// The plan of `files`, each staged at [`staged_at`], with the M3 layout.
pub fn plan_of(files: &[Vec<u8>]) -> ProcessPlan {
    plan_in(files, UserLayout::M3)
}

/// [`plan_of`] with another layout.
pub fn plan_in(files: &[Vec<u8>], layout: UserLayout) -> ProcessPlan {
    ProcessPlan {
        layout,
        images: files
            .iter()
            .enumerate()
            .map(|(i, f)| boot_image_in(f, staged_at(i), &layout))
            .collect(),
    }
}

/// A paged byte memory; unwritten bytes are 0.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PageMem {
    pages: BTreeMap<u64, Box<[u8; 4096]>>,
}

impl PageMem {
    pub fn read(&self, addr: u64, len: usize) -> Vec<u8> {
        (addr..addr + len as u64)
            .map(|a| {
                self.pages
                    .get(&(a / 4096))
                    .map_or(0, |p| p[(a % 4096) as usize])
            })
            .collect()
    }

    pub fn write(&mut self, addr: u64, bytes: &[u8]) {
        for (a, &b) in (addr..).zip(bytes) {
            self.pages
                .entry(a / 4096)
                .or_insert_with(|| Box::new([0; 4096]))[(a % 4096) as usize] = b;
        }
    }

    pub fn word(&self, addr: u64) -> u32 {
        u32::from_le_bytes(self.read(addr, 4).try_into().unwrap())
    }

    pub fn set_word(&mut self, addr: u64, value: u32) {
        self.write(addr, &value.to_le_bytes());
    }

    /// Every file of `plan` at its staging address.
    pub fn stage(&mut self, files: &[Vec<u8>], plan: &ProcessPlan) {
        for (f, b) in files.iter().zip(&plan.images) {
            self.write(u64::from(b.staged), f);
        }
    }
}

/// An access check for [`walk`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Fetch,
    Load,
    Store,
}

/// The independent Sv32 walk (privileged spec, "Virtual Address Translation Process")
/// of `va` for a U-mode access with `SUM = MXR = 0`, over `mem`, from the root table at
/// `root_ppn`. Returns the physical address, or `None` for a page fault. A/D must already
/// be set (the M3 CPU never sets them, §5.2).
pub fn walk(mem: &PageMem, root_ppn: u32, va: u32, kind: Kind) -> Option<u64> {
    let vpn = [(va >> 12) & 0x3FF, (va >> 22) & 0x3FF];
    let mut table = u64::from(root_ppn) << 12;
    for level in [1usize, 0] {
        let pte = mem.word(table + 4 * u64::from(vpn[level]));
        let (v, r, w, x, u, a, d) = (
            pte & 1,
            pte >> 1 & 1,
            pte >> 2 & 1,
            pte >> 3 & 1,
            pte >> 4 & 1,
            pte >> 6 & 1,
            pte >> 7 & 1,
        );
        if v == 0 || (r == 0 && w == 1) {
            return None;
        }
        let ppn = pte >> 10;
        if r == 0 && x == 0 {
            if level == 0 {
                return None;
            }
            table = u64::from(ppn) << 12;
            continue;
        }
        let allowed = match kind {
            Kind::Fetch => x == 1,
            Kind::Load => r == 1,
            Kind::Store => w == 1,
        };
        if u == 0 || !allowed || a == 0 || (kind == Kind::Store && d == 0) {
            return None;
        }
        if level == 1 {
            if ppn & 0x3FF != 0 {
                return None;
            }
            return Some((u64::from(ppn >> 10) << 22) | u64::from(va & 0x3F_FFFF));
        }
        return Some((u64::from(ppn) << 12) | u64::from(va & 0xFFF));
    }
    None
}

/// The level-1 entry for `va` in the root table at `root_ppn` (an S-mode view, for the
/// megapages).
pub fn l1_entry(mem: &PageMem, root_ppn: u32, va: u32) -> u32 {
    mem.word((u64::from(root_ppn) << 12) + 4 * u64::from(va >> 22))
}

/// The downstream txn the bus gave every `ENTER` in harness tests.
pub const HELD: TxnId = TxnId(500);
pub const CLOCK: ClockDomainId = ClockDomainId(3);

pub fn config() -> KernelConfig {
    super::layout::config(CLOCK)
}

/// A pool of `frames` frames at the layout's pool base.
pub fn config_with_pool(frames: u64) -> KernelConfig {
    let mut c = config();
    c.frame_pool.size = frames * 4096;
    c
}

/// A kernel access as the harness saw it: `(write, addr, bytes)`, a read's bytes as
/// read.
pub type Seen = (bool, u64, Vec<u8>);

/// Which access to fault, by its index in the run's access list, counted from 0.
pub type Inject = Option<usize>;

/// A kernel with processes, a memory, and a `MockCtx`, stepped one handler at a time.
pub struct Harness {
    pub k: ModeledKernel,
    pub ctx: MockCtx,
    pub mem: PageMem,
    pub next_txn: u64,
    pub seen: Vec<Seen>,
    /// The request sent and not yet answered.
    pub pending: Option<MemMsg>,
}

/// Where an operation stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The held `ENTER` was released.
    Released,
    /// The session faulted with the message.
    Fault(&'static str),
}

impl Harness {
    /// A harness for `files` with `plan`, the files staged.
    pub fn new(config: KernelConfig, files: &[Vec<u8>], plan: ProcessPlan) -> Harness {
        let mut mem = PageMem::default();
        mem.stage(files, &plan);
        Harness {
            k: ModeledKernel::with_processes(config, plan).unwrap(),
            ctx: MockCtx::new(),
            mem,
            next_txn: 0,
            seen: Vec::new(),
            pending: None,
        }
    }

    /// Delivers `ENTER` of `value`.
    pub fn enter(&mut self, value: u32) -> Result<(), &'static str> {
        let msg = MemMsg::WriteReq {
            txn: HELD,
            addr: 0,
            data: value.to_le_bytes().to_vec(),
        };
        self.ctx
            .deliver(&mut self.k, GATE_PORT, msg, Phase::Request)
            .map_err(fault_text)?;
        assert!(
            self.ctx.take_sent().is_empty(),
            "ENTER is never answered at once"
        );
        assert_eq!(self.ctx.take_woke().len(), 1);
        Ok(())
    }

    /// Delivers the wake, requiring one request with the next txn.
    pub fn issue(&mut self) -> Result<MemMsg, &'static str> {
        self.ctx
            .wake(&mut self.k, ISSUE, Phase::Request)
            .map_err(fault_text)?;
        let sent = self.ctx.take_sent();
        assert_eq!(sent.len(), 1, "one request per wake");
        assert!(self.ctx.take_woke().is_empty());
        assert_eq!(sent[0].port, MEM_PORT);
        let msg = sent[0].msg.clone();
        let txn = match &msg {
            MemMsg::ReadReq { txn, .. } | MemMsg::WriteReq { txn, .. } => txn.0,
            other => panic!("{other:?}"),
        };
        assert_eq!(txn, self.next_txn, "fresh, consecutive kernel txns");
        self.next_txn += 1;
        self.pending = Some(msg.clone());
        Ok(msg)
    }

    /// Answers the pending request from memory (or with a fault), returning whether the
    /// held entry was released.
    pub fn respond(&mut self, fault: bool) -> Result<bool, &'static str> {
        let req = self.pending.take().expect("a request is pending");
        let resp = match &req {
            MemMsg::ReadReq { txn, addr, len } => {
                let data = self.mem.read(*addr, *len as usize);
                self.seen.push((false, *addr, data.clone()));
                MemMsg::ReadResp {
                    txn: *txn,
                    outcome: if fault {
                        ReadOutcome::Fault {
                            fault: MemFault::AccessFault,
                        }
                    } else {
                        ReadOutcome::Data { data }
                    },
                }
            }
            MemMsg::WriteReq { txn, addr, data } => {
                self.seen.push((true, *addr, data.clone()));
                if !fault {
                    self.mem.write(*addr, data);
                }
                MemMsg::WriteResp {
                    txn: *txn,
                    outcome: if fault {
                        WriteOutcome::Fault {
                            fault: MemFault::AccessFault,
                        }
                    } else {
                        WriteOutcome::Done
                    },
                }
            }
            other => panic!("{other:?}"),
        };
        self.ctx
            .deliver(&mut self.k, MEM_PORT, resp, Phase::Complete)
            .map_err(fault_text)?;
        let sent = self.ctx.take_sent();
        let woke = self.ctx.take_woke();
        if sent.is_empty() {
            assert_eq!(woke.len(), 1, "the next access is due next cycle");
            Ok(false)
        } else {
            assert!(woke.is_empty());
            assert_eq!(sent.len(), 1, "one release");
            assert_eq!(sent[0].port, GATE_PORT);
            assert_eq!(
                sent[0].msg,
                MemMsg::WriteResp {
                    txn: HELD,
                    outcome: WriteOutcome::Done
                }
            );
            assert_eq!(self.k.held(), None);
            Ok(true)
        }
    }

    /// Runs an operation from `ENTER` of `value` to its release, faulting the access
    /// with index `inject` among this operation's accesses.
    pub fn op(&mut self, value: u32, inject: Inject) -> Stop {
        if let Err(e) = self.enter(value) {
            return Stop::Fault(e);
        }
        for i in 0.. {
            if let Err(e) = self.issue() {
                return Stop::Fault(e);
            }
            match self.respond(inject == Some(i)) {
                Err(e) => return Stop::Fault(e),
                Ok(true) => return Stop::Released,
                Ok(false) => {}
            }
        }
        unreachable!()
    }

    /// Boots: the first `ENTER`, to its release.
    pub fn boot(&mut self) {
        assert_eq!(self.op(TRAP_FRAME, None), Stop::Released);
    }

    /// Writes a trap frame for a trap with `scause`, `sepc`, `stval`, and `sstatus`, and
    /// `x_i = 0x100 · i + tag` in every register, then runs the `Trap` operation.
    pub fn trap(&mut self, scause: u32, sepc: u32, stval: u32, sstatus: u32, tag: u32) -> Stop {
        self.write_frame(scause, sepc, stval, sstatus, tag);
        self.op(TRAP_FRAME, None)
    }

    /// Writes the trap frame [`Harness::trap`] runs.
    pub fn write_frame(&mut self, scause: u32, sepc: u32, stval: u32, sstatus: u32, tag: u32) {
        let f = u64::from(TRAP_FRAME);
        for i in 1..=31u32 {
            self.mem.set_word(f + 4 * u64::from(i - 1), 0x100 * i + tag);
        }
        self.mem.set_word(f + 0x7C, sepc);
        self.mem.set_word(f + 0x80, sstatus);
        self.mem.set_word(f + 0x84, scause);
        self.mem.set_word(f + 0x88, stval);
    }

    /// Writes the frame of an `ecall` from U at `sepc` with `a7 = nr` and `a0`–`a2` =
    /// `args`, every other register as [`Harness::trap`] writes it.
    pub fn write_syscall(&mut self, nr: u32, args: [u32; 3], sepc: u32, sstatus: u32, tag: u32) {
        self.write_frame(8, sepc, 0, sstatus, tag);
        let f = u64::from(TRAP_FRAME);
        for (i, a) in args.into_iter().enumerate() {
            self.mem.set_word(f + 0x24 + 4 * i as u64, a);
        }
        self.mem.set_word(f + 0x40, nr);
    }

    /// Runs syscall `nr` with `args` from `sepc` (see [`Harness::write_syscall`]).
    pub fn syscall(&mut self, nr: u32, args: [u32; 3], sepc: u32, tag: u32) -> Stop {
        self.write_syscall(nr, args, sepc, 0, tag);
        self.op(TRAP_FRAME, None)
    }

    /// Runs `sched_yield` from `sepc` with `sstatus`.
    pub fn sys_yield(&mut self, sepc: u32, sstatus: u32, tag: u32) -> Stop {
        self.write_syscall(
            124,
            [0x0A00 + tag, 0x0B00 + tag, 0x0C00 + tag],
            sepc,
            sstatus,
            tag,
        );
        self.op(TRAP_FRAME, None)
    }

    /// The trap frame's word at `offset`.
    pub fn frame(&self, offset: u64) -> u32 {
        self.mem.word(u64::from(TRAP_FRAME) + offset)
    }

    /// The traces of `kind`, in order, since the harness started.
    pub fn traced(&self, kind: &str) -> Vec<&Traced> {
        self.ctx.traced.iter().filter(|t| t.0 == kind).collect()
    }
}

/// The message of a session fault.
pub fn fault_text(e: systemscope_contracts::error::SimError) -> &'static str {
    match e {
        systemscope_contracts::error::SimError::ComponentFault(m) => m,
        other => panic!("{other:?}"),
    }
}

/// A trace record's field.
pub fn tfield<'a>(t: &'a Traced, name: &str) -> &'a Value {
    &t.1.iter().find(|(k, _)| *k == name).unwrap().1
}

pub fn tu(t: &Traced, name: &str) -> u64 {
    match tfield(t, name) {
        Value::U64(v) => *v,
        other => panic!("{other:?}"),
    }
}

pub fn ts(t: &Traced, name: &str) -> String {
    match tfield(t, name) {
        Value::Str(v) => v.clone(),
        other => panic!("{other:?}"),
    }
}

/// An independent model of frame reservation (§6.4) and the address-space shape.
pub mod model {
    use systemscope_os::{BootImage, UserLayout};

    /// A user page: `(va, r, w, x, file bytes as (file offset, page offset, len))`.
    pub type Page = (u32, bool, bool, bool, Option<(u32, u32, u32)>);

    /// Every user page of `image` in mapping order (segments, then stack), as
    /// `(va, r, w, x, file bytes as (file offset, page offset, len))`.
    pub fn pages(b: &BootImage, layout: &UserLayout) -> Vec<Page> {
        let mut out = Vec::new();
        for s in &b.image.segments {
            for p in &s.pages {
                out.push((
                    p.va,
                    p.perms.read,
                    p.perms.write,
                    p.perms.execute,
                    p.copy.map(|c| (c.file_offset, c.page_offset, c.len)),
                ));
            }
        }
        let size = 4096 * layout.stack_pages;
        for i in 0..layout.stack_pages {
            out.push((layout.stack_top - size + 4096 * i, true, true, false, None));
        }
        out
    }

    /// How many frames `b` needs: a root, one table per distinct 4 MiB slot, a frame per
    /// page.
    pub fn needed(b: &BootImage, layout: &UserLayout) -> usize {
        let pages = pages(b, layout);
        let mut slots: Vec<u32> = pages.iter().map(|p| p.0 >> 22).collect();
        slots.sort();
        slots.dedup();
        1 + slots.len() + pages.len()
    }

    /// The expected owner of every pool frame after boot creates `images` from an empty
    /// pool of `pool` frames: image `i` gets PID `i + 1` and the lowest free frames, or
    /// nothing if too few are free. Index `f` is pool frame `f`; 0 means free.
    pub fn boot_owners(images: &[BootImage], layout: &UserLayout, pool: usize) -> Vec<u32> {
        let mut owners = vec![0u32; pool];
        for (i, b) in images.iter().enumerate() {
            let n = needed(b, layout);
            let free: Vec<usize> = (0..pool).filter(|&f| owners[f] == 0).take(n).collect();
            if free.len() == n {
                for f in free {
                    owners[f] = i as u32 + 1;
                }
            }
        }
        owners
    }
}

/// An independent codec of the kernel's process-mode snapshot, written from the schema
/// in `ModeledKernel::snapshot`'s documentation, for building and mutating snapshots.
pub mod ksnap {
    use systemscope_contracts::canonical::{DecodeError, Decoder, Encoder};

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Stage {
        Create {
            pid: u32,
            frames: Vec<u32>,
        },
        ReadFrame,
        Dispatch {
            pid: u32,
            context: Vec<u32>,
        },
        Shutdown {
            reason: u32,
        },
        /// `buffer` is `(sepc, buf, n)`.
        Walk {
            buffer: (u32, u32, u32),
            pages: Vec<u32>,
            table: u32,
            level: u8,
        },
        Output {
            buffer: (u32, u32, u32),
            pages: Vec<u32>,
            done: u32,
        },
        Return {
            sepc: u32,
            value: u32,
        },
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Op {
        pub stage: Stage,
        pub step: u32,
        pub data: Vec<u8>,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Pcb {
        pub pid: u32,
        /// The state tag and its words.
        pub state: (u8, Vec<u32>),
        pub context: Option<Vec<u32>>,
        pub root: u32,
        pub tables: Vec<u32>,
        pub regions: Vec<(u32, u8, Vec<u32>)>,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Snap {
        /// The configuration and the plan, unparsed.
        pub prefix: Vec<u8>,
        pub state: u8,
        pub wait_txn: u64,
        pub op: Option<Op>,
        pub held: Option<u64>,
        pub next_txn: u64,
        pub life: u8,
        pub pcbs: Vec<Pcb>,
        pub queue: Vec<u32>,
        pub current: Option<u32>,
        pub bitmap: Vec<u8>,
    }

    fn words(d: &mut Decoder<'_>) -> Result<Vec<u32>, DecodeError> {
        let n = d.len()?;
        (0..n).map(|_| d.u32()).collect()
    }

    fn put_words(e: &mut Encoder, ws: &[u32]) {
        e.len(ws.len());
        for &w in ws {
            e.u32(w);
        }
    }

    fn put_buffer(e: &mut Encoder, (sepc, buf, n): (u32, u32, u32)) {
        e.u32(sepc);
        e.u32(buf);
        e.u32(n);
    }

    impl Snap {
        /// Decodes `bytes`, whose first `prefix` bytes are the configuration and plan.
        pub fn decode(bytes: &[u8], prefix: usize) -> Result<Snap, DecodeError> {
            let mut d = Decoder::new(bytes);
            let pre = d.raw(prefix)?.to_vec();
            let state = d.u8()?;
            let wait_txn = if state == 2 { d.u64()? } else { 0 };
            let op = if state == 0 {
                None
            } else {
                assert_eq!(d.u8()?, 2, "a process-mode operation");
                let stage = match d.u8()? {
                    0 => Stage::Create {
                        pid: d.u32()?,
                        frames: words(&mut d)?,
                    },
                    1 => Stage::ReadFrame,
                    2 => Stage::Dispatch {
                        pid: d.u32()?,
                        context: (0..33).map(|_| d.u32()).collect::<Result<_, _>>()?,
                    },
                    3 => Stage::Shutdown { reason: d.u32()? },
                    4 => Stage::Walk {
                        buffer: (d.u32()?, d.u32()?, d.u32()?),
                        pages: words(&mut d)?,
                        table: d.u32()?,
                        level: d.u8()?,
                    },
                    5 => Stage::Output {
                        buffer: (d.u32()?, d.u32()?, d.u32()?),
                        pages: words(&mut d)?,
                        done: d.u32()?,
                    },
                    6 => Stage::Return {
                        sepc: d.u32()?,
                        value: d.u32()?,
                    },
                    t => panic!("stage {t}"),
                };
                Some(Op {
                    stage,
                    step: d.u32()?,
                    data: d.bytes()?.to_vec(),
                })
            };
            let held = if d.u8()? == 1 { Some(d.u64()?) } else { None };
            let next_txn = d.u64()?;
            let life = d.u8()?;
            let mut pcbs = Vec::new();
            for _ in 0..d.len()? {
                let pid = d.u32()?;
                let tag = d.u8()?;
                let n = [0, 0, 1, 3][tag as usize];
                let fields = (0..n).map(|_| d.u32()).collect::<Result<_, _>>()?;
                let context = if d.u8()? == 1 {
                    Some((0..33).map(|_| d.u32()).collect::<Result<_, _>>()?)
                } else {
                    None
                };
                let root = d.u32()?;
                let tables = words(&mut d)?;
                let mut regions = Vec::new();
                for _ in 0..d.len()? {
                    let va = d.u32()?;
                    let perms = d.u8()?;
                    regions.push((va, perms, words(&mut d)?));
                }
                pcbs.push(Pcb {
                    pid,
                    state: (tag, fields),
                    context,
                    root,
                    tables,
                    regions,
                });
            }
            let queue = words(&mut d)?;
            let current = if d.u8()? == 1 { Some(d.u32()?) } else { None };
            let bitmap = d.bytes()?.to_vec();
            d.finish()?;
            Ok(Snap {
                prefix: pre,
                state,
                wait_txn,
                op,
                held,
                next_txn,
                life,
                pcbs,
                queue,
                current,
                bitmap,
            })
        }

        pub fn encode(&self) -> Vec<u8> {
            let mut e = Encoder::new();
            e.raw(&self.prefix);
            e.u8(self.state);
            if self.state == 2 {
                e.u64(self.wait_txn);
            }
            if let Some(op) = &self.op {
                e.u8(2);
                match &op.stage {
                    Stage::Create { pid, frames } => {
                        e.u8(0);
                        e.u32(*pid);
                        put_words(&mut e, frames);
                    }
                    Stage::ReadFrame => e.u8(1),
                    Stage::Dispatch { pid, context } => {
                        e.u8(2);
                        e.u32(*pid);
                        for &w in context {
                            e.u32(w);
                        }
                    }
                    Stage::Shutdown { reason } => {
                        e.u8(3);
                        e.u32(*reason);
                    }
                    Stage::Walk {
                        buffer,
                        pages,
                        table,
                        level,
                    } => {
                        e.u8(4);
                        put_buffer(&mut e, *buffer);
                        put_words(&mut e, pages);
                        e.u32(*table);
                        e.u8(*level);
                    }
                    Stage::Output {
                        buffer,
                        pages,
                        done,
                    } => {
                        e.u8(5);
                        put_buffer(&mut e, *buffer);
                        put_words(&mut e, pages);
                        e.u32(*done);
                    }
                    Stage::Return { sepc, value } => {
                        e.u8(6);
                        e.u32(*sepc);
                        e.u32(*value);
                    }
                }
                e.u32(op.step);
                e.bytes(&op.data);
            }
            match self.held {
                None => e.u8(0),
                Some(t) => {
                    e.u8(1);
                    e.u64(t);
                }
            }
            e.u64(self.next_txn);
            e.u8(self.life);
            e.len(self.pcbs.len());
            for p in &self.pcbs {
                e.u32(p.pid);
                e.u8(p.state.0);
                for &w in &p.state.1 {
                    e.u32(w);
                }
                match &p.context {
                    None => e.u8(0),
                    Some(c) => {
                        e.u8(1);
                        for &w in c {
                            e.u32(w);
                        }
                    }
                }
                e.u32(p.root);
                put_words(&mut e, &p.tables);
                e.len(p.regions.len());
                for (va, perms, frames) in &p.regions {
                    e.u32(*va);
                    e.u8(*perms);
                    put_words(&mut e, frames);
                }
            }
            put_words(&mut e, &self.queue);
            match self.current {
                None => e.u8(0),
                Some(p) => {
                    e.u8(1);
                    e.u32(p);
                }
            }
            e.bytes(&self.bitmap);
            e.into_bytes()
        }
    }

    /// The prefix length of a kernel whose pool has `frames` frames, from the snapshot of
    /// it before boot, `fresh`: what follows the prefix then is `Idle`, nothing held, the
    /// txn counter, the life, no PCBs, an empty queue, no running PID, and the bitmap.
    pub fn prefix_len(fresh: &[u8], frames: usize) -> usize {
        fresh.len() - (1 + 1 + 8 + 1 + 4 + 4 + 1 + 4 + frames.div_ceil(8))
    }
}
