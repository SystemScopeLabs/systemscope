#!/usr/bin/env bash
# Builds block_irq.elf, the M2 end-to-end program, from block_irq.S (docs/m2-design.md
# §12). Linux only; needs the pinned RISC-V toolchain on PATH, the same as
# build-fixtures.sh and build-hello.sh.
#
# Run it through `cargo xtask rv32-fixtures build`, which then writes the disk fixture,
# tests/rv32/block_irq/manifest.json, and verifies both. Tests never run this script.
#
# Usage: build-block-irq.sh <cache-dir> <out-dir>
#
# 1. Checks the toolchain versions.
# 2. Builds block_irq.S twice, from two copies at different paths, with the rv32ui flags
#    (-march=rv32i_zicsr instead of rv32i) and env/linker.ld, and requires both builds to
#    be byte-identical.
# 3. Checks the ELF: ELF32 RISC-V EXEC, entry == _start == 0x80000000, RISC-V attributes
#    "rv32i2p1_zicsr2p0", the symbols at the addresses the systemscope-rv32 crate pins,
#    and a disassembly with only RV32I, Zicsr, and MRET instructions: no WFI, one MRET,
#    and one ECALL.
# 4. Replaces <out-dir>/block_irq.elf, mode 0644.

set -euo pipefail
export LC_ALL=C

# Pins: the same as build-fixtures.sh. The systemscope-rv32 crate checks the scripts
# against its own constants.
GCC_VERSION='riscv64-unknown-elf-gcc (13.2.0-11ubuntu1+12) 13.2.0'
AS_VERSION='GNU assembler (2.42-1ubuntu1+6) 2.42'
PREFIX=riscv64-unknown-elf-
FLAGS=(-march=rv32i_zicsr -mabi=ilp32 -static -mcmodel=medany -fvisibility=hidden -nostdlib
    -nostartfiles -Wl,--build-id=none)
# RV32I base instructions, the Zicsr instructions, and MRET, as objdump prints them with
# -M no-aliases.
ALLOWED="lui auipc jal jalr beq bne blt bge bltu bgeu lb lh lw lbu lhu sb sh sw addi slti
    sltiu xori ori andi slli srli srai add sub sll slt sltu xor srl sra or and fence ecall
    csrrw csrrs csrrc csrrwi csrrsi csrrci mret"
# Where the symbols must be: the addresses the systemscope-rv32 crate pins
# (block_irq::POLL, HANDLER, VARS, BUFFER_A, BUFFER_B).
SYMBOLS="poll=8000014c handler=80000164 vars=80002000 buffer_a=80002200 buffer_b=80002400"

die() {
    echo "build-block-irq: $*" >&2
    exit 1
}

[ $# -eq 2 ] || die "usage: build-block-irq.sh <cache-dir> <out-dir>"
cache=$(mkdir -p "$1" && cd "$1" && pwd)
out=$(mkdir -p "$2" && cd "$2" && pwd)
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
linker="$here/../env/linker.ld"

# 1. Toolchain.
[ "$("${PREFIX}gcc" --version | head -n1)" = "$GCC_VERSION" ] ||
    die "need $GCC_VERSION, found: $("${PREFIX}gcc" --version | head -n1)"
[ "$("${PREFIX}as" --version | head -n1)" = "$AS_VERSION" ] ||
    die "need $AS_VERSION, found: $("${PREFIX}as" --version | head -n1)"

# 2. Two builds from sources at different paths. Compile and link separately, with fixed
# file names: a one-step gcc run would record a random temporary object name.
for side in a b; do
    dir="$cache/block_irq-$side"
    rm -rf "$dir"
    mkdir -p "$dir/src" "$dir/build"
    cp "$here/block_irq.S" "$dir/src/block_irq.S"
    (cd "$dir/src" && "${PREFIX}gcc" "${FLAGS[@]}" -c block_irq.S -o "$dir/build/block_irq.o")
    (cd "$dir/build" && "${PREFIX}gcc" "${FLAGS[@]}" -T"$linker" block_irq.o -o block_irq.elf)
done
elf="$cache/block_irq-a/build/block_irq.elf"
cmp -s "$elf" "$cache/block_irq-b/build/block_irq.elf" || die "two builds differ"

# 3. Checks.
header=$("${PREFIX}readelf" -h "$elf")
grep -Eq '^ +Class: +ELF32$' <<<"$header" || die "not ELF32"
grep -Eq '^ +Data: +2.s complement, little endian$' <<<"$header" || die "not LE"
grep -Eq '^ +Type: +EXEC ' <<<"$header" || die "not ET_EXEC"
grep -Eq '^ +Machine: +RISC-V$' <<<"$header" || die "not RISC-V"
grep -Eq '^ +Entry point address: +0x80000000$' <<<"$header" || die "entry"
symbols=$("${PREFIX}nm" "$elf")
start=$(awk '$3 == "_start" { print $1 }' <<<"$symbols")
[ "$start" = 80000000 ] || die "_start is at '$start'"
for pair in $SYMBOLS; do
    name=${pair%%=*}
    at=$(awk -v n="$name" '$3 == n { print $1 }' <<<"$symbols")
    [ "$at" = "${pair#*=}" ] || die "$name is at '$at', not ${pair#*=}"
done
"${PREFIX}readelf" -A "$elf" | grep -Eq 'Tag_RISCV_arch: "rv32i2p1_zicsr2p0"$' ||
    die "RISC-V attributes are not rv32i2p1_zicsr2p0"
mnemonics=$("${PREFIX}objdump" -d -M no-aliases -j .text.init "$elf" |
    awk -F'\t' '/^ *[0-9a-f]+:\t/ { split($3, m, " "); print m[1] }')
[ -n "$mnemonics" ] || die "no instructions"
while read -r m; do
    case " $(echo $ALLOWED) " in
    *" $m "*) ;;
    *) die "'$m' is not an RV32I, Zicsr, or MRET instruction" ;;
    esac
done < <(sort -u <<<"$mnemonics")
! grep -qx wfi <<<"$mnemonics" || die "WFI"
[ "$(grep -cx mret <<<"$mnemonics")" = 1 ] || die "not exactly one MRET"
[ "$(grep -cx ecall <<<"$mnemonics")" = 1 ] || die "not exactly one ECALL"

# 4. Install. A fixture is data for the simulator: mode 0644, not the linker's 0755.
install -m 0644 "$elf" "$out/block_irq.elf"
echo "built block_irq.elf into $out"
