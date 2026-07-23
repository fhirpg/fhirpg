//! Process memory reporting.
//!
//! Decision D14. fhirbase's `--memusage` prints Go garbage-collector statistics
//! — `Alloc`, `TotalAlloc`, `Sys`, `NumGC` (`load.go:590-604`) — sampled every
//! 3,000 resources. None of those translate: Rust has no garbage collector, and
//! the default allocator does not expose live bytes.
//!
//! What it reports instead is the process's **resident set size**. That is a
//! different quantity — resident pages including allocator slack, not live heap
//! — so anything printed from here MUST say so, or a reader will take it for
//! fhirbase's `Alloc`.
//!
//! RSS was chosen over a counting global allocator because it costs nothing on
//! the hot path, and because spec invariant 6 needs this measurement anyway:
//! the same reader backs T13's assertion that a 1 GB input does not produce a
//! 1 GB allocation, and T25's benchmarks.

/// The process's current resident set size, in bytes.
///
/// Returns `None` on a platform with no implementation here, in which case
/// callers should report that the figure is unavailable rather than print a
/// zero.
#[must_use]
pub fn resident_bytes() -> Option<u64> {
    platform::resident_bytes()
}

/// Formats a byte count as a human-readable string.
///
/// The `u64` to `f64` conversion loses precision above 2^53 bytes, which is
/// nine petabytes of resident memory; this exists to print a figure a person
/// reads.
#[must_use]
#[allow(clippy::cast_precision_loss, reason = "formatting for humans")]
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Tracks the high-water mark of resident size across a run.
#[derive(Debug, Default)]
pub struct PeakTracker {
    peak: u64,
    available: bool,
}

impl PeakTracker {
    /// Starts tracking, taking an initial sample.
    #[must_use]
    pub fn new() -> Self {
        match resident_bytes() {
            Some(bytes) => Self {
                peak: bytes,
                available: true,
            },
            None => Self {
                peak: 0,
                available: false,
            },
        }
    }

    /// Takes a sample and returns the current resident size, if available.
    pub fn sample(&mut self) -> Option<u64> {
        let current = resident_bytes()?;
        self.peak = self.peak.max(current);
        Some(current)
    }

    /// The highest resident size seen, if the platform reports it.
    #[must_use]
    pub fn peak(&self) -> Option<u64> {
        self.available.then_some(self.peak)
    }
}

#[cfg(target_os = "linux")]
mod platform {
    /// Reads RSS from `/proc/self/statm`, whose second field is resident pages.
    pub fn resident_bytes() -> Option<u64> {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        // `sysconf(_SC_PAGESIZE)` is 4 KiB on every platform this runs on;
        // reading it would mean libc, which the crate otherwise does not need.
        Some(pages * 4096)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    /// Reads RSS via `ps`.
    ///
    /// `task_info` would avoid the subprocess but needs `unsafe` and a `mach2`
    /// dependency, and `unsafe_code` is forbidden crate-wide. This is sampled
    /// once per few thousand resources, so the cost does not matter.
    pub fn resident_bytes() -> Option<u64> {
        let output = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()?;
        let kilobytes: u64 = String::from_utf8_lossy(&output.stdout).trim().parse().ok()?;
        Some(kilobytes * 1024)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    /// No implementation for this platform.
    pub fn resident_bytes() -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resident_size_is_reported_on_this_platform() {
        // Linux and macOS are the platforms CI runs; a `None` here on either
        // means the reader broke, not that the platform is exotic.
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            let bytes = resident_bytes().expect("RSS should be readable here");
            assert!(bytes > 1_000_000, "implausible RSS: {bytes}");
        }
    }

    #[test]
    fn the_peak_never_decreases() {
        let mut tracker = PeakTracker::new();
        let first = tracker.peak();
        // Allocate something substantial, then release it.
        let ballast: Vec<u8> = vec![7; 64 * 1024 * 1024];
        assert_eq!(ballast.len(), 64 * 1024 * 1024);
        tracker.sample();
        drop(ballast);
        tracker.sample();

        if let (Some(before), Some(after)) = (first, tracker.peak()) {
            assert!(after >= before, "peak went backwards: {before} -> {after}");
        }
    }

    #[test]
    fn byte_counts_format_readably() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(10 * 1024 * 1024), "10.0 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }
}
