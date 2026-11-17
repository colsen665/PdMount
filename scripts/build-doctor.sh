#!/usr/bin/env bash
# Checks what building PdMount needs, before `cargo build`, and says what to install when a
# check fails (Debian/Ubuntu package names; the same tools on other systems).
#
# Usage: scripts/build-doctor.sh [--no-network]
#   --no-network   skip the two downloads the build makes (the SDK fork over git, Go modules)
#
# Exit 0 when every check passes, 1 otherwise. If all pass and the build still fails, the first
# `error:` block of `cargo build` (and, for gopenpgp-sys, the "Failed to compile go library" part)
# tells the rest.
set -uo pipefail

# Rust: the highest `rust-version` among the locked dependencies (uuid 1.28 needs 1.89);
# the workspace uses edition 2024 (1.85). Raise it with Cargo.lock updates.
min_rust=1.89.0
# Go: gopenpgp-sys's go.mod (`go 1.23.0`).
min_go=1.23.0
# protoc: prost-build needs proto3 `optional` without the experimental flag (3.15).
min_protoc=3.15.0

network=1
[[ ${1:-} == --no-network ]] && network=0

root=$(cd "$(dirname "$0")/.." && pwd)
fails=0
warns=0

ok() { printf '[ok  ] %-14s %s\n' "$1" "$2"; }
warn() { printf '[WARN] %-14s %s\n' "$1" "$2"; warns=$((warns + 1)); }
fail() { printf '[FAIL] %-14s %s\n' "$1" "$2"; fails=$((fails + 1)); }
have() { command -v "$1" >/dev/null 2>&1; }

# version_ge A B: A >= B for dotted versions.
version_ge() { [[ $(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n 1) == "$2" ]]; }

# --- Rust
if have cargo && have rustc; then
    rust=$(rustc --version | awk '{print $2}')
    where=$(command -v rustc)
    if version_ge "${rust%%-*}" "$min_rust"; then
        ok rust "rustc $rust ($where)"
    elif have rustup; then
        fail rust "rustc $rust is older than $min_rust: rustup update stable && rustup default stable"
    else
        fail rust "rustc $rust ($where) is older than $min_rust; the distribution's rustc is too old: install rustup (https://rustup.rs), then rustup default stable"
    fi
    # A distribution rustc earlier on PATH than rustup's shadows it.
    if have rustup && [[ $where != "${CARGO_HOME:-$HOME/.cargo}/bin/rustc" ]]; then
        warn rust-path "rustc on PATH is $where, not rustup's; put ${CARGO_HOME:-\$HOME/.cargo}/bin first in PATH" # betterleaks:allow
    fi
else
    fail rust "cargo/rustc not found: install rustup (https://rustup.rs), then rustup default stable"
fi

# --- C toolchain (cgo, cc crate)
if have cc || have gcc; then
    ok c-compiler "$(command -v cc || command -v gcc)"
else
    fail c-compiler "no cc/gcc: apt install build-essential"
fi

# --- Go (gopenpgp-sys builds a Go library with cgo)
go_bin=${GO:-go}
if have "$go_bin"; then
    go_version=$("$go_bin" env GOVERSION 2>/dev/null)
    go_version=${go_version#go}
    if [[ -z $go_version ]]; then
        fail go "$("$go_bin" version 2>&1): too old to report GOVERSION; need $min_go or newer"
    elif version_ge "$go_version" "$min_go"; then
        ok go "go $go_version ($(command -v "$go_bin"))"
    else
        fail go "go $go_version is older than $min_go: Debian 12's golang is too old; bookworm-backports (golang-1.23 or newer, then put /usr/lib/go-1.2x/bin first in PATH) or the go.dev tarball"
    fi
    [[ $("$go_bin" env CGO_ENABLED 2>/dev/null) == 1 ]] ||
        fail cgo "CGO_ENABLED is not 1 (go env CGO_ENABLED): unset CGO_ENABLED, check that a C compiler is installed"
else
    fail go "go not found: Go $min_go or newer (Debian 12: bookworm-backports or the go.dev tarball; Debian 13: apt install golang)"
fi

# --- clang and libclang (bindgen in gopenpgp-sys)
if have clang; then
    ok clang "$(clang --version | head -n 1)"
else
    fail clang "clang not found: apt install clang"
fi
# bindgen loads libclang.so or libclang-<version>.so, never libclang-cpp.so.
find_libclang() { # dir
    compgen -G "$1/libclang.so*" | head -n 1
    compgen -G "$1/libclang-[0-9]*.so*" | head -n 1
}
libclang=""
if [[ -n ${LIBCLANG_PATH:-} ]]; then
    libclang=$(find_libclang "$LIBCLANG_PATH" | head -n 1)
fi
if [[ -z $libclang ]] && have llvm-config; then
    libclang=$(find_libclang "$(llvm-config --libdir)" | head -n 1)
fi
if [[ -z $libclang ]] && have ldconfig; then
    libclang=$(ldconfig -p 2>/dev/null | awk '$1 ~ /^libclang(-[0-9.]+)?\.so/ {print $NF; exit}')
fi
if [[ -z $libclang ]]; then
    for dir in /usr/lib/llvm-*/lib /usr/lib/llvm/*/lib64; do
        libclang=$(find_libclang "$dir" | head -n 1)
        [[ -n $libclang ]] && break
    done
fi
if [[ -n $libclang ]]; then
    ok libclang "$libclang"
else
    fail libclang "libclang.so not found (bindgen needs it): apt install libclang-dev, or set LIBCLANG_PATH to its directory"
fi

# --- protoc
if have protoc; then
    protoc_version=$(protoc --version | awk '{print $2}')
    if version_ge "$protoc_version" "$min_protoc"; then
        ok protoc "protoc $protoc_version"
    else
        fail protoc "protoc $protoc_version is older than $min_protoc"
    fi
else
    fail protoc "protoc not found: apt install protobuf-compiler"
fi

# --- pkg-config and libdbus (keyring, logind)
if have pkg-config; then
    ok pkg-config "$(command -v pkg-config)"
    if pkg-config --exists dbus-1; then
        ok libdbus "dbus-1 $(pkg-config --modversion dbus-1)"
    else
        fail libdbus "pkg-config finds no dbus-1: apt install libdbus-1-dev"
    fi
else
    fail pkg-config "pkg-config not found: apt install pkg-config"
fi

# --- git (the SDK fork is a git dependency)
have git && ok git "$(git --version)" || fail git "git not found: apt install git"

# --- FUSE (running, not building)
if have fusermount3; then
    ok fusermount3 "$(command -v fusermount3)"
else
    warn fusermount3 "not found (needed to mount, not to build): apt install fuse3"
fi
[[ -c /dev/fuse ]] || warn fuse-device "/dev/fuse missing (needed to mount): modprobe fuse"

# --- network: the build downloads the SDK fork and the Go modules
if ((network)); then
    fork=$(sed -n '/^\[patch.crates-io\]/,/^\[/{s/.*git *= *"\([^"]*\)".*/\1/p}' "$root/Cargo.toml" | head -n 1)
    if [[ -z $fork ]]; then
        warn sdk-fork "no git URL under [patch.crates-io] in Cargo.toml"
    elif have git && timeout 20 git ls-remote --heads "$fork" >/dev/null 2>&1; then
        ok sdk-fork "reachable: $fork"
    else
        fail sdk-fork "cannot reach $fork (git ls-remote): network, proxy or CARGO_NET_GIT_FETCH_WITH_CLI"
    fi
    if have "$go_bin"; then
        proxy=$("$go_bin" env GOPROXY 2>/dev/null)
        first=${proxy%%,*}
        if [[ $first == off ]]; then
            fail go-modules "GOPROXY=off: the Go modules of gopenpgp-sys cannot be downloaded"
        elif [[ $first == direct || -z $first ]]; then
            ok go-modules "GOPROXY=$proxy (modules come from their repositories)"
        elif have curl; then
            if curl -fsS --max-time 15 -o /dev/null "${first%/}/github.com/!proton!mail/gopenpgp/v3/@v/list"; then
                ok go-modules "reachable: $first"
            else
                fail go-modules "cannot reach $first (GOPROXY): network or proxy settings"
            fi
        else
            warn go-modules "curl not found; not checked whether $first is reachable"
        fi
    fi
fi

# --- disk space for target/ (a release build with the Go cache takes a few GiB)
avail=$(df -Pk "$root" 2>/dev/null | awk 'NR == 2 {print $4}')
if [[ -n $avail ]] && ((avail < 4 * 1024 * 1024)); then
    warn disk "$((avail / 1024)) MiB free under $root; a build needs about 4 GiB"
fi

echo
if ((fails)); then
    echo "$fails check(s) failed, $warns warning(s)."
    exit 1
fi
echo "All checks passed ($warns warning(s)). If the build still fails, keep the first 'error:' block of cargo build."
