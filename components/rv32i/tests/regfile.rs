//! The register file and `Reg` (`docs/m1-design.md` §5.1).

use proptest::prelude::*;
use systemscope_rv32i::{Reg, RegisterFile};

fn x(i: u8) -> Reg {
    Reg::new(i).unwrap()
}

fn all() -> impl Iterator<Item = Reg> {
    (0..32).map(x)
}

#[test]
fn reg_indices_are_below_32() {
    assert_eq!(Reg::new(0), Some(Reg::ZERO));
    assert_eq!(Reg::new(31).map(Reg::index), Some(31));
    assert_eq!(Reg::new(32), None);
    assert_eq!(Reg::new(u8::MAX), None);
    for (i, r) in (0..32).zip(all()) {
        assert_eq!(r.index(), i);
    }
}

#[test]
fn every_register_starts_at_zero() {
    let regs = RegisterFile::new();
    for r in all() {
        assert_eq!(regs.read(r), 0, "{r:?}");
    }
    assert_eq!(regs, RegisterFile::default());
}

#[test]
fn writes_to_x0_are_discarded() {
    let mut regs = RegisterFile::new();
    regs.write(Reg::ZERO, 0xdead_beef);
    assert_eq!(regs.read(Reg::ZERO), 0);
    // Discarded, not stored somewhere else either.
    assert_eq!(regs, RegisterFile::new());
}

#[test]
fn x1_and_x31_hold_what_was_written() {
    let mut regs = RegisterFile::new();
    regs.write(x(1), 0x1234_5678);
    regs.write(x(31), u32::MAX);
    assert_eq!(regs.read(x(1)), 0x1234_5678);
    assert_eq!(regs.read(x(31)), u32::MAX);
}

#[test]
fn registers_are_independent() {
    let mut regs = RegisterFile::new();
    for r in all() {
        regs.write(r, 0x100 + u32::from(r.index()));
    }
    assert_eq!(regs.read(Reg::ZERO), 0);
    for r in all().skip(1) {
        assert_eq!(regs.read(r), 0x100 + u32::from(r.index()), "{r:?}");
    }
    regs.write(x(7), 0);
    assert_eq!(
        (regs.read(x(6)), regs.read(x(7)), regs.read(x(8))),
        (0x106, 0, 0x108)
    );
}

#[test]
fn the_last_write_wins() {
    let mut regs = RegisterFile::new();
    for v in [1, u32::MAX, 0, 0x8000_0000] {
        regs.write(x(5), v);
        assert_eq!(regs.read(x(5)), v);
    }
}

proptest! {
    /// Any sequence of writes leaves every register as a plain 32-entry model says, with
    /// `x0` still zero.
    #[test]
    fn matches_a_model_under_arbitrary_writes(
        writes in prop::collection::vec((0u8..32, any::<u32>()), 0..200),
    ) {
        let mut regs = RegisterFile::new();
        let mut model = [0u32; 32];
        for (i, v) in writes {
            regs.write(x(i), v);
            if i != 0 {
                model[usize::from(i)] = v;
            }
            prop_assert_eq!(regs.read(Reg::ZERO), 0);
        }
        for r in all() {
            prop_assert_eq!(regs.read(r), model[usize::from(r.index())]);
        }
    }
}
