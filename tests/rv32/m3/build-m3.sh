#!/usr/bin/env bash
# Builds the M3 reference fixtures (docs/m3-design.md §7, §12.2): m3-firmware.elf, the
# guest glue, and the five user programs of the M3 scenario. Linux only; needs the pinned
# RISC-V toolchain on PATH, the same as build-fixtures.sh and build-block-irq.sh.
#
# Run it through `cargo xtask rv32-fixtures build`, which then writes the disk images,
# tests/rv32/m3/manifest.json, and verifies them. Tests never run this script.
#
# Usage: build-m3.sh <cache-dir> <out-dir>
#
# 1. Checks the toolchain versions.
# 2. Builds every program twice, from two copies at different paths, and requires both
#    builds to be byte-identical. The firmware uses -march=rv32i_zicsr and firmware.ld;
#    the user programs -march=rv32i and user.ld, or user-data.ld for hello. Everything is assembled with
#    `.option norelax` and linked with --no-relax.
# 3. Checks each ELF: ELF32 RISC-V EXEC, the entry at _start (0x80000000 for the
#    firmware, USER_BASE 0x10000 for a user program), the RISC-V attributes, and a
#    disassembly with only the allowed instructions. A user program has exactly its
#    PT_LOAD segments, each with a non-zero size, at USER_BASE and above, page-aligned. The firmware has exactly one MRET,
#    one SRET, one SFENCE.VMA, and one ECALL, and no WFI. A user program has only RV32I
#    instructions and at least one ECALL.
# 4. Checks gp independence (§17.1): no object has a relaxation or gp-relative
#    relocation, and no user program instruction names x3. In the firmware, only the
#    trampoline's save and restore of the context name it.
# 5. Replaces the ELFs in <out-dir>, mode 0644.

set -euo pipefail
export LC_ALL=C

# Pins: the same as build-fixtures.sh. The systemscope-rv32 crate checks the scripts
# against its own constants.
GCC_VERSION='riscv64-unknown-elf-gcc (13.2.0-11ubuntu1+12) 13.2.0'
AS_VERSION='GNU assembler (2.42-1ubuntu1+6) 2.42'
PREFIX=riscv64-unknown-elf-
COMMON=(-mabi=ilp32 -static -mcmodel=medany -fvisibility=hidden -nostdlib -nostartfiles
    -Wl,--build-id=none -Wl,--no-relax)
FIRMWARE_FLAGS=(-march=rv32i_zicsr "${COMMON[@]}")
USER_FLAGS=(-march=rv32i "${COMMON[@]}")
USER_PROGRAMS="hello ping pong fault badptr"
# The PT_LOAD segments each user program must have, as `readelf -lW` flags: hello has
# text, rodata, data, and bss (user-data.ld); the others text and rodata (user.ld).
SEGMENTS_hello="R E,R,RW,RW"
SEGMENTS_OTHER="R E,R"
# RV32I base instructions as objdump prints them with -M no-aliases.
RV32I="lui auipc jal jalr beq bne blt bge bltu bgeu lb lh lw lbu lhu sb sh sw addi slti
    sltiu xori ori andi slli srli srai add sub sll slt sltu xor srl sra or and fence ecall"
# The firmware adds the Zicsr instructions, MRET, SRET, and SFENCE.VMA.
FIRMWARE_ALLOWED="$RV32I csrrw csrrs csrrc csrrwi csrrsi csrrci mret sret sfence.vma"

die() {
    echo "build-m3: $*" >&2
    exit 1
}

[ $# -eq 2 ] || die "usage: build-m3.sh <cache-dir> <out-dir>"
cache=$(mkdir -p "$1" && cd "$1" && pwd)
out=$(mkdir -p "$2" && cd "$2" && pwd)
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

# 1. Toolchain.
[ "$("${PREFIX}gcc" --version | head -n1)" = "$GCC_VERSION" ] ||
    die "need $GCC_VERSION, found: $("${PREFIX}gcc" --version | head -n1)"
[ "$("${PREFIX}as" --version | head -n1)" = "$AS_VERSION" ] ||
    die "need $AS_VERSION, found: $("${PREFIX}as" --version | head -n1)"

# build <name> <linker script> <flags...>: two builds from sources at different paths.
# Compile and link separately, with fixed file names: a one-step gcc run would record a
# random temporary object name.
build() {
    local name=$1 script=$2
    shift 2
    for side in a b; do
        local dir="$cache/m3-$side"
        mkdir -p "$dir/src" "$dir/build"
        cp "$here/$name.S" "$dir/src/$name.S"
        cp "$here/$script" "$dir/src/$script"
        (cd "$dir/src" && "${PREFIX}gcc" "$@" -c "$name.S" -o "$dir/build/$name.o")
        (cd "$dir/build" && "${PREFIX}gcc" "$@" -T"$dir/src/$script" "$name.o" -o "$name.elf")
    done
    cmp -s "$cache/m3-a/build/$name.elf" "$cache/m3-b/build/$name.elf" ||
        die "$name: two builds differ"
}

# check <name> <entry> <arch attribute> <allowed mnemonics> <the instructions naming x3>
check() {
    local name=$1 entry=$2 arch=$3 allowed=$4 x3=$5
    local elf="$cache/m3-a/build/$name.elf" obj="$cache/m3-a/build/$name.o"
    local header symbols start mnemonics
    header=$("${PREFIX}readelf" -h "$elf")
    grep -Eq '^ +Class: +ELF32$' <<<"$header" || die "$name: not ELF32"
    grep -Eq '^ +Data: +2.s complement, little endian$' <<<"$header" || die "$name: not LE"
    grep -Eq '^ +Type: +EXEC ' <<<"$header" || die "$name: not ET_EXEC"
    grep -Eq '^ +Machine: +RISC-V$' <<<"$header" || die "$name: not RISC-V"
    grep -Eq "^ +Entry point address: +0x$entry\$" <<<"$header" || die "$name: entry"
    symbols=$("${PREFIX}nm" "$elf")
    start=$(awk '$3 == "_start" { print $1 }' <<<"$symbols")
    [ "$start" = "$(printf %08x 0x"$entry")" ] || die "$name: _start is at '$start'"
    "${PREFIX}readelf" -A "$elf" | grep -Eq "Tag_RISCV_arch: \"$arch\"\$" ||
        die "$name: RISC-V attributes are not $arch"
    mnemonics=$("${PREFIX}objdump" -d -M no-aliases "$elf" |
        awk -F'\t' '/^ *[0-9a-f]+:\t/ { split($3, m, " "); print m[1] }')
    [ -n "$mnemonics" ] || die "$name: no instructions"
    while read -r m; do
        case " $(echo $allowed) " in
        *" $m "*) ;;
        *) die "$name: '$m' is not an allowed instruction" ;;
        esac
    done < <(sort -u <<<"$mnemonics")
    ! grep -qx wfi <<<"$mnemonics" || die "$name: WFI"
    grep -qx ecall <<<"$mnemonics" || die "$name: no ECALL"
    # 4. gp independence: no relaxation or gp-relative relocation, and x3 named only
    # where allowed.
    local uses
    uses=$("${PREFIX}objdump" -d -M no-aliases,numeric "$elf" |
        awk -F'\t' '/^ *[0-9a-f]+:\t/ { print $3 " " $4 }' |
        sed -E 's/[[:space:]]+/ /g; s/ $//' | grep -E '(^|[ ,(])x3([ ,)]|$)' || true)
    [ "$uses" = "$x3" ] || die "$name: x3 (gp) uses are '$uses', not '$x3'"
    ! "${PREFIX}readelf" -r "$obj" | grep -Eq 'R_RISCV_(RELAX|GPREL)' ||
        die "$name: the object has a relaxation or gp-relative relocation"
    ! grep -qw __global_pointer\$ <<<"$symbols" || die "$name: defines __global_pointer\$"
}

rm -rf "$cache/m3-a" "$cache/m3-b"

# 2-4. The firmware.
build firmware firmware.ld "${FIRMWARE_FLAGS[@]}"
check firmware 80000000 rv32i2p1_zicsr2p0 "$FIRMWARE_ALLOWED" \
    "$(printf 'sw x3,8(x31)\nlw x3,8(x31)')"
fw_mnemonics=$("${PREFIX}objdump" -d -M no-aliases "$cache/m3-a/build/firmware.elf" |
    awk -F'\t' '/^ *[0-9a-f]+:\t/ { split($3, m, " "); print m[1] }')
for one in mret sret sfence.vma ecall; do
    [ "$(grep -cx "$one" <<<"$fw_mnemonics")" = 1 ] || die "firmware: not exactly one $one"
done

# 2-4. The user programs.
for name in $USER_PROGRAMS; do
    if [ "$name" = hello ]; then
        script=user-data.ld want=$SEGMENTS_hello
    else
        script=user.ld want=$SEGMENTS_OTHER
    fi
    build "$name" "$script" "${USER_FLAGS[@]}"
    check "$name" 10000 rv32i2p1 "$RV32I" ""
    loads=$("${PREFIX}readelf" -lW "$cache/m3-a/build/$name.elf" | awk '$1 == "LOAD"')
    flags=$(awk '{ f = $7; for (i = 8; i < NF; i++) f = f " " $i; print f }' <<<"$loads" |
        paste -sd,)
    [ "$flags" = "$want" ] || die "$name: segments are '$flags', not '$want'"
    while read -r _ offset vaddr _ filesz memsz _; do
        [ $((memsz)) -gt 0 ] || die "$name: an empty segment at $vaddr"
        [ $((vaddr)) -ge $((0x10000)) ] && [ $((vaddr % 0x1000)) = 0 ] ||
            die "$name: a segment at $vaddr"
    done <<<"$loads"
done

# 5. Install. A fixture is data for the simulator: mode 0644, not the linker's 0755.
install -m 0644 "$cache/m3-a/build/firmware.elf" "$out/m3-firmware.elf"
for name in $USER_PROGRAMS; do
    install -m 0644 "$cache/m3-a/build/$name.elf" "$out/$name.elf"
done
echo "built m3-firmware.elf and $USER_PROGRAMS into $out"
