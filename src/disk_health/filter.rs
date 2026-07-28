// src/disk_health/filter.rs

/// Device name prefixes that never correspond to real physical media.
const VIRTUAL_DEVICE_PREFIXES: &[&str] = &[
    "loop", "ram", "zram", "dm-", "md", "sr", "fd", "vd", "xvd", "zd",
];

/// Whether a scanned device path or name looks like real physical media.
pub fn is_physical_device(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    if base.is_empty() {
        return false;
    }
    if base.starts_with("nvme") {
        return true;
    }
    !VIRTUAL_DEVICE_PREFIXES.iter().any(|p| base.starts_with(p))
}

/// Returns true when the network interface name looks like a physical NIC.
pub fn is_physical_interface(name: &str) -> bool {
    const LOOPBACK_EXACT: &[&str] = &["lo", "Loopback Pseudo-Interface 1"];
    if LOOPBACK_EXACT.iter().any(|&s| name == s) {
        return false;
    }
    const VIRTUAL_PREFIXES: &[&str] = &[
        "veth", "docker", "br-", "virbr", "vmnet", "vnet",
        "tun", "tap", "wg", "utun", "llw", "isatap", "teredo", "6to4",
    ];
    !VIRTUAL_PREFIXES.iter().any(|&p| name.to_lowercase().starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_nvme_devices() {
        assert!(is_physical_device("/dev/nvme0"));
        assert!(is_physical_device("/dev/nvme0n1"));
    }

    #[test]
    fn keeps_sata_disks() {
        assert!(is_physical_device("/dev/sda"));
        assert!(is_physical_device("/dev/sdb"));
    }

    #[test]
    fn drops_virtual_block_devices() {
        assert!(!is_physical_device("/dev/loop0"));
        assert!(!is_physical_device("/dev/dm-0"));
        assert!(!is_physical_device("/dev/md0"));
        assert!(!is_physical_device("/dev/vda"));
        assert!(!is_physical_device("/dev/xvda"));
        assert!(!is_physical_device("/dev/zram0"));
        assert!(!is_physical_device("/dev/sr0"));
        assert!(!is_physical_device("/dev/zd16"));
    }

    #[test]
    fn drops_loopback_exact_match() {
        assert!(!is_physical_interface("lo"));
        assert!(!is_physical_interface("Loopback Pseudo-Interface 1"));
    }

    #[test]
    fn drops_virtual_nic_prefixes() {
        assert!(!is_physical_interface("veth0"));
        assert!(!is_physical_interface("docker0"));
        assert!(!is_physical_interface("br-abc123"));
        assert!(!is_physical_interface("virbr0"));
        assert!(!is_physical_interface("tun0"));
        assert!(!is_physical_interface("wg0"));
        assert!(!is_physical_interface("utun2"));
        assert!(!is_physical_interface("ISATAP"));   // case-insensitive
        assert!(!is_physical_interface("Teredo"));
    }

    #[test]
    fn keeps_physical_nics() {
        assert!(is_physical_interface("eth0"));
        assert!(is_physical_interface("enp3s0"));
        assert!(is_physical_interface("wlan0"));
        assert!(is_physical_interface("Wi-Fi"));
        assert!(is_physical_interface("以太网"));
    }
}
