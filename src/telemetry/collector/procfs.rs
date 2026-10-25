//! Parsers for the `/proc` files the collector reads, from their text.
//! Reading the files is the binary's job; these only interpret what it read.

/// `PF_KTHREAD` in `/proc/<pid>/stat`'s flags: a kernel thread.
pub const PF_KTHREAD: u64 = 0x0020_0000;

/// The fields of `/proc/<pid>/stat` the collector uses (proc(5) numbering in
/// the comments). Times are in clock ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcStat {
    /// Field 4.
    pub ppid: u32,
    /// Field 9.
    pub flags: u64,
    /// Fields 14 and 15: the process's own CPU time, all its threads.
    pub utime: u64,
    pub stime: u64,
    /// Fields 16 and 17: the CPU time of the children it has waited for.
    pub cutime: u64,
    pub cstime: u64,
    /// Field 22: when it started, in ticks since boot.
    pub starttime: u64,
}

impl ProcStat {
    /// Parse a `/proc/<pid>/stat` line. The fields are read after the
    /// **last** `)`, so a process name holding `)` or spaces cannot shift
    /// them. `None` when the line is malformed.
    pub fn parse(text: &str) -> Option<Self> {
        let rest = &text[text.rfind(')')? + 1..];
        // `fields[0]` is field 3 (state), so field n is `fields[n - 3]`.
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let unsigned = |n: usize| fields.get(n - 3)?.parse::<u64>().ok();
        // `cutime` and `cstime` are signed (`%ld`); a negative value is not
        // time anyone used.
        let signed = |n: usize| {
            fields
                .get(n - 3)?
                .parse::<i64>()
                .ok()
                .map(|v| u64::try_from(v).unwrap_or(0))
        };
        Some(ProcStat {
            ppid: u32::try_from(signed(4)?).ok()?,
            flags: unsigned(9)?,
            utime: unsigned(14)?,
            stime: unsigned(15)?,
            cutime: signed(16)?,
            cstime: signed(17)?,
            starttime: unsigned(22)?,
        })
    }

    /// Everything the process and the children it reaped used.
    pub fn total_ticks(&self) -> u64 {
        self.utime + self.stime + self.cutime + self.cstime
    }

    /// What the children it reaped used.
    pub fn children_ticks(&self) -> u64 {
        self.cutime + self.cstime
    }

    pub fn is_kernel_thread(&self) -> bool {
        self.flags & PF_KTHREAD != 0
    }
}

/// A process's proportional set size by kind, in bytes, from
/// `/proc/<pid>/smaps_rollup`. Their sum is its `Pss`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pss {
    pub anon: u64,
    pub file: u64,
    pub shmem: u64,
}

impl Pss {
    /// Parse `smaps_rollup`'s `Pss_Anon`, `Pss_File` and `Pss_Shmem` lines
    /// (in kB). `None` when none of them is present: a kernel older than 5.8,
    /// or a process with no address space.
    pub fn parse_smaps_rollup(text: &str) -> Option<Self> {
        let mut pss = Pss::default();
        let mut found = false;
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            let slot = match parts.next() {
                Some("Pss_Anon:") => &mut pss.anon,
                Some("Pss_File:") => &mut pss.file,
                Some("Pss_Shmem:") => &mut pss.shmem,
                _ => continue,
            };
            if let Some(kb) = parts.next().and_then(|v| v.parse::<u64>().ok()) {
                *slot = kb * 1024;
                found = true;
            }
        }
        found.then_some(pss)
    }

    pub fn total(&self) -> u64 {
        self.anon + self.file + self.shmem
    }

    pub fn add(&mut self, other: &Pss) {
        self.anon += other.anon;
        self.file += other.file;
        self.shmem += other.shmem;
    }
}

/// The effective user a process runs as: the second value of the `Uid:`
/// line of `/proc/<pid>/status` (real, effective, saved, filesystem). Not the
/// owner of `/proc/<pid>`, which is root for a process that made itself
/// non-dumpable, whatever user it runs as. `None` when the line is missing
/// or malformed.
pub fn parse_effective_uid(status: &str) -> Option<u32> {
    let values = status.lines().find_map(|line| line.strip_prefix("Uid:"))?;
    values.split_whitespace().nth(1)?.parse().ok()
}

/// `btime` from `/proc/stat`: the boot time, in seconds since the epoch.
pub fn parse_boot_time(proc_stat: &str) -> Option<u64> {
    proc_stat
        .lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|v| v.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real line, from a process named `a) b (c`.
    const STAT: &str = "4242 (a) b (c) S 4200 4242 4200 34816 4242 4194560 1525 0 0 0 \
        123 45 67 8 20 0 3 0 9876543 10000000 500 18446744073709551615 1 1 0 0 0 0 \
        65536 3686404 1266761467 0 0 0 17 3 0 0 0 0 0";

    #[test]
    fn parses_stat_after_the_last_paren() {
        let stat = ProcStat::parse(STAT).unwrap();
        assert_eq!(
            stat,
            ProcStat {
                ppid: 4200,
                flags: 4194560,
                utime: 123,
                stime: 45,
                cutime: 67,
                cstime: 8,
                starttime: 9876543,
            }
        );
        assert_eq!(stat.total_ticks(), 243);
        assert_eq!(stat.children_ticks(), 75);
        assert!(!stat.is_kernel_thread());
    }

    #[test]
    fn kernel_threads_and_negative_children_time() {
        let kthread = "2 (kthreadd) S 0 0 0 0 -1 2129984 0 0 0 0 0 7 -3 0 20 0 1 0 2 0 0";
        let stat = ProcStat::parse(kthread).unwrap();
        assert!(stat.is_kernel_thread());
        assert_eq!(stat.ppid, 0);
        assert_eq!(stat.cutime, 0, "a negative value counts as none");
        assert_eq!(stat.starttime, 2);
    }

    #[test]
    fn malformed_stat_is_none() {
        assert_eq!(ProcStat::parse(""), None);
        assert_eq!(ProcStat::parse("1 (x"), None);
        assert_eq!(ProcStat::parse("1 (x) S 0 1 2"), None);
        assert_eq!(
            ProcStat::parse(&STAT.replace(" 123 ", " notanumber ")),
            None
        );
    }

    #[test]
    fn parses_pss_by_kind() {
        let text = "55d0c0de0000-7ffd00000000 ---p 00000000 00:00 0                          [rollup]\n\
            Rss:                1200 kB\n\
            Pss:                 900 kB\n\
            Pss_Dirty:           396 kB\n\
            Pss_Anon:            396 kB\n\
            Pss_File:            500 kB\n\
            Pss_Shmem:             4 kB\n\
            SwapPss:              10 kB\n";
        let pss = Pss::parse_smaps_rollup(text).unwrap();
        assert_eq!(
            pss,
            Pss {
                anon: 396 * 1024,
                file: 500 * 1024,
                shmem: 4 * 1024
            }
        );
        assert_eq!(pss.total(), 900 * 1024);
        assert_eq!(Pss::parse_smaps_rollup("Rss: 1 kB\nPss: 1 kB\n"), None);
        assert_eq!(Pss::parse_smaps_rollup(""), None);
    }

    #[test]
    fn parses_the_effective_uid_from_status() {
        let status = "Name:\tsh\nUmask:\t0022\nState:\tS (sleeping)\nTgid:\t4242\n\
            Pid:\t4242\nPPid:\t1\nUid:\t1000\t0\t0\t0\nGid:\t1000\t1000\t1000\t1000\n";
        assert_eq!(parse_effective_uid(status), Some(0));
        assert_eq!(
            parse_effective_uid("Uid:\t1000\t1001\t1002\t1003\n"),
            Some(1001)
        );
        assert_eq!(parse_effective_uid("Name:\tsh\n"), None);
        assert_eq!(parse_effective_uid("Uid:\t1000\n"), None);
        assert_eq!(parse_effective_uid("Uid:\t1000\tx\t0\t0\n"), None);
    }

    #[test]
    fn parses_boot_time() {
        let text = "cpu  1 2 3\nintr 5\nctxt 9\nbtime 1790000000\nprocesses 12\n";
        assert_eq!(parse_boot_time(text), Some(1790000000));
        assert_eq!(parse_boot_time("cpu 1\n"), None);
    }
}
