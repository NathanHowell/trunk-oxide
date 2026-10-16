//! Process CPU time from `/proc`, per thread group.
//!
//! GNU Radio runs each block on its own thread named after the block, so CPU
//! time grouped by thread name shows which stage of the pipeline is expensive.
//! Linux only: elsewhere (or if `/proc` is unreadable) nothing is reported.

use std::collections::BTreeMap;

/// `USER_HZ`, the unit of `utime`/`stime` in `/proc/*/stat`. Fixed at 100 on
/// Linux regardless of the kernel's internal tick rate.
const CLOCK_TICKS_PER_SEC: f64 = 100.0;

/// CPU seconds consumed so far, split by mode.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CpuTimes {
    pub user: f64,
    pub system: f64,
}

/// Parse one `/proc/<pid>/stat` (or `/proc/<pid>/task/<tid>/stat`) line into
/// `(comm, times)`. `comm` is parenthesised and may itself contain spaces and
/// parentheses, so fields are located relative to the *last* `)`.
pub fn parse_stat(line: &str) -> Option<(String, CpuTimes)> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    let comm = line.get(open + 1..close)?.to_string();
    // After ")": state(3) ppid(4) ... utime(14) stime(15); state is index 0.
    let mut rest = line.get(close + 1..)?.split_whitespace();
    let utime: f64 = rest.nth(11)?.parse().ok()?;
    let stime: f64 = rest.next()?.parse().ok()?;
    Some((
        comm,
        CpuTimes {
            user: utime / CLOCK_TICKS_PER_SEC,
            system: stime / CLOCK_TICKS_PER_SEC,
        },
    ))
}

/// Group key for a thread name: GNU Radio appends the block instance number
/// (`fll_band_edge_cc12`), which would otherwise make one series per recorder.
pub fn thread_group(comm: &str) -> &str {
    let trimmed = comm.trim_end_matches(|c: char| c.is_ascii_digit());
    if trimmed.is_empty() { comm } else { trimmed }
}

/// Whole-process CPU time (all threads, including exited ones).
pub fn process_cpu() -> Option<CpuTimes> {
    let line = std::fs::read_to_string("/proc/self/stat").ok()?;
    parse_stat(&line).map(|(_, t)| t)
}

/// CPU time of the *live* threads, summed by thread-name group.
pub fn thread_cpu_by_group() -> BTreeMap<String, f64> {
    let mut groups: BTreeMap<String, f64> = BTreeMap::new();
    let Ok(dir) = std::fs::read_dir("/proc/self/task") else {
        return groups;
    };
    for entry in dir.flatten() {
        let Ok(line) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue; // thread exited between readdir and read
        };
        if let Some((comm, t)) = parse_stat(&line) {
            *groups.entry(thread_group(&comm).to_string()).or_default() += t.user + t.system;
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    // utime=1234 stime=56 (fields 14 and 15).
    const LINE: &str = "4242 (fll_band_edge_c) S 1 4242 4242 0 -1 4194560 100 0 0 0 1234 56 0 0 20 0 30 0 100 1000 200 18446744073709551615";

    #[test]
    fn parses_cpu_times() {
        let (comm, t) = parse_stat(LINE).unwrap();
        assert_eq!(comm, "fll_band_edge_c");
        assert!((t.user - 12.34).abs() < 1e-9);
        assert!((t.system - 0.56).abs() < 1e-9);
    }

    #[test]
    fn comm_may_contain_spaces_and_parens() {
        let line = "7 (my (odd) name) R 1 7 7 0 -1 0 0 0 0 0 100 200 0 0 20 0 1 0 5 6 7 8";
        let (comm, t) = parse_stat(line).unwrap();
        assert_eq!(comm, "my (odd) name");
        assert!((t.user - 1.0).abs() < 1e-9);
        assert!((t.system - 2.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_stat("not a stat line").is_none());
        assert!(parse_stat("1 (x) S 1 2").is_none());
    }

    #[test]
    fn groups_gnuradio_block_instances() {
        assert_eq!(thread_group("fll_band_edge_c12"), "fll_band_edge_c");
        assert_eq!(thread_group("selector3"), "selector");
        assert_eq!(thread_group("123"), "123");
        assert_eq!(thread_group("tokio-runtime-w"), "tokio-runtime-w");
    }
}
