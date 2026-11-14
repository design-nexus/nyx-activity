//! Processes added up by the systemd unit they run in: each app Hyprland
//! launches gets its own scope, each service its own cgroup. Memory comes from
//! the cgroup itself, so shared pages aren't counted once per process.

use super::procs::Proc;
use super::read;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    /// An app's scope (`app-….scope`).
    App,
    Service,
    #[default]
    Other,
}

#[derive(Debug, Clone, Default)]
pub struct Group {
    /// The cgroup's last part: "app-Hyprland-firefox-1a2b.scope", "pipewire.service".
    pub unit: String,
    /// The whole cgroup path ("/system.slice/…" for system services).
    pub path: String,
    pub kind: Kind,
    /// Its first process, the one the others descend from.
    pub leader: i32,
    /// A name for people: the leader's name, or the biggest process's when the leader is a wrapper.
    pub name: String,
    pub pids: Vec<i32>,
    pub procs: usize,
    pub cpu: f64,
    /// Resident memory summed over the processes (counts shared pages more than once).
    pub rss: u64,
    /// What the cgroup is charged for, less file cache it could drop: anonymous,
    /// shared memory and kernel memory. `None` when the cgroup can't be read.
    pub mem: Option<u64>,
    /// File cache charged to it.
    pub cache: u64,
    pub swap: u64,
    pub io: f64,
    pub gpu: f64,
    pub gpu_mem: u64,
}

/// `memory.stat` keys by name, in bytes.
pub fn parse_memory_stat(text: &str) -> HashMap<&str, u64> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(' ')?;
            Some((k, v.trim().parse().ok()?))
        })
        .collect()
}

/// (charged memory less dropable file cache, file cache).
pub fn charged(stat: &HashMap<&str, u64>) -> (u64, u64) {
    let g = |k: &str| stat.get(k).copied().unwrap_or(0);
    let kernel =
        if stat.contains_key("kernel") { g("kernel") } else { g("kernel_stack") + g("pagetables") + g("slab") + g("percpu") };
    let file = g("file");
    let shmem = g("shmem");
    (g("anon") + shmem + kernel + g("sock"), file.saturating_sub(shmem))
}

const WRAPPERS: &[&str] =
    &["sh", "bash", "dash", "zsh", "fish", "uwsm", "uwsm-app", "env", "sleep", "systemd-run", "xdg-terminal-exec"];

pub fn kind(unit: &str) -> Kind {
    if unit.starts_with("app-") && unit.ends_with(".scope") {
        Kind::App
    } else if unit.ends_with(".service") {
        Kind::Service
    } else {
        Kind::Other
    }
}

pub fn groups(procs: &[Proc]) -> Vec<Group> {
    let mut by_cg: HashMap<&str, Vec<&Proc>> = HashMap::new();
    for p in procs.iter().filter(|p| !p.kernel && !p.cgroup.is_empty() && p.cgroup != "/") {
        by_cg.entry(p.cgroup.as_str()).or_default().push(p);
    }
    let mut out = Vec::with_capacity(by_cg.len());
    for (cg, members) in by_cg {
        let unit = cg.rsplit('/').next().unwrap_or(cg).to_string();
        // Orphans are adopted from outside the group, so there can be several roots;
        // the leader is the one with the most processes under it.
        let parent: HashMap<i32, i32> = members.iter().map(|p| (p.pid, p.ppid)).collect();
        let mut under: HashMap<i32, usize> = HashMap::new();
        for p in &members {
            let mut at = p.pid;
            let mut hops = 0;
            while let Some(&up) = parent.get(&at).filter(|up| parent.contains_key(up)) {
                at = up;
                hops += 1;
                if hops > 64 {
                    break;
                }
            }
            *under.entry(at).or_default() += 1;
        }
        let leader = members
            .iter()
            .filter(|p| under.contains_key(&p.pid))
            .max_by_key(|p| (under[&p.pid], std::cmp::Reverse(p.pid)))
            .or(members.first())
            .copied();
        let Some(leader) = leader else { continue };
        let biggest = members.iter().max_by_key(|p| p.rss).copied().unwrap_or(leader);
        let name = if members.iter().any(|p| p.name == "Hyprland") {
            // The session's own cgroup: Hyprland and everything its config starts.
            "Hyprland".to_string()
        } else if WRAPPERS.contains(&leader.name.as_str()) {
            biggest.app_name()
        } else {
            leader.app_name()
        };
        let stat = read(&format!("/sys/fs/cgroup{cg}/memory.stat"));
        let (mem, cache) = if stat.is_empty() {
            (None, 0)
        } else {
            let (m, c) = charged(&parse_memory_stat(&stat));
            (Some(m), c)
        };
        let swap = read(&format!("/sys/fs/cgroup{cg}/memory.swap.current"))
            .trim()
            .parse()
            .unwrap_or_else(|_| members.iter().map(|p| p.swap).sum());
        out.push(Group {
            path: cg.to_string(),
            kind: kind(&unit),
            unit,
            leader: leader.pid,
            name,
            pids: members.iter().map(|p| p.pid).collect(),
            procs: members.len(),
            cpu: members.iter().map(|p| p.cpu).sum(),
            rss: members.iter().map(|p| p.rss).sum(),
            mem,
            cache,
            swap,
            io: members.iter().map(|p| p.read_bps + p.write_bps).sum(),
            gpu: members.iter().map(|p| p.gpu).fold(0.0, f64::max),
            gpu_mem: members.iter().map(|p| p.gpu_mem).sum(),
        });
    }
    out
}

impl Group {
    /// The best memory figure: the cgroup's when there is one.
    pub fn memory(&self) -> u64 {
        self.mem.unwrap_or(self.rss)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charges_memory_without_file_cache() {
        let text = "anon 1000\nfile 5000\nkernel 200\nkernel_stack 10\nshmem 300\nsock 5\nfile_mapped 40\n";
        let s = parse_memory_stat(text);
        assert_eq!(charged(&s), (1000 + 300 + 200 + 5, 4700));
    }

    #[test]
    fn names_units() {
        assert_eq!(kind("app-Hyprland-firefox-1a2b.scope"), Kind::App);
        assert_eq!(kind("pipewire.service"), Kind::Service);
        assert_eq!(kind("session-2.scope"), Kind::Other);
    }
}
