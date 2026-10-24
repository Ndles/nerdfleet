# The release build environment: Rocky Linux 8, glibc 2.28, so the binary
# runs on hosts as old as RHEL 8 and still loads the NVIDIA driver's NVML.
#
# The base image is pinned by digest, and the Rust toolchain by version.
# rustup-init is checked against the SHA-256 that static.rust-lang.org
# publishes beside it, which proves an intact download, not authorship;
# rustup then checks every toolchain component against the release
# manifest's hashes.

FROM docker.io/rockylinux/rockylinux:8@sha256:e8a49c5403b687db05d4d67333fa45808fbe74f36e683cec7abb1f7d0f2338c6

RUN dnf install -y --setopt=install_weak_deps=False gcc binutils && dnf clean all

ARG RUST_VERSION=1.92.0
ENV RUSTUP_HOME=/opt/rustup PATH=/opt/cargo/bin:$PATH
RUN set -eu; \
    url=https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init; \
    curl --proto '=https' --tlsv1.2 -sSfo /tmp/rustup-init "$url"; \
    curl --proto '=https' --tlsv1.2 -sSfo /tmp/rustup-init.sha256 "$url.sha256"; \
    echo "$(cut -d' ' -f1 /tmp/rustup-init.sha256)  /tmp/rustup-init" | sha256sum -c -; \
    chmod +x /tmp/rustup-init; \
    CARGO_HOME=/opt/cargo /tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain "$RUST_VERSION"; \
    rm /tmp/rustup-init /tmp/rustup-init.sha256; \
    chmod -R a+rX /opt/rustup /opt/cargo
