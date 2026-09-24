#!/usr/bin/env bash
# Builds hello.elf, the M1-A5 program, from hello.S (docs/m1-design.md §7.3, §10.6). Linux
# only; needs the pinned RISC-V toolchain on PATH, the same as build-fixtures.sh.
#
# Run it through `cargo xtask rv32-fixtures build`, which then writes and verifies
# tests/rv32/hello/manifest.json. Tests never run this script.
#
# Usage: build-hello.sh <cache-dir> <out-dir>
#
# 1. Checks the toolchain versions.
# 2. Builds hello.S twice, from two copies at different paths, with the rv32ui flags and
#    env/linker.ld, and requires both builds to be byte-identical.
# 3. Checks the ELF: ELF32 RISC-V EXEC, entry == _start == 0x80000000, RISC-V attributes
#    "rv32i2p1", and a disassembly with only RV32I instructions, one SB, no other store,
#    and one ECALL as its only SYSTEM instruction.
# 4. Replaces <out-dir>/hello.elf, mode 0644.

set -euo pipefail
export LC_ALL=C

# Pins: the same as build-fixtures.sh. The systemscope-rv32 crate checks both scripts
# against its own constants.
GCC_VERSION='riscv64-unknown-elf-gcc (13.2.0-11ubuntu1+12) 13.2.0'
AS_VERSION='GNU assembler (2.42-1ubuntu1+6) 2.42'
PREFIX=riscv64-unknown-elf-
FLAGS=(-march=rv32i -mabi=ilp32 -static -mcmodel=medany -fvisibility=hidden -nostdlib
    -nostartfiles -Wl,--build-id=none)
# RV32I base instructions as objdump prints them with -M no-aliases.
RV32I="lui auipc jal jalr beq bne blt bge bltu bgeu lb lh lw lbu lhu sb sh sw addi slti
    sltiu xori ori andi slli srli srai add sub sll slt sltu xor srl sra or and fence ecall"

die() {
    echo "build-hello: $*" >&2
    exit 1
}

[ $# -eq 2 ] || die "usage: build-hello.sh <cache-dir> <out-dir>"
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
    dir="$cache/hello-$side"
    rm -rf "$dir"
    mkdir -p "$dir/src" "$dir/build"
    cp "$here/hello.S" "$dir/src/hello.S"
    (cd "$dir/src" && "${PREFIX}gcc" "${FLAGS[@]}" -c hello.S -o "$dir/build/hello.o")
    (cd "$dir/build" && "${PREFIX}gcc" "${FLAGS[@]}" -T"$linker" hello.o -o hello.elf)
done
elf="$cache/hello-a/build/hello.elf"
cmp -s "$elf" "$cache/hello-b/build/hello.elf" || die "two builds differ"

# 3. Checks.
header=$("${PREFIX}readelf" -h "$elf")
grep -Eq '^ +Class: +ELF32$' <<<"$header" || die "not ELF32"
grep -Eq '^ +Data: +2.s complement, little endian$' <<<"$header" || die "not LE"
grep -Eq '^ +Type: +EXEC ' <<<"$header" || die "not ET_EXEC"
grep -Eq '^ +Machine: +RISC-V$' <<<"$header" || die "not RISC-V"
grep -Eq '^ +Entry point address: +0x80000000$' <<<"$header" || die "entry"
start=$("${PREFIX}nm" "$elf" | awk '$3 == "_start" { print $1 }')
[ "$start" = 80000000 ] || die "_start is at '$start'"
"${PREFIX}readelf" -A "$elf" | grep -Eq 'Tag_RISCV_arch: "rv32i2p1"$' ||
    die "RISC-V attributes are not rv32i2p1"
mnemonics=$("${PREFIX}objdump" -d -M no-aliases -j .text.init "$elf" |
    awk -F'\t' '/^ *[0-9a-f]+:\t/ { split($3, m, " "); print m[1] }')
[ -n "$mnemonics" ] || die "no instructions"
while read -r m; do
    case " $(echo $RV32I) " in
    *" $m "*) ;;
    *) die "'$m' is not an RV32I instruction" ;;
    esac
done < <(sort -u <<<"$mnemonics")
[ "$(grep -cx sb <<<"$mnemonics")" = 1 ] || die "not exactly one SB"
! grep -Eqx 'sh|sw' <<<"$mnemonics" || die "a store other than SB"
[ "$(grep -cx ecall <<<"$mnemonics")" = 1 ] || die "not exactly one ECALL"
! grep -Eqx 'fence' <<<"$mnemonics" || die "unexpected FENCE"

# 4. Install. A fixture is data for the simulator: mode 0644, not the linker's 0755.
install -m 0644 "$elf" "$out/hello.elf"
echo "built hello.elf into $out"
