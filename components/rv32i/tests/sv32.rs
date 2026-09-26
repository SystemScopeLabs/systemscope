//! `sv32_translate`, the crate's synchronous Sv32 walk (`docs/m3-design.md` §5.2), against
//! the independent reference [`common::sv32_ref`]: the same result and the same PTE reads
//! for random page tables, modes, `SUM`, `MXR`, and accesses.

mod common;

use std::collections::BTreeMap;

use common::sv32_ref::{self, Kind, Outcome};
use proptest::prelude::*;
use systemscope_rv32i::Privilege;
use systemscope_rv32i::sv32::{Access, Fault, sv32_translate};

fn access(kind: Kind) -> Access {
    match kind {
        Kind::Fetch => Access::Fetch,
        Kind::Load => Access::Load,
        Kind::Store => Access::Store,
    }
}

fn privilege(mode: u8) -> Privilege {
    match mode {
        0 => Privilege::User,
        1 => Privilege::Supervisor,
        _ => Privilege::Machine,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// Random satp, PTEs placed where the walk will look, and random refusals.
    #[test]
    fn sv32_translate_matches_the_reference(
        satp in any::<u32>(),
        mode in prop_oneof![Just(0u8), Just(1), Just(3)],
        sum in any::<bool>(),
        mxr in any::<bool>(),
        kind in prop_oneof![Just(Kind::Fetch), Just(Kind::Load), Just(Kind::Store)],
        va in any::<u32>(),
        root_pte in any::<u32>(),
        leaf_pte in any::<u32>(),
        pointer in any::<bool>(),
        refuse in 0u8..8,
    ) {
        let satp = satp & !(0x1ff << 22);
        // A pointer at level 1 half the time, so level 0 is reached.
        let root_pte = if pointer { root_pte & !0xde } else { root_pte };
        let root_at = u64::from(satp & 0x3f_ffff) * 4096 + u64::from(va >> 22) * 4;
        let leaf_at = u64::from(root_pte >> 10) * 4096 + u64::from((va >> 12) & 0x3ff) * 4;
        let mut mem = BTreeMap::new();
        if refuse != 1 {
            mem.insert(root_at, root_pte);
        }
        if refuse != 2 && leaf_at != root_at {
            mem.insert(leaf_at, leaf_pte);
        }
        let read = |a: u64| mem.get(&a).copied();
        let (walk, expected) = sv32_ref::translate(satp, mode, sum, mxr, kind, va, read);
        let mut reads = Vec::new();
        let got = sv32_translate(satp, privilege(mode), sum, mxr, access(kind), va, |a| {
            reads.push(a);
            read(a)
        });
        let expected = match expected {
            Outcome::Pa(pa) => Ok(pa),
            Outcome::PageFault => Err(Fault::Page),
            Outcome::PteAccessFault => Err(Fault::Access),
        };
        prop_assert_eq!(got, expected);
        prop_assert_eq!(reads, walk);
    }
}
