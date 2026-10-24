#!/bin/sh
# Builds the release binary in a Rocky Linux 8 container (glibc 2.28), so it
# runs on old cluster nodes as well as current distributions, and writes it
# with its SHA-256 to dist/.
#
#   scripts/release.sh              # needs Docker, and nerdmux next to this repo
#   NERDMUX_DIR=/path/to/nerdmux scripts/release.sh
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
nerdmux=$(cd "${NERDMUX_DIR:-$root/../nerdmux}" && pwd)
image=nerdfleet-build:rocky8
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
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
    "$image" cargo build --release --locked

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
