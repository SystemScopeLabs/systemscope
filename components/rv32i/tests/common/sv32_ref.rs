//! An independent Sv32 reference translator (`docs/m3-design.md` §5.2), test-only.
//!
//! It is written from the privileged specification's "Virtual Address Translation
//! Process" as a numbered-step loop over `i` = LEVELS − 1 … 0, with §5.2's choices (Svade,
//! no hardware A/D update, reserved non-leaf `D`/`A`/`U`), and shares no code with the
//! crate's `sv32` module. It also returns every PTE address it read, in order, so tests can
//! check the CPU's bus traffic, not only its result.

/// Mode encodings.
pub const U: u8 = 0;
pub const S: u8 = 1;
pub const M: u8 = 3;

const PAGESIZE: u64 = 4096;
const LEVELS: usize = 2;
const PTESIZE: u64 = 4;

/// The kind of access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Fetch,
    Load,
    Store,
}

/// How a translation ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The physical address.
    Pa(u64),
    /// A page fault of the access's type.
    PageFault,
    /// A PTE read the memory refused: the access fault of the access's type.
    PteAccessFault,
}

impl Kind {
    /// The `mcause` code of the page fault.
    pub fn page_fault_code(self) -> u32 {
        match self {
            Kind::Fetch => 12,
            Kind::Load => 13,
            Kind::Store => 15,
        }
    }

    /// The `mcause` code of the access fault.
    pub fn access_fault_code(self) -> u32 {
        match self {
            Kind::Fetch => 1,
            Kind::Load => 5,
            Kind::Store => 7,
        }
    }
}

fn bit(pte: u32, n: u32) -> bool {
    (pte >> n) & 1 == 1
}

/// Translates `va`. `mem(addr)` returns the 32-bit little-endian word at `addr`, or `None`
/// where nothing answers. Returns the PTE addresses read and the outcome.
#[allow(clippy::too_many_arguments)]
pub fn translate(
    satp: u32,
    privilege: u8,
    sum: bool,
    mxr: bool,
    kind: Kind,
    va: u32,
    mem: impl Fn(u64) -> Option<u32>,
) -> (Vec<u64>, Outcome) {
    let mut reads = Vec::new();
    // Translation applies below M when satp.MODE = 1.
    if privilege == M || satp >> 31 == 0 {
        return (reads, Outcome::Pa(u64::from(va)));
    }
    let vpn = [(va >> 12) & 0x3ff, (va >> 22) & 0x3ff];
    // Step 1: a = satp.ppn × PAGESIZE, i = LEVELS − 1.
    let mut a = u64::from(satp & 0x003f_ffff) * PAGESIZE;
    let mut i = LEVELS - 1;
    let (pte, level) = loop {
        // Step 2: pte = the PTE at a + va.vpn[i] × PTESIZE; an access fault if refused.
        let addr = a + u64::from(vpn[i]) * PTESIZE;
        reads.push(addr);
        let Some(pte) = mem(addr) else {
            return (reads, Outcome::PteAccessFault);
        };
        let (v, r, w, x) = (bit(pte, 0), bit(pte, 1), bit(pte, 2), bit(pte, 3));
        // Step 3: invalid, or reserved W-without-R.
        if !v || (!r && w) {
            return (reads, Outcome::PageFault);
        }
        // Step 4: a leaf when R or X.
        if r || x {
            break (pte, i);
        }
        // A pointer: D, A, and U are reserved in a non-leaf PTE.
        if bit(pte, 7) || bit(pte, 6) || bit(pte, 4) {
            return (reads, Outcome::PageFault);
        }
        if i == 0 {
            return (reads, Outcome::PageFault);
        }
        i -= 1;
        a = u64::from(pte >> 10) * PAGESIZE;
    };
    // Step 5: the leaf's permissions.
    let (r, w, x, u, acc, d) = (
        bit(pte, 1),
        bit(pte, 2),
        bit(pte, 3),
        bit(pte, 4),
        bit(pte, 6),
        bit(pte, 7),
    );
    let mode_ok = if privilege == U {
        u
    } else if u {
        // S touching a U page: loads and stores need SUM; fetches never.
        sum && kind != Kind::Fetch
    } else {
        true
    };
    let perm_ok = match kind {
        Kind::Fetch => x,
        Kind::Load => r || (x && mxr),
        Kind::Store => w,
    };
    if !mode_ok || !perm_ok {
        return (reads, Outcome::PageFault);
    }
    let ppn = [(pte >> 10) & 0x3ff, pte >> 20];
    // Step 6: a misaligned superpage.
    if level > 0 && ppn[0] != 0 {
        return (reads, Outcome::PageFault);
    }
    // Step 7: Svade — A clear, or D clear on a store.
    if !acc || (kind == Kind::Store && !d) {
        return (reads, Outcome::PageFault);
    }
    // Step 8: pa.pgoff = va.pgoff; a superpage takes ppn[0] from va.vpn[0].
    let ppn0 = if level > 0 { vpn[0] } else { ppn[0] };
    let pa = u64::from(ppn[1]) << 22 | u64::from(ppn0) << 12 | u64::from(va & 0xfff);
    (reads, Outcome::Pa(pa))
}
