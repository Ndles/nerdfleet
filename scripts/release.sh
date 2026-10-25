#!/bin/sh
# Builds the release binary in a Rocky Linux 8 container (glibc 2.28), so it
# runs on old cluster nodes as well as current distributions, and writes it
# with its SHA-256 to dist/.
#
#   scripts/release.sh              # needs Docker, and nerdmux next to this repo
#   NERDMUX_DIR=/path/to/nerdmux scripts/release.sh
#
# nerdmux is built from that checkout, which must be at the commit
# Cargo.toml pins, so the container needs no network or access to the
# private repository.
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
out="$root/target/rocky8"
# The newest glibc symbol version the binary may need.
max_glibc=2.28

docker build -q -t "$image" -f "$root/build/rocky8.Dockerfile" "$root/build" >/dev/null
mkdir -p "$out/cargo-home" "$root/dist"

# As the calling user, so nothing in the tree ends up owned by root. The
# nerdmux checkout is mounted read-only.
docker run --rm \
    --user "$(id -u):$(id -g)" \
    -e HOME=/tmp \
    -e CARGO_HOME=/src/nerdfleet/target/rocky8/cargo-home \
    -e CARGO_TARGET_DIR=/src/nerdfleet/target/rocky8 \
    -v "$root:/src/nerdfleet" \
    -v "$nerdmux:/src/nerdmux:ro" \
    -w /src/nerdfleet \
    "$image" cargo build --release --locked \
        --config "patch.\"$url\".nerdmux.path=\"/src/nerdmux\""

bin="$out/release/nerdfleet"
need=$(objdump -T "$bin" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)
if [ "$(printf '%s\n%s\n' "$need" "$max_glibc" | sort -V | tail -1)" != "$max_glibc" ]; then
    echo "release: the binary needs glibc $need, more than $max_glibc" >&2
    exit 1
fi

name="nerdfleet-$version-x86_64-linux"
cp "$bin" "$root/dist/$name"
(cd "$root/dist" && sha256sum "$name" > "$name.sha256")
echo "dist/$name (needs glibc $need or newer)"
cat "$root/dist/$name.sha256"
