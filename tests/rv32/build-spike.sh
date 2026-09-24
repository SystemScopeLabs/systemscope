#!/usr/bin/env bash
# Builds the pinned Spike for the M1-A3 differential (docs/m1-design.md §10.3). Linux
# only; needs git, a C++ compiler, make, and dtc (Spike's configure requires it, though
# the differential runs Spike with --disable-dtb and never calls it).
#
# Run it through `cargo xtask spike build`. Tests never run this script, and the normal
# test suite never needs Spike: only `cargo xtask spike verify` and `spike diff` do.
#
# Usage: build-spike.sh <dir>
#
# 1. Fetches riscv-isa-sim at the pinned commit into <dir>/src (network access happens
#    only here) and checks the checkout.
# 2. Configures and builds it out of tree, and installs it into <dir> (<dir>/bin/spike).
# 3. Writes <dir>/spike.stamp with the commit and Spike's version line, which
#    `cargo xtask spike verify` checks along with the checkout itself.

set -euo pipefail
export LC_ALL=C

# Pins. The systemscope-rv32 crate checks that these match its own constants.
SPIKE_REPO=https://github.com/riscv-software-src/riscv-isa-sim.git
SPIKE_COMMIT=19609434bb3d83448eec8796e8f0367c868efbda

die() {
    echo "build-spike: $*" >&2
    exit 1
}

[ $# -eq 1 ] || die "usage: build-spike.sh <dir>"
dir=$(mkdir -p "$1" && cd "$1" && pwd)
src="$dir/src"
build="$dir/build"

# 1. Sources at the pinned commit.
rm -rf "$src" "$build" "$dir/bin" "$dir/lib" "$dir/include" "$dir/share" "$dir/spike.stamp"
git init -q "$src"
git -C "$src" fetch -q --depth 1 "$SPIKE_REPO" "$SPIKE_COMMIT"
git -C "$src" -c advice.detachedHead=false checkout -q --detach FETCH_HEAD
[ "$(git -C "$src" rev-parse HEAD)" = "$SPIKE_COMMIT" ] ||
    die "fetched $(git -C "$src" rev-parse HEAD), not $SPIKE_COMMIT"

# 2. Build and install. Boost only serves Spike's socket and debug features, which the
# differential does not use.
mkdir "$build"
cd "$build"
"$src/configure" --prefix="$dir" --without-boost --without-boost-asio --without-boost-regex \
    >configure.log 2>&1 || { tail -n 30 configure.log; die "configure failed"; }
make -j"$(nproc)" >make.log 2>&1 || { tail -n 50 make.log; die "make failed"; }
make install >install.log 2>&1 || { tail -n 30 install.log; die "make install failed"; }
cd "$dir"
rm -rf "$build"

# 3. The stamp.
version=$("$dir/bin/spike" --help 2>&1 | head -n1 || true)
printf 'commit %s\nversion %s\n' "$SPIKE_COMMIT" "$version" >"$dir/spike.stamp"
echo "built Spike $SPIKE_COMMIT into $dir: $version"
