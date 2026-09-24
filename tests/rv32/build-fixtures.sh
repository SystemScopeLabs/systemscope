#!/usr/bin/env bash
# Builds the rv32ui fixture ELFs from the pinned riscv-tests (docs/m1-design.md §10.2,
# §10.6). Linux only; needs git and the pinned RISC-V toolchain on PATH.
#
# Run it through `cargo xtask rv32-fixtures build`, which passes the selected tests, then
# checks the upstream test list and writes and verifies tests/rv32/fixtures/manifest.json.
# Tests never run this script: fixtures change only when someone rebuilds them on purpose.
#
# Usage: build-fixtures.sh <cache-dir> <out-dir> <test>...
#
# 1. Checks the toolchain versions.
# 2. Fetches riscv-tests at the pinned commit into <cache-dir> (network access happens
#    only here) and checks that the checkout is unmodified.
# 3. Builds every test twice, from two copies of the sources at different paths, with the
#    SystemScope environment (env/riscv_test.h, env/linker.ld), and requires both builds
#    to be byte-identical.
# 4. Checks every ELF: ELF32 RISC-V EXEC, entry == _start == 0x80000000, RISC-V
#    attributes "rv32i2p1", and a disassembly with only RV32I instructions (no CSR,
#    MRET, or other SYSTEM instruction besides ECALL), containing FENCE and ECALL.
# 5. Replaces <out-dir>/rv32ui-*.elf with the new ELFs.

set -euo pipefail
export LC_ALL=C

# Pins. The systemscope-rv32 crate checks that these match its own constants.
RISCV_TESTS_REPO=https://github.com/riscv-software-src/riscv-tests.git
RISCV_TESTS_COMMIT=793a5ff2d99a6d9fbd91e84c34b9a0437e313b88
GCC_VERSION='riscv64-unknown-elf-gcc (13.2.0-11ubuntu1+12) 13.2.0'
AS_VERSION='GNU assembler (2.42-1ubuntu1+6) 2.42'
PREFIX=riscv64-unknown-elf-
# The upstream isa/Makefile flags for the p environment, with -march/-mabi narrowed to
# RV32I and no build-id note.
FLAGS=(-march=rv32i -mabi=ilp32 -static -mcmodel=medany -fvisibility=hidden -nostdlib
    -nostartfiles -Wl,--build-id=none)
# RV32I base instructions as objdump prints them with -M no-aliases.
RV32I="lui auipc jal jalr beq bne blt bge bltu bgeu lb lh lw lbu lhu sb sh sw addi slti
    sltiu xori ori andi slli srli srai add sub sll slt sltu xor srl sra or and fence ecall"

die() {
    echo "build-fixtures: $*" >&2
    exit 1
}

[ $# -ge 3 ] || die "usage: build-fixtures.sh <cache-dir> <out-dir> <test>..."
cache=$(mkdir -p "$1" && cd "$1" && pwd)
out=$(mkdir -p "$2" && cd "$2" && pwd)
shift 2
tests=("$@")
env_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/env" && pwd)

# 1. Toolchain.
[ "$("${PREFIX}gcc" --version | head -n1)" = "$GCC_VERSION" ] ||
    die "need $GCC_VERSION, found: $("${PREFIX}gcc" --version | head -n1)"
[ "$("${PREFIX}as" --version | head -n1)" = "$AS_VERSION" ] ||
    die "need $AS_VERSION, found: $("${PREFIX}as" --version | head -n1)"

# 2. Sources at the pinned commit.
src="$cache/riscv-tests"
if [ "$(git -C "$src" rev-parse HEAD 2>/dev/null || true)" != "$RISCV_TESTS_COMMIT" ]; then
    rm -rf "$src"
    git init -q "$src"
    git -C "$src" remote add origin "$RISCV_TESTS_REPO"
    git -C "$src" fetch -q --depth 1 origin "$RISCV_TESTS_COMMIT"
    git -C "$src" checkout -q --detach FETCH_HEAD
fi
[ "$(git -C "$src" rev-parse HEAD)" = "$RISCV_TESTS_COMMIT" ] || die "wrong riscv-tests commit"
[ -z "$(git -C "$src" status --porcelain)" ] || die "the riscv-tests checkout is modified"

# 3. Two builds from sources at different paths.
copy="$cache/riscv-tests-copy"
rm -rf "$copy" "$cache/build-a" "$cache/build-b"
mkdir -p "$copy" "$cache/build-a" "$cache/build-b"
cp -R "$src/isa" "$copy/isa"
for t in "${tests[@]}"; do
    [ -f "$src/isa/rv32ui/$t.S" ] || die "no upstream source for $t"
    for side in a b; do
        if [ "$side" = a ]; then isa="$src/isa"; else isa="$copy/isa"; fi
        # Compile and link separately: the linker records each object's file name in a
        # FILE symbol, and a one-step gcc run would pass it a random temporary name.
        (cd "$isa" && "${PREFIX}gcc" "${FLAGS[@]}" -I"$env_dir" -Imacros/scalar -c \
            "rv32ui/$t.S" -o "$cache/build-$side/rv32ui-$t.o")
        (cd "$cache/build-$side" && "${PREFIX}gcc" "${FLAGS[@]}" -T"$env_dir/linker.ld" \
            "rv32ui-$t.o" -o "rv32ui-$t.elf")
    done
    cmp -s "$cache/build-a/rv32ui-$t.elf" "$cache/build-b/rv32ui-$t.elf" ||
        die "$t: two builds differ"
done

# 4. Checks.
for t in "${tests[@]}"; do
    elf="$cache/build-a/rv32ui-$t.elf"
    header=$("${PREFIX}readelf" -h "$elf")
    grep -Eq '^ +Class: +ELF32$' <<<"$header" || die "$t: not ELF32"
    grep -Eq '^ +Data: +2.s complement, little endian$' <<<"$header" || die "$t: not LE"
    grep -Eq '^ +Type: +EXEC ' <<<"$header" || die "$t: not ET_EXEC"
    grep -Eq '^ +Machine: +RISC-V$' <<<"$header" || die "$t: not RISC-V"
    grep -Eq '^ +Entry point address: +0x80000000$' <<<"$header" || die "$t: entry"
    start=$("${PREFIX}nm" "$elf" | awk '$3 == "_start" { print $1 }')
    [ "$start" = 80000000 ] || die "$t: _start is at '$start'"
    "${PREFIX}readelf" -A "$elf" | grep -Eq 'Tag_RISCV_arch: "rv32i2p1"$' ||
        die "$t: RISC-V attributes are not rv32i2p1"
    # One mnemonic per instruction line: "  addr:<TAB>word<TAB>mnemonic<TAB>operands".
    # A word that is not an instruction prints as ".insn" or ".word" and fails the
    # allowlist; objdump elides runs of zero words, such as RVTEST_CODE_END's, as "...".
    mnemonics=$("${PREFIX}objdump" -d -M no-aliases "$elf" |
        awk -F'\t' '/^ *[0-9a-f]+:\t/ { split($3, m, " "); print m[1] }')
    [ -n "$mnemonics" ] || die "$t: no instructions"
    while read -r m; do
        case " $(echo $RV32I) " in
        *" $m "*) ;;
        *) die "$t: '$m' is not an RV32I instruction" ;;
        esac
    done < <(sort -u <<<"$mnemonics")
    grep -qx fence <<<"$mnemonics" || die "$t: no FENCE (RVTEST_PASS)"
    grep -qx ecall <<<"$mnemonics" || die "$t: no ECALL (RVTEST_PASS)"
done

# 5. Install.
rm -f "$out"/rv32ui-*.elf
for t in "${tests[@]}"; do
    cp "$cache/build-a/rv32ui-$t.elf" "$out/rv32ui-$t.elf"
done
echo "built ${#tests[@]} fixtures into $out"
