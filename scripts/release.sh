#!/bin/sh
# Builds release viewers for x86_64 and aarch64 Linux, each carrying the
# agents for both, and writes them with their SHA-256 to dist/. Every build
# needs glibc 2.28 at most, so it runs on old cluster nodes as well as
# current distributions.
#
#   scripts/release.sh              # needs Docker, zig, cargo-zigbuild, and nerdmux next to this repo
#   NERDMUX_DIR=/path/to/nerdmux scripts/release.sh
#
# Two stages, because the agent is the same program as the viewer:
#   1. agents: plain builds for each target, in target/agents/<version>/;
#   2. viewers: the same builds with those agents inside (see build.rs).
# x86_64 builds in a Rocky Linux 8 container with a pinned toolchain;
# aarch64 cross-builds with cargo-zigbuild against glibc 2.28.
#
# nerdmux is built from that checkout, which must be at the commit
# Cargo.toml pins, so no build needs network or access to the private
# repository.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
nerdmux=$(cd "${NERDMUX_DIR:-$root/../nerdmux}" && pwd)
image=nerdfleet-build:rocky8
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
url=$(sed -n 's/^nerdmux = { git = "\([^"]*\)".*/\1/p' "$root/Cargo.toml")
rev=$(sed -n 's/^nerdmux = { git = .*rev = "\([0-9a-f]*\)".*/\1/p' "$root/Cargo.toml")
if [ -z "$url" ] || [ -z "$rev" ]; then
    echo "release: no nerdmux git dependency with a rev in Cargo.toml" >&2
    exit 1
fi
have=$(git -C "$nerdmux" rev-parse HEAD)
if [ "$have" != "$rev" ] || [ -n "$(git -C "$nerdmux" status --porcelain --untracked-files=no)" ]; then
    echo "release: $nerdmux must be at $rev, the commit Cargo.toml pins, with no changes (it is at $have)" >&2
    exit 1
fi
for tool in docker zig cargo-zigbuild readelf; do
    command -v "$tool" >/dev/null || { echo "release: $tool is needed" >&2; exit 1; }
done
# The newest glibc symbol version a binary may need.
max_glibc=2.28
agents="$root/target/agents/$version"

docker build -q -t "$image" -f "$root/build/rocky8.Dockerfile" "$root/build" >/dev/null
mkdir -p "$root/target/rocky8/cargo-home" "$root/dist"

# x86_64 in the container, as the calling user, so nothing in the tree ends
# up owned by root. The nerdmux checkout is mounted read-only. $1 is the
# agents directory to build in, as seen inside, or empty for none.
build_x86_64() {
    docker run --rm \
        --user "$(id -u):$(id -g)" \
        -e HOME=/tmp \
        -e CARGO_HOME=/src/nerdfleet/target/rocky8/cargo-home \
        -e CARGO_TARGET_DIR=/src/nerdfleet/target/rocky8 \
        -e NERDFLEET_AGENTS="$1" \
        -v "$root:/src/nerdfleet" \
        -v "$nerdmux:/src/nerdmux:ro" \
        -w /src/nerdfleet \
        "$image" cargo build --release --locked \
            --config "patch.\"$url\".nerdmux.path=\"/src/nerdmux\"" >&2 &&
        echo "$root/target/rocky8/release/nerdfleet"
}

# aarch64 here, with zig as the linker. $1 as above.
build_aarch64() {
    (cd "$root" && NERDFLEET_AGENTS="$1" cargo zigbuild --release --locked --quiet \
        --target aarch64-unknown-linux-gnu.2.28 \
        --config "patch.\"$url\".nerdmux.path=\"$nerdmux\"" >&2) &&
        echo "$root/target/aarch64-unknown-linux-gnu/release/nerdfleet"
}

# Refuses a binary that needs a newer glibc than $max_glibc; prints what
# it needs.
check_glibc() {
    need=$(readelf -V "$1" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)
    if [ "$(printf '%s\n%s\n' "$need" "$max_glibc" | sort -V | tail -1)" != "$max_glibc" ]; then
        echo "release: $1 needs glibc $need, more than $max_glibc" >&2
        exit 1
    fi
    echo "$need"
}

# 1. Agents.
rm -rf "$agents"
mkdir -p "$agents"
bin=$(build_x86_64 "")
check_glibc "$bin" >/dev/null
cp "$bin" "$agents/nerdfleet-$version-x86_64-linux"
bin=$(build_aarch64 "")
check_glibc "$bin" >/dev/null
cp "$bin" "$agents/nerdfleet-$version-aarch64-linux"

# 2. Viewers, carrying the agents.
for target in x86_64 aarch64; do
    if [ "$target" = x86_64 ]; then
        bin=$(build_x86_64 "/src/nerdfleet/target/agents/$version")
    else
        bin=$(build_aarch64 "$agents")
    fi
    need=$(check_glibc "$bin")
    name="nerdfleet-$version-$target-linux"
    cp "$bin" "$root/dist/$name"
    (cd "$root/dist" && sha256sum "$name" > "$name.sha256")
    echo "dist/$name (needs glibc $need or newer)"
    cat "$root/dist/$name.sha256"
done
echo "Each carries agents for: $(ls "$agents" | sed "s/^nerdfleet-$version-//" | tr '\n' ' ')"
