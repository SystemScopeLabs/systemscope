#!/usr/bin/env bash
# Builds SystemScope's ACT4 corpus: the ACT4 RV32I self-checking ELFs, with expected
# values from the Sail reference model (M1-A4, docs/m1-design.md §10.4). Linux x86_64
# only; needs git, curl, tar, xz, sha256sum, make, and a host C compiler (for the native
# extensions of UDB's Ruby gems).
#
# Run it through `cargo xtask act4 build`, or by hand followed by `cargo xtask act4
# install <out-dir>`; CI runs it twice and compares with `cargo xtask act4 check`. Tests
# never run this script, and nothing but it needs ACT4, Sail, the RISC-V GCC, Ruby, or
# Python: the committed ELFs are checked and run with no external tool.
#
# Usage: build-act4.sh <cache-dir> <out-dir>
#
# 1. Prepares the pinned stack in <cache-dir> (network access happens only here): the
#    ACT4 commit, the Sail release, the riscv-collab GCC release, mise, and through mise
#    Ruby, Bundler, and uv. Every download, binary, and version line is checked against
#    the pins below on every run, cache or not.
# 2. Generates in the canonical directory $CANON, from scratch: a fresh ACT4 checkout at
#    the pin, the configuration from tests/act4/config/systemscope-rv32i, and ACT4's
#    `make elfs` with EXTENSIONS=I. ACT4 selects the tests; the ELFs carry $CANON paths
#    in their debug information, so every machine builds them at the same path.
# 3. Checks the result: the ACT4 checkout is unchanged, every selected test went through
#    Sail (.sig.elf, then .sig, then .results, then the final ELF), and every final ELF
#    holds RV32I instructions only, plus ECALL, by GNU objdump.
# 4. Writes <out-dir>/elfs/<test>.elf (mode 0644) and <out-dir>/record.txt, which
#    `cargo xtask act4` turns into, or compares with, tests/act4/manifest.json.

set -euo pipefail
export LC_ALL=C TZ=UTC

# Pins. The systemscope-rv32 crate checks that these match its own constants.
ACT4_REPO=https://github.com/riscv/riscv-arch-test.git
ACT4_COMMIT=54cfe21bb70ecc0609ab5a70588f8ecfc3e4bf88
ACT4_TESTPLAN_SHA256=12fc316e0fbacbebde91c3d730871929ec235f22e3a060aafed4734e1e4c0272
ACT4_UV_LOCK_SHA256=4f5740cd6457b3c4bff96bd2b7b42b1785c987a3ead6c70d0b89b1493bd8a93b
ACT4_GEMFILE_LOCK_SHA256=5e06729627ddcfdb70ed37855c9309245b868db4f8461c4f63430c6b3d6891c5
SAIL_VERSION=0.14.1
SAIL_URL=https://github.com/riscv/sail-riscv/releases/download/0.14.1/sail-riscv-Linux-x86_64.tar.gz
SAIL_TARBALL_SHA256=de45a89748ca67a8a522b3ac0924c303b5609a16bb50d759bbd08c4d440df0eb
SAIL_BINARY_SHA256=4ccc3bb600387165323f8812bf534e91740f51d1c05ebf6184afb4f6f2a9d2a9
GCC_URL=https://github.com/riscv-collab/riscv-gnu-toolchain/releases/download/2026.08.27/riscv64-elf-ubuntu-24.04-gcc.tar.xz
GCC_TARBALL_SHA256=fe7dadf99dfaee59855b4be5f8d491dc66593bec295090e155a3ec51f0d14f56
GCC_BINARY_SHA256=fa4616e3aa8b2abddeb95e1b13be57a1ec425203278bf20debb02ce6c63385c3
AS_BINARY_SHA256=aad8812815e58b1727e31cf7d50302df3807e8c74847cfa29f24711f0895407c
GCC_VERSION_LINE='riscv64-unknown-elf-gcc (g6afcc4f6d) 16.1.0'
AS_VERSION_LINE='GNU assembler (GNU Binutils) 2.47.20260726'
MISE_URL=https://github.com/jdx/mise/releases/download/v2026.9.12/mise-v2026.9.12-linux-x64
MISE_SHA256=e79ae57945034903aee8aa2ea66b4c7ca9cd4f4edd5a8a78a589cbae6d0f428a
MISE_VERSION_PREFIX='2026.9.12 linux-x64'
RUBY_VERSION_PREFIX='ruby 3.4.10 '
UV_VERSION_PREFIX='uv 0.11.33 '
EXTENSIONS=I
CANON=/tmp/systemscope-act4
CONFIG_NAME=systemscope-rv32i

# The mnemonics a final ELF may hold (GNU objdump -M no-aliases): RV32I, and ECALL,
# which ends every test. FENCE.TSO is the RV32I FENCE encoding with fm = 1000.
RV32I_MNEMONICS="lui auipc jal jalr beq bne blt bge bltu bgeu lb lh lw lbu lhu sb sh sw
addi slti sltiu xori ori andi slli srli srai add sub sll slt sltu xor srl sra or and
fence fence.tso ecall"

die() {
    echo "build-act4: $*" >&2
    exit 1
}

[ $# -eq 2 ] || die "usage: build-act4.sh <cache-dir> <out-dir>"
[ "$(uname -s)-$(uname -m)" = Linux-x86_64 ] || die "Linux x86_64 only"
script_dir=$(cd "$(dirname "$0")" && pwd)
config_src="$script_dir/config/$CONFIG_NAME"
cache=$(mkdir -p "$1" && cd "$1" && pwd)
out=$(mkdir -p "$2" && cd "$2" && pwd)
case "$cache/" in "$CANON"/*) die "the cache must not be under $CANON" ;; esac
case "$out/" in "$CANON"/*) die "the output must not be under $CANON" ;; esac

sha256() { sha256sum "$1" | cut -d' ' -f1; }
check_sha256() { # <file> <expected> <what>
    local actual
    actual=$(sha256 "$1")
    [ "$actual" = "$2" ] || die "$3: sha256 $actual, pinned $2"
}
first_line() { "$@" 2>/dev/null | head -n1; }

# 1. The pinned stack. ------------------------------------------------------------------
mkdir -p "$cache/dl"
fetch() { # <url> <file> <sha256>
    if [ ! -f "$2" ] || [ "$(sha256 "$2")" != "$3" ]; then
        rm -f "$2" "$2.part"
        curl --fail --silent --show-error --location --retry 5 --retry-all-errors \
            --retry-delay 5 --output "$2.part" "$1"
        mv "$2.part" "$2"
    fi
    check_sha256 "$2" "$3" "$(basename "$2")"
}
fetch "$SAIL_URL" "$cache/dl/sail.tar.gz" "$SAIL_TARBALL_SHA256"
fetch "$GCC_URL" "$cache/dl/gcc.tar.xz" "$GCC_TARBALL_SHA256"
fetch "$MISE_URL" "$cache/dl/mise" "$MISE_SHA256"

# The extracted trees are checked by their binaries; a mismatch re-extracts once.
unpack() { # <archive> <dir> <binary> <sha256> <tar flag>
    if [ ! -f "$2/$3" ] || [ "$(sha256 "$2/$3")" != "$4" ]; then
        rm -rf "$2"
        mkdir -p "$2"
        tar "$5" "$1" --directory="$2" --strip-components=1
    fi
    check_sha256 "$2/$3" "$4" "$3"
}
unpack "$cache/dl/sail.tar.gz" "$cache/sail" bin/sail_riscv_sim "$SAIL_BINARY_SHA256" -xzf
unpack "$cache/dl/gcc.tar.xz" "$cache/gcc" bin/riscv64-unknown-elf-gcc "$GCC_BINARY_SHA256" -xJf
check_sha256 "$cache/gcc/bin/riscv64-unknown-elf-as" "$AS_BINARY_SHA256" riscv64-unknown-elf-as
install -D -m755 "$cache/dl/mise" "$cache/bin/mise"

# ACT4, as a bare mirror of the pinned commit only.
if ! git -C "$cache/act4.git" cat-file -e "$ACT4_COMMIT^{commit}" 2>/dev/null; then
    rm -rf "$cache/act4.git"
    git init -q --bare "$cache/act4.git"
    git -C "$cache/act4.git" fetch -q --depth 1 "$ACT4_REPO" "$ACT4_COMMIT"
    git -C "$cache/act4.git" update-ref refs/heads/pinned "$ACT4_COMMIT"
fi
git -C "$cache/act4.git" cat-file -e "$ACT4_COMMIT^{commit}" ||
    die "the mirror in $cache/act4.git lacks $ACT4_COMMIT"

# The environment of every tool from here on: the pinned stack first on PATH, and every
# tool's state in the cache.
export PATH="$CANON/config/$CONFIG_NAME:$cache/gcc/bin:$cache/sail/bin:$cache/bin:$PATH"
export MISE_DATA_DIR="$cache/mise/data" MISE_CACHE_DIR="$cache/mise/cache"
export MISE_CONFIG_DIR="$cache/mise/config" MISE_STATE_DIR="$cache/mise/state"
export MISE_YES=1 MISE_TRUSTED_CONFIG_PATHS="$CANON/act4"
export BUNDLE_PATH="$cache/bundle" BUNDLE_FROZEN=true
export UV_CACHE_DIR="$cache/uv/cache" UV_PYTHON_INSTALL_DIR="$cache/uv/python" UV_FROZEN=1
export XDG_CACHE_HOME="$cache/xdg/cache" XDG_DATA_HOME="$cache/xdg/data"
unset VIRTUAL_ENV

[ "$(command -v riscv64-unknown-elf-gcc)" = "$cache/gcc/bin/riscv64-unknown-elf-gcc" ] ||
    die "riscv64-unknown-elf-gcc resolves to $(command -v riscv64-unknown-elf-gcc)"
[ "$(command -v sail_riscv_sim)" = "$cache/sail/bin/sail_riscv_sim" ] ||
    die "sail_riscv_sim resolves to $(command -v sail_riscv_sim)"
[ "$(first_line riscv64-unknown-elf-gcc --version)" = "$GCC_VERSION_LINE" ] ||
    die "gcc says $(first_line riscv64-unknown-elf-gcc --version)"
[ "$(first_line riscv64-unknown-elf-as --version)" = "$AS_VERSION_LINE" ] ||
    die "as says $(first_line riscv64-unknown-elf-as --version)"
[ "$(first_line sail_riscv_sim --version)" = "$SAIL_VERSION" ] ||
    die "sail_riscv_sim says $(first_line sail_riscv_sim --version)"
mise_version=$(first_line mise --version)
case "$mise_version" in "$MISE_VERSION_PREFIX"*) ;; *) die "mise says $mise_version" ;; esac

# 2. Generation, from scratch at the canonical path. -----------------------------------
rm -rf "$CANON"
mkdir -p "$CANON/config"
git init -q "$CANON/act4"
git -C "$CANON/act4" fetch -q --depth 1 "file://$cache/act4.git" "$ACT4_COMMIT"
git -C "$CANON/act4" -c advice.detachedHead=false checkout -q --detach FETCH_HEAD
[ "$(git -C "$CANON/act4" rev-parse HEAD)" = "$ACT4_COMMIT" ] || die "checkout is not $ACT4_COMMIT"
check_sha256 "$CANON/act4/testplans/I.csv" "$ACT4_TESTPLAN_SHA256" testplans/I.csv
check_sha256 "$CANON/act4/uv.lock" "$ACT4_UV_LOCK_SHA256" uv.lock
check_sha256 "$CANON/act4/framework/src/act/data/Gemfile.lock" "$ACT4_GEMFILE_LOCK_SHA256" Gemfile.lock
cp -R "$config_src" "$CANON/config/$CONFIG_NAME"
chmod 0755 "$CANON/config/$CONFIG_NAME/systemscope-act4-gcc"

cd "$CANON/act4"
# Only the tools ACT4 runs: Ruby with Bundler for UDB, and uv for the Python framework.
mise install ruby uv gem:bundler >"$CANON/mise-install.log" 2>&1 ||
    { tail -n 30 "$CANON/mise-install.log"; die "mise install failed"; }
ruby_version=$(first_line mise exec -- ruby --version)
case "$ruby_version" in "$RUBY_VERSION_PREFIX"*) ;; *) die "ruby says $ruby_version" ;; esac
uv_version=$(first_line mise exec -- uv --version)
case "$uv_version" in "$UV_VERSION_PREFIX"*) ;; *) die "uv says $uv_version" ;; esac

mise exec -- make elfs CONFIG_FILES="$CANON/config/$CONFIG_NAME/test_config.yaml" \
    EXTENSIONS="$EXTENSIONS" WORKDIR="$CANON/work" >"$CANON/make.log" 2>&1 ||
    { tail -n 60 "$CANON/make.log"; die "ACT4 make elfs failed"; }
python_version=$(first_line mise exec -- uv run python --version)

# 3. Checks. ----------------------------------------------------------------------------
[ -z "$(git -C "$CANON/act4" status --porcelain)" ] ||
    { git -C "$CANON/act4" status --short | head -n 20; die "ACT4 changed its own checkout"; }

work="$CANON/work/$CONFIG_NAME"
mapfile -t elfs < <(cd "$work/elfs" && find . -type f -name '*.elf' | sed 's|^\./||' | sort)
[ ${#elfs[@]} -gt 0 ] || die "ACT4 built no ELF"
stray=$(cd "$work/elfs" && find . -type f ! -name '*.elf' ! -name '*.elf.objdump' | head -n 5)
[ -z "$stray" ] || die "unexpected files in $work/elfs: $stray"

allowed=" $(echo $RV32I_MNEMONICS) "
tests=()
for rel in "${elfs[@]}"; do
    name=$(basename "$rel" .elf)
    dir=$(dirname "$rel")
    source="tests/$dir/$name.S"
    [ -f "$CANON/act4/$source" ] || die "$rel: no source $source"
    build="$work/build/$dir/$name"
    final="$work/elfs/$rel"
    # The Sail stage: the signature ELF, Sail's signature, and the results the final ELF
    # includes, each at least as new as the one before.
    for f in "$build.sig.elf" "$build.sig" "$build.results"; do
        [ -s "$f" ] || die "$name: missing or empty $f"
    done
    [ ! "$build.sig" -ot "$build.sig.elf" ] || die "$name: .sig is older than .sig.elf"
    [ ! "$build.results" -ot "$build.sig" ] || die "$name: .results is older than .sig"
    [ ! "$final" -ot "$build.results" ] || die "$name: the ELF is older than .results"
    # The instruction audit, over the executable sections only; `.word` and the like are
    # data the assembler marked with $d mapping symbols.
    mnemonics=$(riscv64-unknown-elf-objdump -d -M no-aliases "$final" | awk -F'\t' '
        NF >= 3 && $1 ~ /^ *[0-9a-f]+:$/ {
            enc = $2; gsub(/ /, "", enc); m = $3; sub(/ .*/, "", m)
            if (m ~ /^\.(word|short|byte|2byte|4byte)$/) next
            if (length(enc) != 8) { print "!" m "@" $1; next }
            print m
        }' | sort | uniq -c | awk '{ printf "%s%s=%s", sep, $2, $1; sep = "," }')
    [ -n "$mnemonics" ] || die "$name: objdump found no instruction"
    for pair in ${mnemonics//,/ }; do
        m=${pair%%=*}
        case "$allowed" in *" $m "*) ;; *) die "$name: instruction outside RV32I+ECALL: $pair" ;; esac
    done
    tests+=("test $name $source $(sha256 "$build.sig") $(sha256 "$build.results") $mnemonics")
done

# 4. Output. ----------------------------------------------------------------------------
rm -rf "$out/elfs" "$out/record.txt"
mkdir -p "$out/elfs"
for rel in "${elfs[@]}"; do
    install -m 0644 "$work/elfs/$rel" "$out/elfs/$(basename "$rel")"
done
{
    echo "schema 1"
    echo "act4-repository $ACT4_REPO"
    echo "act4-commit $(git -C "$CANON/act4" rev-parse HEAD)"
    echo "act4-checkout unchanged"
    echo "act4-testplan-sha256 $(sha256 "$CANON/act4/testplans/I.csv")"
    echo "act4-uv-lock-sha256 $(sha256 "$CANON/act4/uv.lock")"
    echo "act4-gemfile-lock-sha256 $(sha256 "$CANON/act4/framework/src/act/data/Gemfile.lock")"
    echo "extensions $EXTENSIONS"
    echo "canonical-path $CANON"
    echo "sail-version $(first_line sail_riscv_sim --version)"
    echo "sail-tarball-sha256 $(sha256 "$cache/dl/sail.tar.gz")"
    echo "sail-binary-sha256 $(sha256 "$cache/sail/bin/sail_riscv_sim")"
    echo "gcc-tarball-sha256 $(sha256 "$cache/dl/gcc.tar.xz")"
    echo "gcc-binary-sha256 $(sha256 "$cache/gcc/bin/riscv64-unknown-elf-gcc")"
    echo "as-binary-sha256 $(sha256 "$cache/gcc/bin/riscv64-unknown-elf-as")"
    echo "gcc-version $(first_line riscv64-unknown-elf-gcc --version)"
    echo "as-version $(first_line riscv64-unknown-elf-as --version)"
    echo "mise-sha256 $(sha256 "$cache/bin/mise")"
    echo "ruby-version ${ruby_version%% (*}"
    echo "uv-version ${uv_version%% (*}"
    echo "python-version $python_version"
    printf '%s\n' "${tests[@]}"
} >"$out/record.txt"
echo "built ${#elfs[@]} ACT4 ELFs from $ACT4_COMMIT into $out"
