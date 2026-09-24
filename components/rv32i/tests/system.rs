//! `FENCE`, `ECALL`, and `EBREAK` (`docs/m1-design.md` §5.2, §6), and the trap-cause names
//! the `rv32.trap` record uses.

use systemscope_rv32i::{
    ExecOutcome, NotSystem, PendingEffect, PendingTrap, TrapCause, decode, execute_system,
};

const PC: u32 = 0x8000_0100;

fn system(word: u32, pc: u32) -> Result<ExecOutcome, NotSystem> {
    execute_system(&decode(word).unwrap(), pc)
}

#[test]
fn fence_retires_as_a_no_op() {
    for pc in [PC, 0xffff_fffc] {
        assert_eq!(
            system(0x0ff0_000f, pc),
            Ok(ExecOutcome::Effect(PendingEffect {
                reg_write: None,
                next_pc: pc.wrapping_add(4),
            }))
        );
    }
}

#[test]
fn ecall_traps_with_a_zero_trap_value() {
    assert_eq!(
        system(0x0000_0073, PC),
        Ok(ExecOutcome::Trap(PendingTrap {
            cause: TrapCause::EnvironmentCall,
            tval: 0,
        }))
    );
}

#[test]
fn ebreak_traps_with_its_own_pc() {
    assert_eq!(
        system(0x0010_0073, PC),
        Ok(ExecOutcome::Trap(PendingTrap {
            cause: TrapCause::Breakpoint,
            tval: PC,
        }))
    );
}

#[test]
fn other_instructions_are_not_system() {
    // LUI, AUIPC, ADDI, SLLI, ADD, LW, SW, JAL, JALR, BEQ.
    for word in [
        0x1234_52b7,
        0x0000_1297,
        0x0010_0293,
        0x0013_1293,
        0x0073_02b3,
        0x0000_2003,
        0x0000_2023,
        0x0000_006f,
        0x0000_8067,
        0x0000_0063,
    ] {
        assert_eq!(system(word, PC), Err(NotSystem), "{word:#010x}");
    }
}

#[test]
fn trap_causes_are_named_as_in_the_design() {
    let names = [
        (
            TrapCause::InstructionAddressMisaligned,
            "InstructionAddressMisaligned",
        ),
        (TrapCause::InstructionAccessFault, "InstructionAccessFault"),
        (TrapCause::IllegalInstruction, "IllegalInstruction"),
        (TrapCause::Breakpoint, "Breakpoint"),
        (TrapCause::EnvironmentCall, "EnvironmentCall"),
        (TrapCause::LoadAddressMisaligned, "LoadAddressMisaligned"),
        (TrapCause::LoadAccessFault, "LoadAccessFault"),
        (TrapCause::StoreAddressMisaligned, "StoreAddressMisaligned"),
        (TrapCause::StoreAccessFault, "StoreAccessFault"),
    ];
    for (cause, name) in names {
        assert_eq!(cause.name(), name);
    }
}
