//! The integer register file (`docs/m1-design.md` §5.1).

use crate::instr::Reg;

/// The 32 integer registers. `x0` always reads 0 and ignores writes. It has no storage, so
/// no sequence of calls can make it non-zero.
///
/// `x1` to `x31` are stored in index order, the order a snapshot will write them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct RegisterFile {
    /// `x[i]` holds register `x{i + 1}`.
    x: [u32; 31],
}

impl RegisterFile {
    /// All registers zero.
    pub fn new() -> RegisterFile {
        RegisterFile::default()
    }

    /// The value of `reg`; always 0 for `x0`.
    pub fn read(&self, reg: Reg) -> u32 {
        match usize::from(reg.index()).checked_sub(1) {
            None => 0,
            // A `Reg` index is below 32, so this is below 31.
            Some(i) => self.x[i],
        }
    }

    /// Sets `reg` to `value`. A write to `x0` has no effect.
    pub fn write(&mut self, reg: Reg, value: u32) {
        if let Some(i) = usize::from(reg.index()).checked_sub(1) {
            self.x[i] = value;
        }
    }
}
