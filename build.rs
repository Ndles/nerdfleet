//! Builds agents for other hosts into the viewer.
//!
//! `NERDFLEET_AGENTS` names a directory of agent builds, one per target, as
//! `nerdfleet-<version>-<target>` (`scripts/release.sh` makes them). Each one
//! of this version is embedded, and the viewer deploys it to hosts of that
//! target. Unset, nothing is embedded and the viewer deploys itself, to
//! hosts like the machine it runs on.

use std::path::Path;

/// The targets agents are built for, with the ELF machine number their
/// files must have.
const TARGETS: &[(&str, u16)] = &[("x86_64-linux", 0x3E), ("aarch64-linux", 0xB7)];

fn main() {
    println!("cargo:rerun-if-env-changed=NERDFLEET_AGENTS");
    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join("agents.rs");
    let mut entries = String::new();
    if let Some(dir) = std::env::var_os("NERDFLEET_AGENTS").filter(|d| !d.is_empty()) {
        let dir = Path::new(&dir);
        println!("cargo:rerun-if-changed={}", dir.display());
        let prefix = format!("nerdfleet-{}-", std::env::var("CARGO_PKG_VERSION").unwrap());
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("NERDFLEET_AGENTS={}: {e}", dir.display()))
            .map(|e| e.unwrap().path())
            .collect();
        files.sort();
        for path in files {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let Some(target) = name.strip_prefix(&prefix) else {
                if name.starts_with("nerdfleet-") {
                    println!("cargo:warning=skipping {name}: not version {}", &prefix[10..prefix.len() - 1]);
                }
                continue;
            };
            let Some(&(_, machine)) = TARGETS.iter().find(|(t, _)| *t == target) else {
                panic!("{}: no such agent target {target:?}", path.display());
            };
            let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            // A 64-bit little-endian ELF file for the target's machine, so a
            // mislabelled build is caught here and not on a host.
            let elf = bytes.len() > 20 && bytes.starts_with(b"\x7fELF\x02\x01");
            if !elf || u16::from_le_bytes([bytes[18], bytes[19]]) != machine {
                panic!("{}: not a 64-bit ELF program for {target}", path.display());
            }
            println!("cargo:rerun-if-changed={}", path.display());
            let path = path.canonicalize().unwrap();
            entries.push_str(&format!("    ({target:?}, include_bytes!({:?})),\n", path.display().to_string()));
        }
        if entries.is_empty() {
            panic!("NERDFLEET_AGENTS={} has no {prefix}<target> builds", dir.display());
        }
    }
    std::fs::write(out, format!("pub static AGENTS: &[(&str, &[u8])] = &[\n{entries}];\n")).unwrap();
}
