//! Process resource sampling from `/proc/self`.

use std::fs;
use std::io;
use std::time::Instant;

/// User and system CPU time in clock ticks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuTimes {
    /// Ticks spent in user mode.
    pub user: u64,
    /// Ticks spent in the kernel.
    pub system: u64,
}

impl CpuTimes {
    /// User plus system ticks.
    #[must_use]
    pub fn total(self) -> u64 {
        self.user + self.system
    }
}

/// CPU usage as percentages of one core.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CpuSplit {
    /// User plus system.
    pub total: f64,
    /// User mode only.
    pub user: f64,
    /// Kernel only.
    pub system: f64,
}

/// Parses `utime` and `stime` in clock ticks from a `/proc/<pid>/stat` line.
///
/// The command name in field 2 may contain spaces and parentheses, so fields
/// are counted from the last `)`: the field after it is 3 (`state`), `utime`
/// is field 14 and `stime` field 15.
fn parse_stat_times(line: &str) -> Option<CpuTimes> {
    let after = &line[line.rfind(')')? + 1..];
    let mut fields = after.split_ascii_whitespace();
    let user: u64 = fields.nth(11)?.parse().ok()?;
    let system: u64 = fields.next()?.parse().ok()?;
    Some(CpuTimes { user, system })
}

/// Parses `VmRSS` (kibibytes) from `/proc/<pid>/status` text.
fn parse_vmrss_kib(status: &str) -> Option<u64> {
    status_field(status, "VmRSS:")
}

/// Parses `Threads` from `/proc/<pid>/status` text.
fn parse_threads(status: &str) -> Option<u64> {
    status_field(status, "Threads:")
}

fn status_field(status: &str, name: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|line| line.strip_prefix(name))
        .and_then(|rest| rest.split_ascii_whitespace().next())
        .and_then(|value| value.parse().ok())
}

/// Reads this process's user and system CPU time in clock ticks.
fn cpu_times() -> io::Result<CpuTimes> {
    let stat = fs::read_to_string("/proc/self/stat")?;
    parse_stat_times(&stat).ok_or_else(|| io::Error::other("cannot parse /proc/self/stat"))
}

/// Reads this process's resident set size in bytes.
pub fn rss_bytes() -> io::Result<u64> {
    let status = fs::read_to_string("/proc/self/status")?;
    parse_vmrss_kib(&status)
        .map(|kib| kib * 1024)
        .ok_or_else(|| io::Error::other("cannot parse VmRSS in /proc/self/status"))
}

/// Reads this process's thread count.
pub fn threads() -> io::Result<u64> {
    let status = fs::read_to_string("/proc/self/status")?;
    parse_threads(&status)
        .ok_or_else(|| io::Error::other("cannot parse Threads in /proc/self/status"))
}

/// Counts this process's open file descriptors (the directory handle used to
/// count them included).
pub fn fd_count() -> io::Result<usize> {
    Ok(fs::read_dir("/proc/self/fd")?.count())
}

/// Measures CPU usage as a percentage of one core.
#[derive(Debug)]
pub struct CpuMeter {
    ticks_per_sec: f64,
    start: CpuTimes,
    start_at: Instant,
    last: CpuTimes,
    last_at: Instant,
}

impl CpuMeter {
    /// Starts measuring from the moment of the call.
    ///
    /// # Errors
    ///
    /// Fails when `/proc/self/stat` cannot be read or parsed.
    pub fn try_new() -> io::Result<Self> {
        let times = cpu_times()?;
        let now = Instant::now();
        Ok(Self {
            ticks_per_sec: rustix::param::clock_ticks_per_second() as f64,
            start: times,
            start_at: now,
            last: times,
            last_at: now,
        })
    }

    /// Percent of one core used since the previous call (or since creation),
    /// split into user and system time.
    ///
    /// # Errors
    ///
    /// Fails when `/proc/self/stat` cannot be read or parsed.
    pub fn sample(&mut self) -> io::Result<CpuSplit> {
        let times = cpu_times()?;
        let now = Instant::now();
        let secs = now.duration_since(self.last_at).as_secs_f64();
        let split = CpuSplit {
            total: percent(
                times.total().saturating_sub(self.last.total()),
                secs,
                self.ticks_per_sec,
            ),
            user: percent(
                times.user.saturating_sub(self.last.user),
                secs,
                self.ticks_per_sec,
            ),
            system: percent(
                times.system.saturating_sub(self.last.system),
                secs,
                self.ticks_per_sec,
            ),
        };
        self.last = times;
        self.last_at = now;
        Ok(split)
    }

    /// Percent of one core used since the meter was created.
    ///
    /// # Errors
    ///
    /// Fails when `/proc/self/stat` cannot be read or parsed.
    pub fn mean(&self) -> io::Result<f64> {
        let ticks = cpu_times()?.total();
        Ok(percent(
            ticks.saturating_sub(self.start.total()),
            self.start_at.elapsed().as_secs_f64(),
            self.ticks_per_sec,
        ))
    }
}

fn percent(ticks: u64, secs: f64, ticks_per_sec: f64) -> f64 {
    if secs <= 0.0 {
        0.0
    } else {
        ticks as f64 / ticks_per_sec / secs * 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stat_with_spaces_in_command_name() {
        let line = "12345 (irisd (test) daemon) S 1 12345 12345 0 -1 4194560 1000 0 0 0 150 25 0 0 \
                    20 0 1 0 100 1000000 500 18446744073709551615";
        assert_eq!(
            parse_stat_times(line),
            Some(CpuTimes {
                user: 150,
                system: 25
            })
        );
        assert_eq!(parse_stat_times(line).map(CpuTimes::total), Some(175));
        assert_eq!(parse_stat_times("garbage"), None);
    }

    #[test]
    fn parses_status_fields() {
        let status = "Name:\tirisd\nVmPeak:\t   20000 kB\nVmRSS:\t   12345 kB\nThreads:\t3\n";
        assert_eq!(parse_vmrss_kib(status), Some(12_345));
        assert_eq!(parse_threads(status), Some(3));
        assert_eq!(parse_vmrss_kib("Name:\tx\n"), None);
    }

    #[test]
    fn percent_handles_zero_interval() {
        assert_eq!(percent(100, 0.0, 100.0), 0.0);
        assert!((percent(50, 1.0, 100.0) - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn counts_own_descriptors() {
        assert!(fd_count().unwrap() >= 3);
    }

    #[test]
    fn counts_own_threads() {
        assert!(threads().unwrap() >= 1);
    }
}
