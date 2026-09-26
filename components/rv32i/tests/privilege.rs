//! The M3 privileged subset, pure (`docs/m3-design.md` §5.1, §5.3): modes, the whitelist
//! and the access rule, each supervisor CSR's rule, `satp` Bare-only, `SRET`, `SFENCE.VMA`,
//! `MRET` with modes, and delegated exception entry, against expectations written from the
//! design that share no code with the crate.

use proptest::prelude::*;
use systemscope_rv32i::privilege::{
    self, SupervisorInstr, accessible, decode_supervisor, is_supported, legal_mpp, satp_reachable,
    satp_write,
};
use systemscope_rv32i::{CsrFile, M3State, Privilege, Spp, TrapCause};

/// §5.1's whitelist.
const WHITELIST: [u16; 19] = [
    0x300, 0x304, 0x344, 0x305, 0x340, 0x341, 0x342, 0x343, 0x100, 0x104, 0x144, 0x105, 0x140,
    0x141, 0x142, 0x143, 0x180, 0x302, 0x303,
];

fn modes() -> impl Strategy<Value = Privilege> {
    prop::sample::select(vec![
        Privilege::User,
        Privilege::Supervisor,
        Privilege::Machine,
    ])
}

fn code(p: Privilege) -> u16 {
    match p {
        Privilege::User => 0,
        Privilege::Supervisor => 1,
        Privilege::Machine => 3,
    }
}

#[test]
fn privilege_and_spp_encode_only_supported_modes() {
    for (bits, mode) in [
        (0, Some(Privilege::User)),
        (1, Some(Privilege::Supervisor)),
        (2, None),
        (3, Some(Privilege::Machine)),
        (4, None),
        (0xff, None),
    ] {
        assert_eq!(Privilege::from_bits(bits), mode, "{bits}");
        if let Some(m) = mode {
            assert_eq!(m.bits(), bits);
        }
    }
    assert!(Privilege::User < Privilege::Supervisor && Privilege::Supervisor < Privilege::Machine);
    assert_eq!(Spp::from_bits(0), Some(Spp::User));
    assert_eq!(Spp::from_bits(1), Some(Spp::Supervisor));
    assert_eq!(Spp::from_bits(2), None);
    assert_eq!(Spp::Supervisor.privilege(), Privilege::Supervisor);
    assert_eq!(Spp::User.privilege(), Privilege::User);
}

#[test]
fn mpp_writes_are_warl_with_the_reserved_encoding_stored_as_u() {
    assert_eq!(legal_mpp(0), Privilege::User);
    assert_eq!(legal_mpp(1), Privilege::Supervisor);
    assert_eq!(legal_mpp(2), Privilege::User);
    assert_eq!(legal_mpp(3), Privilege::Machine);
    // Only the low two bits matter.
    assert_eq!(legal_mpp(0b110), Privilege::User);
    assert_eq!(legal_mpp(0b111), Privilege::Machine);
}

#[test]
fn the_whitelist_is_exactly_the_design_s() {
    assert_eq!(privilege::SUPPORTED, WHITELIST);
    for csr in 0..4096u16 {
        assert_eq!(is_supported(csr), WHITELIST.contains(&csr), "{csr:#05x}");
    }
}

#[test]
fn the_access_rule_is_the_number_s_mode_and_read_only_bits() {
    for csr in 0..4096u16 {
        let lowest = csr >> 8 & 3;
        let read_only = csr >> 10 == 3;
        for mode in [Privilege::User, Privilege::Supervisor, Privilege::Machine] {
            for writes in [false, true] {
                let want = code(mode) >= lowest && !(writes && read_only);
                assert_eq!(
                    accessible(csr, mode, writes),
                    want,
                    "{csr:#05x} {mode:?} {writes}"
                );
            }
        }
    }
    // Spot checks from the design's table.
    assert!(!accessible(0x300, Privilege::Supervisor, false));
    assert!(accessible(0x100, Privilege::Supervisor, true));
    assert!(!accessible(0x100, Privilege::User, false));
    assert!(!accessible(0xf14, Privilege::Machine, true));
    assert!(accessible(0xf14, Privilege::Machine, false));
    assert!(!accessible(0xc00, Privilege::Machine, true));
    // Reserved level 2 (hypervisor) numbers are M-only.
    assert!(!accessible(0x200, Privilege::Supervisor, false));
    assert!(accessible(0x200, Privilege::Machine, false));
}

#[test]
fn sret_and_sfence_vma_decode_and_wfi_does_not() {
    assert_eq!(decode_supervisor(0x1020_0073), Some(SupervisorInstr::Sret));
    for rs1 in [0u32, 1, 31] {
        for rs2 in [0u32, 5, 31] {
            let word = 0x1200_0073 | rs2 << 20 | rs1 << 15;
            assert_eq!(decode_supervisor(word), Some(SupervisorInstr::SfenceVma));
        }
    }
    for word in [
        0x1050_0073u32, // WFI
        0x3020_0073,    // MRET
        0x0020_0073,    // URET
        0x1020_00f3,    // SRET with rd
        0x1020_8073,    // SRET with rs1
        0x1200_00f3,    // SFENCE.VMA with rd
        0x1200_1073,    // SFENCE.VMA with funct3
        0x2200_0073,    // HFENCE.VVMA
        0x1600_0073,    // SINVAL.VMA
        0x0000_0073,    // ECALL
    ] {
        assert_eq!(decode_supervisor(word), None, "{word:#010x}");
    }
}

#[test]
fn reset_state_is_m_with_mpp_u_and_everything_zero() {
    let m = CsrFile::new();
    let s = M3State::new();
    assert_eq!(s.privilege, Privilege::Machine);
    assert_eq!(s.mpp, Privilege::User);
    for csr in WHITELIST {
        assert_eq!(s.read(&m, csr), Some(0), "{csr:#05x}");
    }
    assert_eq!(M3State::default(), s);
}

#[test]
fn delegation_needs_a_mode_below_m_and_the_cause_s_bit() {
    let mut s = M3State::new();
    s.medeleg = 1 << 2 | 1 << 8;
    assert!(
        !s.delegates(TrapCause::IllegalInstruction),
        "M never delegates"
    );
    s.privilege = Privilege::Supervisor;
    assert!(s.delegates(TrapCause::IllegalInstruction));
    assert!(!s.delegates(TrapCause::Breakpoint));
    s.privilege = Privilege::User;
    assert!(s.delegates(TrapCause::EnvironmentCallFromU));
    let before = s;
    assert_eq!(s.take_exception(TrapCause::Breakpoint, 0x100, 0x100), None);
    assert_eq!(s, before);
}

#[test]
fn the_cause_codes_are_the_architectural_ones() {
    for (cause, code) in [
        (TrapCause::InstructionAddressMisaligned, 0),
        (TrapCause::InstructionAccessFault, 1),
        (TrapCause::IllegalInstruction, 2),
        (TrapCause::Breakpoint, 3),
        (TrapCause::LoadAddressMisaligned, 4),
        (TrapCause::LoadAccessFault, 5),
        (TrapCause::StoreAddressMisaligned, 6),
        (TrapCause::StoreAccessFault, 7),
        (TrapCause::EnvironmentCallFromU, 8),
        (TrapCause::EnvironmentCallFromS, 9),
        (TrapCause::EnvironmentCall, 11),
        (TrapCause::InstructionPageFault, 12),
        (TrapCause::LoadPageFault, 13),
        (TrapCause::StorePageFault, 15),
    ] {
        assert_eq!(cause.code(), code, "{cause:?}");
        assert_eq!(privilege::raised(cause), !matches!(code, 12 | 13 | 15));
    }
    assert_eq!(TrapCause::EnvironmentCall.m3_name(), "EnvironmentCallFromM");
    assert_eq!(
        TrapCause::EnvironmentCallFromU.m3_name(),
        "EnvironmentCallFromU"
    );
    assert_eq!(
        TrapCause::IllegalInstruction.m3_name(),
        "IllegalInstruction"
    );
}

/// Any state `M3State` can hold.
fn states() -> impl Strategy<Value = (M3State, CsrFile)> {
    (
        modes(),
        any::<[bool; 7]>(),
        modes(),
        any::<[u32; 7]>(),
        any::<u32>(),
    )
        .prop_map(|(privilege, f, mpp, v, mtvec)| {
            let mut m = CsrFile::new();
            m.mie = f[5];
            m.mpie = f[6];
            m.mtvec = mtvec & !3;
            let s = M3State {
                privilege,
                sie: f[0],
                spie: f[1],
                spp: if f[2] { Spp::Supervisor } else { Spp::User },
                mpp,
                sum: f[3],
                mxr: f[4],
                medeleg: v[0] & 0xB1FF,
                stvec: v[1] & !3,
                sscratch: v[2],
                sepc: v[3] & !3,
                scause: v[4],
                stval: v[5],
                satp: v[6] & !(0x1ff << 22) & !(1 << 31),
            };
            (s, m)
        })
}

proptest! {
    /// Each CSR's write rule (§5.1), read back.
    #[test]
    fn every_csr_write_follows_its_rule((mut s, mut m) in states(), value in any::<u32>()) {
        let old_satp = s.satp;
        for csr in WHITELIST {
            let (mut s2, mut m2) = (s, m);
            prop_assert_eq!(s2.write(&mut m2, csr, value), Some(()));
            let got = s2.read(&m2, csr).unwrap();
            let mpp = match value >> 11 & 3 { 1 => 1, 3 => 3, _ => 0 };
            let want = match csr {
                0x300 => value & 0x000c_01aa | mpp << 11,
                0x100 => value & 0x000c_0122,
                0x104 | 0x144 | 0x303 | 0x344 => 0,
                0x302 => value & 0xB1FF,
                0x105 | 0x141 | 0x305 | 0x341 => value & !3,
                0x180 if value >> 31 == 1 => old_satp,
                0x180 => value & !(0x1ff << 22),
                0x304 => value & 0x800,
                _ => value,
            };
            prop_assert_eq!(got, want, "{:#05x}", csr);
            // An sstatus write leaves the M fields; an mstatus write sets the S view.
            if csr == 0x100 {
                prop_assert_eq!(s2.mstatus(&m2) & !0x000c_0122, s.mstatus(&m) & !0x000c_0122);
            }
            if csr == 0x300 {
                prop_assert_eq!(s2.sstatus(), value & 0x000c_0122);
            }
            // Nothing else changes.
            prop_assert_eq!(s2.privilege, s.privilege);
        }
        // Off the whitelist: nothing.
        let before = (s, m);
        prop_assert_eq!(s.write(&mut m, 0x7c0, value), None);
        prop_assert_eq!(s.read(&m, 0xf14), None);
        prop_assert_eq!((s, m), before);
    }

    /// `satp`: a Bare write stores MODE and PPN and zeroes ASID; Sv32 keeps the old value;
    /// and everything a write produces is reachable.
    #[test]
    fn satp_is_bare_only(old in any::<u32>(), value in any::<u32>()) {
        let got = satp_write(old, value);
        if value >> 31 == 0 {
            prop_assert_eq!(got, value & 0x803f_ffff);
            prop_assert!(satp_reachable(got));
        } else {
            prop_assert_eq!(got, old);
        }
        prop_assert_eq!(satp_reachable(value), value & 0xffc0_0000 == 0);
    }

    /// `MRET` (§5.1): priv ← MPP, MIE ← MPIE, MPIE ← 1, MPP ← U; returns mepc.
    #[test]
    fn mret_follows_the_design((mut s, mut m) in states(), mepc in any::<u32>()) {
        m.mepc = mepc & !3;
        let before = (s, m);
        prop_assert_eq!(s.mret(&mut m), mepc & !3);
        prop_assert_eq!(s.privilege, before.0.mpp);
        prop_assert_eq!(s.mpp, Privilege::User);
        prop_assert_eq!((m.mie, m.mpie), (before.1.mpie, true));
        prop_assert_eq!(s.sstatus(), before.0.sstatus());
    }

    /// `SRET` (§5.1): priv ← SPP, SIE ← SPIE, SPIE ← 1, SPP ← U; returns sepc.
    #[test]
    fn sret_follows_the_design((mut s, m) in states()) {
        let before = s;
        prop_assert_eq!(s.sret(), before.sepc);
        prop_assert_eq!(s.privilege, before.spp.privilege());
        prop_assert_eq!(s.spp, Spp::User);
        prop_assert_eq!((s.sie, s.spie), (before.spie, true));
        prop_assert_eq!((s.mpp, s.sum, s.mxr), (before.mpp, before.sum, before.mxr));
        prop_assert_eq!(s.mstatus(&m) & 0x1888, before.mstatus(&m) & 0x1888);
    }

    /// Delegated entry (§5.3), for every cause and mode.
    #[test]
    fn delegated_entry_follows_the_design(
        (mut s, _m) in states(),
        cause in prop::sample::select(vec![
            TrapCause::InstructionAddressMisaligned,
            TrapCause::InstructionAccessFault,
            TrapCause::IllegalInstruction,
            TrapCause::Breakpoint,
            TrapCause::LoadAddressMisaligned,
            TrapCause::LoadAccessFault,
            TrapCause::StoreAddressMisaligned,
            TrapCause::StoreAccessFault,
            TrapCause::EnvironmentCallFromU,
            TrapCause::EnvironmentCallFromS,
            TrapCause::EnvironmentCall,
        ]),
        pc in any::<u32>(),
        tval in any::<u32>(),
    ) {
        let before = s;
        let code = cause.code();
        let delegated = before.privilege != Privilege::Machine && before.medeleg >> code & 1 == 1;
        match s.take_exception(cause, pc, tval) {
            None => {
                prop_assert!(!delegated);
                prop_assert_eq!(s, before);
            }
            Some(handler) => {
                prop_assert!(delegated);
                prop_assert!(code != 9 && code != 11);
                prop_assert_eq!(handler, before.stvec & !3);
                prop_assert_eq!((s.sepc, s.scause, s.stval), (pc, code, tval));
                let spp = if before.privilege == Privilege::Supervisor { Spp::Supervisor } else { Spp::User };
                prop_assert_eq!(s.spp, spp);
                prop_assert_eq!((s.sie, s.spie), (false, before.sie));
                prop_assert_eq!(s.privilege, Privilege::Supervisor);
                prop_assert_eq!(
                    (s.mpp, s.sum, s.mxr, s.medeleg, s.stvec, s.sscratch, s.satp),
                    (before.mpp, before.sum, before.mxr, before.medeleg, before.stvec, before.sscratch, before.satp)
                );
            }
        }
    }

    /// The interrupt with modes (§5.1): eligible when MEIP and MEIE and (below M or MIE);
    /// entry sets MPP to the mode and enters M.
    #[test]
    fn mei_with_modes((mut s, mut m) in states(), level in any::<bool>(), meie in any::<bool>(), pc in any::<u32>()) {
        m.irq_level = level;
        m.meie = meie;
        let eligible = level && meie && (s.privilege != Privilege::Machine || m.mie);
        prop_assert_eq!(s.mei_eligible(&m), eligible);
        if eligible {
            let (from, mie) = (s.privilege, m.mie);
            prop_assert_eq!(s.take_mei(&mut m, pc), m.mtvec & !3);
            prop_assert_eq!((s.privilege, s.mpp), (Privilege::Machine, from));
            prop_assert_eq!((m.mie, m.mpie, m.mepc, m.mcause, m.mtval), (false, mie, pc, 0x8000_000b, 0));
        }
    }
}
