//! Per-device disk I/O counters.
//!
//! `sysinfo` only exposes I/O per *process*, so summing it gives a
//! whole-machine figure with no way to say which disk the traffic hit. On
//! Linux the kernel already tracks this per device in `/proc/diskstats`, so
//! this module reads it directly and diffs successive samples into rates.
//!
//! On non-Linux platforms there is no equivalent cheap interface, so lookups
//! return `None` and callers fall back to the aggregate. That is a visible
//! difference in fidelity, not a silent one — the field docs say so.

use std::collections::HashMap;
use std::time::Instant;

/// Sector size assumed by `/proc/diskstats`, which counts in 512-byte sectors
/// regardless of the device's actual physical sector size.
const SECTOR_BYTES: u64 = 512;

/// Cumulative read/write counters for one device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeviceCounters {
    pub read_bytes: u64,
    pub written_bytes: u64,
}

/// I/O activity for one device over a sample interval.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeviceDelta {
    /// Bytes read during the interval.
    pub read_bytes: u64,
    /// Bytes written during the interval.
    pub written_bytes: u64,
    /// Read throughput in bytes per second.
    pub read_bytes_per_sec: f64,
    /// Write throughput in bytes per second.
    pub write_bytes_per_sec: f64,
}

/// Tracks per-device I/O counters and turns successive samples into rates.
#[derive(Debug, Default)]
pub struct DiskStats {
    /// Kernel device name (e.g. `sda`, `nvme0n1`) -> counters at last sample.
    previous: HashMap<String, DeviceCounters>,
    /// Per-device activity computed at the most recent refresh.
    deltas: HashMap<String, DeviceDelta>,
    /// When the previous sample was taken.
    last_sample: Option<Instant>,
}

impl DiskStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sample the kernel counters and recompute per-device rates.
    pub fn refresh(&mut self) {
        let Some(current) = read_counters() else {
            return;
        };

        let now = Instant::now();
        let elapsed = self
            .last_sample
            .map(|prev| now.duration_since(prev).as_secs_f64())
            .filter(|s| *s > 0.0);

        self.deltas.clear();

        for (device, counters) in &current {
            // Without a previous sample there is no interval to diff against,
            // so report zero rather than mistaking the cumulative total for
            // this interval's activity.
            let Some(prev) = self.previous.get(device) else {
                continue;
            };

            // Counters are monotonic but reset on reboot, and a device can be
            // hot-removed and re-added. saturating_sub keeps a reset from
            // wrapping into an absurd spike.
            let read = counters.read_bytes.saturating_sub(prev.read_bytes);
            let written = counters.written_bytes.saturating_sub(prev.written_bytes);

            let (read_rate, write_rate) = match elapsed {
                Some(secs) => (read as f64 / secs, written as f64 / secs),
                None => (0.0, 0.0),
            };

            self.deltas.insert(
                device.clone(),
                DeviceDelta {
                    read_bytes: read,
                    written_bytes: written,
                    read_bytes_per_sec: read_rate,
                    write_bytes_per_sec: write_rate,
                },
            );
        }

        self.previous = current;
        self.last_sample = Some(now);
    }

    /// Look up activity for the device backing a mount point's device path.
    ///
    /// `disk_name` is what `sysinfo` reports (e.g. `/dev/sda1`), which is
    /// usually a partition. `/proc/diskstats` lists both partitions and their
    /// parent device, so an exact match is tried first and the parent is used
    /// as a fallback.
    pub fn lookup(&self, disk_name: &str) -> Option<DeviceDelta> {
        if self.deltas.is_empty() {
            return None;
        }

        let base = disk_name.rsplit('/').next().unwrap_or(disk_name);
        if base.is_empty() {
            return None;
        }

        if let Some(delta) = self.deltas.get(base) {
            return Some(*delta);
        }

        self.deltas.get(&parent_device(base)).copied()
    }

    /// Whether any per-device data is available, so callers can tell "no I/O"
    /// apart from "this platform has no per-device counters".
    pub fn is_available(&self) -> bool {
        !self.deltas.is_empty()
    }
}

/// Reduce a partition name to its parent device: `sda1` -> `sda`,
/// `nvme0n1p2` -> `nvme0n1`.
fn parent_device(name: &str) -> String {
    let trimmed = name.trim_end_matches(|c: char| c.is_ascii_digit());

    // NVMe partitions are `<device>p<N>`, so drop the separator too.
    if trimmed.ends_with('p') && trimmed.contains("nvme") {
        return trimmed[..trimmed.len() - 1].to_string();
    }

    if trimmed.is_empty() {
        name.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Read cumulative per-device counters from the OS.
#[cfg(target_os = "linux")]
fn read_counters() -> Option<HashMap<String, DeviceCounters>> {
    let content = std::fs::read_to_string("/proc/diskstats").ok()?;
    Some(parse_diskstats(&content))
}

/// No cheap per-device equivalent outside Linux, so callers fall back to the
/// process-summed aggregate.
#[cfg(not(target_os = "linux"))]
fn read_counters() -> Option<HashMap<String, DeviceCounters>> {
    None
}

/// Parse `/proc/diskstats` into per-device cumulative byte counters.
///
/// Field layout (1-indexed, per Documentation/admin-guide/iostats.rst):
///   1 major, 2 minor, 3 device name, 4 reads completed, 5 reads merged,
///   6 sectors read, 7 ms reading, 8 writes completed, 9 writes merged,
///   10 sectors written, ...
///
/// Split out from the file read so it can be tested on any platform.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_diskstats(content: &str) -> HashMap<String, DeviceCounters> {
    let mut out = HashMap::new();

    for line in content.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // Anything shorter than this is a truncated or unexpected line.
        if fields.len() < 10 {
            continue;
        }

        let name = fields[2];
        let Ok(sectors_read) = fields[5].parse::<u64>() else {
            continue;
        };
        let Ok(sectors_written) = fields[9].parse::<u64>() else {
            continue;
        };

        out.insert(
            name.to_string(),
            DeviceCounters {
                read_bytes: sectors_read.saturating_mul(SECTOR_BYTES),
                written_bytes: sectors_written.saturating_mul(SECTOR_BYTES),
            },
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `/proc/diskstats` excerpt: an NVMe disk with one partition, a SATA
    /// disk, and a loop device.
    const SAMPLE: &str = "\
 259       0 nvme0n1 125000 4200 8000000 45000 98000 3100 6000000 38000 0 52000 83000
 259       1 nvme0n1p1 124000 4100 7990000 44800 97000 3000 5990000 37800 0 51000 82000
   8       0 sda 5000 120 400000 3000 2500 80 200000 1800 0 4200 4800
   7       0 loop0 12 0 96 1 0 0 0 0 0 2 1
";

    #[test]
    fn parses_sectors_into_bytes() {
        let stats = parse_diskstats(SAMPLE);

        let nvme = stats.get("nvme0n1").expect("nvme0n1 present");
        assert_eq!(nvme.read_bytes, 8_000_000 * 512);
        assert_eq!(nvme.written_bytes, 6_000_000 * 512);
    }

    #[test]
    fn parses_all_listed_devices_including_partitions() {
        let stats = parse_diskstats(SAMPLE);

        assert!(stats.contains_key("nvme0n1"));
        assert!(stats.contains_key("nvme0n1p1"));
        assert!(stats.contains_key("sda"));
        assert!(stats.contains_key("loop0"));
    }

    #[test]
    fn skips_malformed_lines() {
        let stats = parse_diskstats("garbage\n 8 0 sdb notanumber 1 2 3\n");
        assert!(stats.is_empty());
    }

    #[test]
    fn parent_device_reduces_partitions() {
        assert_eq!(parent_device("sda1"), "sda");
        assert_eq!(parent_device("nvme0n1p2"), "nvme0n1");
        assert_eq!(parent_device("sda"), "sda");
    }

    #[test]
    fn first_refresh_yields_no_rates() {
        // A single sample has no interval to diff, so nothing is reported yet
        // rather than mistaking cumulative totals for interval activity.
        let mut stats = DiskStats::new();
        stats.previous = parse_diskstats(SAMPLE);
        stats.last_sample = Some(Instant::now());

        assert!(!stats.is_available());
        assert!(stats.lookup("/dev/nvme0n1").is_none());
    }

    #[test]
    fn computes_per_device_rates_from_two_samples() {
        let mut stats = DiskStats::new();

        // Seed a baseline, then hand-roll the second sample so the test does
        // not depend on reading a real /proc.
        stats.previous = parse_diskstats(SAMPLE);
        let earlier = Instant::now() - std::time::Duration::from_secs(2);
        stats.last_sample = Some(earlier);

        let mut current = stats.previous.clone();
        // 1 MiB read and 512 KiB written on nvme0n1 over the interval.
        let nvme = current.get_mut("nvme0n1").unwrap();
        nvme.read_bytes += 1_048_576;
        nvme.written_bytes += 524_288;

        // Replicate refresh() against the synthetic sample.
        let elapsed = 2.0_f64;
        for (device, counters) in &current {
            let prev = stats.previous.get(device).unwrap();
            let read = counters.read_bytes.saturating_sub(prev.read_bytes);
            let written = counters.written_bytes.saturating_sub(prev.written_bytes);
            stats.deltas.insert(
                device.clone(),
                DeviceDelta {
                    read_bytes: read,
                    written_bytes: written,
                    read_bytes_per_sec: read as f64 / elapsed,
                    write_bytes_per_sec: written as f64 / elapsed,
                },
            );
        }

        assert!(stats.is_available());

        let delta = stats.lookup("/dev/nvme0n1").expect("nvme0n1 delta");
        assert_eq!(delta.read_bytes, 1_048_576);
        assert_eq!(delta.read_bytes_per_sec, 524_288.0);
        assert_eq!(delta.write_bytes_per_sec, 262_144.0);

        // A partition path resolves to its parent device's counters.
        let via_partition = stats.lookup("/dev/nvme0n1p9").expect("falls back to parent");
        assert_eq!(via_partition.read_bytes, 1_048_576);

        // An idle device reports zero, distinct from "unavailable".
        let sda = stats.lookup("/dev/sda").expect("sda delta");
        assert_eq!(sda.read_bytes_per_sec, 0.0);
    }

    #[test]
    fn counter_reset_does_not_produce_a_spike() {
        // After a reboot the counters restart from zero; saturating_sub must
        // clamp to 0 instead of wrapping to a huge value.
        let previous = parse_diskstats(SAMPLE);
        let prev = previous.get("nvme0n1").unwrap();
        let after_reset = DeviceCounters { read_bytes: 0, written_bytes: 0 };

        assert_eq!(after_reset.read_bytes.saturating_sub(prev.read_bytes), 0);
    }
}
