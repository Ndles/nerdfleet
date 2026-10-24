//! Device names from the system's copy of the PCI ID database, so a GPU
//! the driver cannot name (or that is asleep) still gets a real name.

const PATHS: &[&str] = &["/usr/share/hwdata/pci.ids", "/usr/share/misc/pci.ids", "/usr/share/pci.ids"];

pub fn device_name(vendor: u16, device: u16) -> Option<String> {
    let text = PATHS.iter().find_map(|p| std::fs::read_to_string(p).ok())?;
    lookup(&text, vendor, device)
}

fn lookup(text: &str, vendor: u16, device: u16) -> Option<String> {
    let vendor = format!("{vendor:04x}  ");
    let device = format!("\t{device:04x}  ");
    let mut in_vendor = false;
    for line in text.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if !line.starts_with('\t') {
            // A vendor line. Past ours, the device is not listed.
            if in_vendor {
                return None;
            }
            in_vendor = line.starts_with(&vendor);
        } else if in_vendor && line.starts_with(&device) {
            return Some(line[device.len()..].trim().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::lookup;

    const IDS: &str = "\
# comment
1002  Advanced Micro Devices, Inc. [AMD/ATI]
\t1586  Strix Halo [Radeon Graphics / Radeon 8050S Graphics / Radeon 8060S Graphics]
\t\t1002 1586  subsystem
10de  NVIDIA Corporation
\t2bb4  GB203GL [RTX PRO 4500 Blackwell]
";

    #[test]
    fn finds_devices_under_their_vendor() {
        assert_eq!(lookup(IDS, 0x10de, 0x2bb4).as_deref(), Some("GB203GL [RTX PRO 4500 Blackwell]"));
        assert!(lookup(IDS, 0x1002, 0x1586).unwrap().starts_with("Strix Halo"));
        assert_eq!(lookup(IDS, 0x1002, 0x2bb4), None);
        assert_eq!(lookup(IDS, 0x8086, 0x1234), None);
    }
}
