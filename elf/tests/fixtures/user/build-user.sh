#!/usr/bin/env bash
# Builds user.elf, the M3.1 user-image fixture, from user.S (docs/m3-design.md §8.3).
# Linux only; needs the pinned RISC-V toolchain on PATH, the same as
# tests/rv32/hello/build-hello.sh.
#
# Run by hand; the cargo xtask pipeline and CI do not run it yet. After a rebuild, update
# the BLAKE3 hashes in manifest.json: elf/tests/user_fixture.rs checks the committed ELF
# against them. Tests never run this script.
#
# Usage: build-user.sh <cache-dir> <out-dir>
#
# 1. Checks the toolchain versions.
# 2. Builds user.S twice, from two copies at different paths, with the rv32ui flags and
#    the linker's default script, and requires both builds to be byte-identical.
# 3. Checks the ELF: ELF32 RISC-V EXEC, entry == _start, and exactly two PT_LOAD
#    segments, one R+X and one R+W.
# 4. Replaces <out-dir>/user.elf, mode 0644.

set -euo pipefail
export LC_ALL=C

# Pins: the same as build-hello.sh.
GCC_VERSION='riscv64-unknown-elf-gcc (13.2.0-11ubuntu1+12) 13.2.0'
AS_VERSION='GNU assembler (2.42-1ubuntu1+6) 2.42'
PREFIX=riscv64-unknown-elf-
FLAGS=(-march=rv32i -mabi=ilp32 -static -mcmodel=medany -fvisibility=hidden -nostdlib
    -nostartfiles -Wl,--build-id=none)

die() {
    echo "build-user: $*" >&2
    exit 1
}

[ $# -eq 2 ] || die "usage: build-user.sh <cache-dir> <out-dir>"
cache=$(mkdir -p "$1" && cd "$1" && pwd)
out=$(mkdir -p "$2" && cd "$2" && pwd)
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

# 1. Toolchain.
[ "$("${PREFIX}gcc" --version | head -n1)" = "$GCC_VERSION" ] ||
    die "need $GCC_VERSION, found: $("${PREFIX}gcc" --version | head -n1)"
[ "$("${PREFIX}as" --version | head -n1)" = "$AS_VERSION" ] ||
    die "need $AS_VERSION, found: $("${PREFIX}as" --version | head -n1)"

# 2. Two builds from sources at different paths. Compile and link separately, with fixed
# file names: a one-step gcc run would record a random temporary object name.
for side in a b; do
    dir="$cache/user-$side"
    rm -rf "$dir"
    mkdir -p "$dir/src" "$dir/build"
    cp "$here/user.S" "$dir/src/user.S"
    (cd "$dir/src" && "${PREFIX}gcc" "${FLAGS[@]}" -c user.S -o "$dir/build/user.o")
    (cd "$dir/build" && "${PREFIX}gcc" "${FLAGS[@]}" user.o -o user.elf)
done
elf="$cache/user-a/build/user.elf"
cmp -s "$elf" "$cache/user-b/build/user.elf" || die "two builds differ"

# 3. Checks.
header=$("${PREFIX}readelf" -h "$elf")
grep -Eq '^ +Class: +ELF32$' <<<"$header" || die "not ELF32"
grep -Eq '^ +Data: +2.s complement, little endian$' <<<"$header" || die "not LE"
grep -Eq '^ +Type: +EXEC ' <<<"$header" || die "not ET_EXEC"
grep -Eq '^ +Machine: +RISC-V$' <<<"$header" || die "not RISC-V"
start=$("${PREFIX}nm" "$elf" | awk '$3 == "_start" { print $1 }')
entry=$(awk '/^ +Entry point address:/ { print $NF }' <<<"$header")
[ -n "$start" ] && [ "$((16#$start))" = "$((entry))" ] ||
    die "entry $entry is not _start (${start:-missing})"
# Flags are the fields between MemSiz and Align, "R E" for R+X.
loads=$("${PREFIX}readelf" -lW "$elf" |
    awk '$1 == "LOAD" { f = ""; for (i = 7; i < NF; i++) f = f $i; print f }')
[ "$(sort <<<"$loads" | tr '\n' ' ')" = "RE RW " ] ||
    die "PT_LOAD flags are '$(tr '\n' ' ' <<<"$loads")', not one RE and one RW"

# 4. Install. A fixture is data for the simulator: mode 0644, not the linker's 0755.
install -m 0644 "$elf" "$out/user.elf"
echo "built user.elf into $out"
