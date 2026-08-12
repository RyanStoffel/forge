//! Process inspection for the right sidebar's Processes tab.
//!
//! Rather than listing every process on the machine, this walks the process
//! tree rooted at each pane's shell, so the tab answers "what is running in my
//! panes right now" — including whatever an agent CLI has spawned.
//!
//! Refreshing reads every process on the system (needed to reconstruct
//! parent/child links), which costs tens of milliseconds. Callers should only
//! refresh while the tab is actually visible, and on a slow interval.

use std::collections::HashMap;
use std::path::PathBuf;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

pub use sysinfo::MINIMUM_CPU_UPDATE_INTERVAL;

#[derive(Clone, Debug)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    /// Percentage of a single core.
    pub cpu: f32,
    /// Resident set size in bytes.
    pub memory: u64,
    /// Nesting level below the pane's shell; the shell itself is 0.
    pub depth: usize,
}

pub struct ProcessMonitor {
    system: System,
}

impl ProcessMonitor {
    pub fn new() -> Self {
        Self {
            system: System::new(),
        }
    }

    /// Refresh and return the process trees rooted at `roots`, depth-first and
    /// stable-sorted so rows don't jump between refreshes.
    ///
    /// CPU percentages are computed as a delta against the previous refresh,
    /// so the first call after construction reports 0.
    pub fn refresh(&mut self, roots: &[u32]) -> Vec<ProcessInfo> {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );

        let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
        for (pid, process) in self.system.processes() {
            if let Some(parent) = process.parent() {
                children.entry(parent).or_default().push(*pid);
            }
        }
        for kids in children.values_mut() {
            kids.sort();
        }

        let mut out = Vec::new();
        for root in roots {
            self.collect(Pid::from_u32(*root), 0, &children, &mut out);
        }
        out
    }

    fn collect(
        &self,
        pid: Pid,
        depth: usize,
        children: &HashMap<Pid, Vec<Pid>>,
        out: &mut Vec<ProcessInfo>,
    ) {
        // Guard against a pathological/cyclic tree rather than recursing away
        // the stack.
        if depth > 32 {
            return;
        }
        if let Some(process) = self.system.process(pid) {
            out.push(ProcessInfo {
                pid: pid.as_u32(),
                name: process.name().to_string_lossy().to_string(),
                cpu: process.cpu_usage(),
                memory: process.memory(),
                depth,
            });
        }
        if let Some(kids) = children.get(&pid) {
            for kid in kids {
                self.collect(*kid, depth + 1, children, out);
            }
        }
    }
}

impl Default for ProcessMonitor {
    fn default() -> Self {
        Self::new()
    }
}

/// Cheap lookup of individual process names, for deriving a workspace's
/// dynamic label from whatever its shell is currently running.
///
/// Unlike [`ProcessMonitor`], this refreshes only the pids asked about rather
/// than the whole process table, so it is cheap enough to call whenever the
/// foreground process might have changed.
pub struct ForegroundProbe {
    system: System,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForegroundInfo {
    pub name: String,
    pub cwd: Option<PathBuf>,
}

impl ForegroundProbe {
    pub fn new() -> Self {
        Self {
            system: System::new(),
        }
    }

    pub fn inspect(&mut self, pid: u32) -> Option<ForegroundInfo> {
        let pid = Pid::from_u32(pid);
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_cwd(UpdateKind::Always),
        );
        self.system.process(pid).map(|process| ForegroundInfo {
            name: process.name().to_string_lossy().to_string(),
            cwd: process.cwd().map(PathBuf::from),
        })
    }

    pub fn name_of(&mut self, pid: u32) -> Option<String> {
        self.inspect(pid).map(|info| info.name)
    }
}

impl Default for ForegroundProbe {
    fn default() -> Self {
        Self::new()
    }
}

/// Compact human-readable byte size, sized for a narrow sidebar column.
pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.1}G", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.0}M", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0}K", bytes as f64 / KB as f64)
    } else {
        format!("{bytes}B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_byte_sizes_compactly() {
        assert_eq!(format_bytes(512), "512B");
        assert_eq!(format_bytes(2048), "2K");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5M");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0G");
    }

    #[test]
    fn finds_the_current_process_tree() {
        let mut monitor = ProcessMonitor::new();
        let me = std::process::id();
        let found = monitor.refresh(&[me]);
        assert!(
            found.iter().any(|p| p.pid == me && p.depth == 0),
            "expected the current process as a depth-0 root"
        );
    }

    #[test]
    fn unknown_root_yields_nothing() {
        let mut monitor = ProcessMonitor::new();
        assert!(monitor.refresh(&[u32::MAX]).is_empty());
    }

    #[test]
    fn probe_reads_the_current_process_name_and_cwd() {
        let mut probe = ForegroundProbe::new();
        let info = probe.inspect(std::process::id()).expect("own process");
        assert!(!info.name.is_empty());
        assert_eq!(info.cwd.as_deref(), std::env::current_dir().ok().as_deref());
    }

    #[test]
    fn probe_returns_none_for_unknown_pid() {
        let mut probe = ForegroundProbe::new();
        assert!(probe.name_of(u32::MAX).is_none());
    }
}
