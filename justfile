# nerdfleet: `just` lists the recipes.

bin := justfile_directory() / "target/release/nerdfleet"

_default:
    @just --list --unsorted

# Build the release binary
build:
    cargo build --release

# The overview, in this terminal (args go to nerdfleet: `just run probe gpu-box`)
run *args: build
    {{bin}} {{args}}

# The overview in a window of your nerdmux session (the current one, or the last used); `fleet` again goes back to it
fleet: build
    #!/bin/sh
    set -eu
    if [ -z "${NERDMUX:-}" ] && ! nerdmux ls 2>/dev/null | grep -q ':'; then
        exec nerdmux new -- {{bin}}
    fi
    if nerdmux new-window -n nerdfleet --reuse -- {{bin}}; then
        [ -n "${NERDMUX:-}" ] || exec nerdmux attach
    else
        echo "nerdmux has no new-window yet: reinstall it and restart its server. Running here instead." >&2
        exec {{bin}}
    fi

# What is wrong with the config, if anything
check: build
    {{bin}} check

# Connect to one host and print what it reports
probe host: build
    {{bin}} probe {{host}}

# Resident agent on a host: enable, status or disable
resident action host *flags: build
    {{bin}} resident {{action}} {{host}} {{flags}}

# Open the config file in $EDITOR
config:
    ${EDITOR:-vi} ~/.config/nerdfleet/config.toml

# Run the tests
test:
    cargo test

# Formatting, then clippy with warnings as errors
lint:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings

# Format the code
fmt:
    cargo fmt

# Put `nerdfleet` in ~/.cargo/bin
install:
    cargo install --path . --locked

# Release viewers for x86_64 and aarch64 Linux, carrying agents for both, in dist/ (needs Docker, zig, cargo-zigbuild)
release:
    scripts/release.sh
