//! Processes added up by the systemd unit they run in: each app Hyprland
//! launches gets its own scope, each service its own cgroup. Memory comes from
//! the cgroup itself, so shared pages aren't counted once per process.

use super::procs::Proc;
use super::read;
use std::collections::{HashMap, HashSet};

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
    /// How many scopes it was added up from: one per window for a terminal that opens each in its own.
    pub scopes: usize,
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

/// The compositor's own processes, which stay with it when its cgroup is split.
const COMPOSITOR: &[&str] = &["Hyprland", "start-hyprland", "Xwayland"];

/// Each member's root: the furthest ancestor inside `members`. `stop` ends the walk
/// below a process (the compositor), so what it started gets roots of its own.
fn roots(members: &[&Proc], stop: impl Fn(&Proc) -> bool) -> HashMap<i32, i32> {
    let by_pid: HashMap<i32, &Proc> = members.iter().map(|p| (p.pid, *p)).collect();
    members
        .iter()
        .map(|p| {
            let mut at = *p;
            for _ in 0..64 {
                match by_pid.get(&at.ppid) {
                    Some(up) if !stop(up) && !stop(at) => at = up,
                    _ => break,
                }
            }
            (p.pid, at.pid)
        })
        .collect()
}

/// A shell or launcher, by its name or (for a script, which takes the script's name) its interpreter.
fn wrapper(p: &Proc) -> bool {
    let program = p.cmdline.split_whitespace().next().and_then(|a| a.rsplit('/').next()).unwrap_or("");
    WRAPPERS.contains(&p.name.as_str()) || WRAPPERS.contains(&program)
}

/// A name for a set of processes: its leader's, or the biggest one's when the leader is a wrapper.
fn name_of(leader: &Proc, members: &[&Proc]) -> String {
    if wrapper(leader) {
        members.iter().filter(|p| !wrapper(p)).max_by_key(|p| p.rss).unwrap_or(&leader).app_name()
    } else {
        leader.app_name()
    }
}

fn group_of(path: String, unit: String, kind: Kind, leader: &Proc, name: String, members: &[&Proc]) -> Group {
    Group {
        path,
        kind,
        unit,
        leader: leader.pid,
        name,
        pids: members.iter().map(|p| p.pid).collect(),
        procs: members.len(),
        cpu: members.iter().map(|p| p.cpu).sum(),
        rss: members.iter().map(|p| p.rss).sum(),
        mem: None,
        cache: 0,
        swap: members.iter().map(|p| p.swap).sum(),
        io: members.iter().map(|p| p.read_bps + p.write_bps).sum(),
        gpu: members.iter().map(|p| p.gpu).fold(0.0, f64::max),
        gpu_mem: members.iter().map(|p| p.gpu_mem).sum(),
        scopes: 1,
    }
}

/// The session's cgroup holds Hyprland and everything it (or its config) started
/// without a scope of its own: each of those becomes a group, named after what it runs.
fn split_session(cg: &str, unit: &str, members: &[&Proc]) -> Vec<Group> {
    let compositor = |p: &Proc| COMPOSITOR.contains(&p.name.as_str());
    let root = roots(members, compositor);
    let mut by_root: HashMap<i32, Vec<&Proc>> = HashMap::new();
    for p in members {
        let r = if compositor(p) { 0 } else { root[&p.pid] };
        by_root.entry(r).or_default().push(p);
    }
    let by_pid: HashMap<i32, &Proc> = members.iter().map(|p| (p.pid, *p)).collect();
    let mut by_name: HashMap<String, Vec<&Proc>> = HashMap::new();
    for (r, ps) in by_root {
        let name = if r == 0 { "Hyprland".to_string() } else { name_of(by_pid[&r], &ps) };
        by_name.entry(name).or_default().extend(ps);
    }
    by_name
        .into_iter()
        .map(|(name, ps)| {
            let leader = ps.iter().min_by_key(|p| (p.start as u64, p.pid)).copied().unwrap_or(ps[0]);
            let leader =
                if name == "Hyprland" { ps.iter().find(|p| p.name == "Hyprland").copied().unwrap_or(leader) } else { leader };
            if name == "Hyprland" {
                group_of(cg.to_string(), unit.to_string(), kind(unit), leader, name, &ps)
            } else {
                group_of(format!("{cg}#{name}"), String::new(), Kind::Other, leader, name, &ps)
            }
        })
        .collect()
}

/// Apps open in several scopes (a terminal per window) are one app: their groups added up.
fn merge_apps(groups: Vec<Group>) -> Vec<Group> {
    let mut out: Vec<Group> = Vec::with_capacity(groups.len());
    let mut at: HashMap<String, usize> = HashMap::new();
    for g in groups {
        let key = (g.kind == Kind::App).then(|| g.name.to_lowercase());
        match key.as_ref().and_then(|k| at.get(k)) {
            Some(&i) => {
                let m = &mut out[i];
                if g.memory() > m.memory() {
                    m.leader = g.leader;
                }
                m.path = format!("app:{}", g.name.to_lowercase());
                m.pids.extend(g.pids);
                m.procs += g.procs;
                m.cpu += g.cpu;
                m.rss += g.rss;
                m.mem = m.mem.zip(g.mem).map(|(a, b)| a + b);
                m.cache += g.cache;
                m.swap += g.swap;
                m.io += g.io;
                m.gpu = m.gpu.max(g.gpu);
                m.gpu_mem += g.gpu_mem;
                m.scopes += 1;
            }
            None => {
                if let Some(k) = key {
                    at.insert(k, out.len());
                }
                out.push(g);
            }
        }
    }
    out
}

/// `windowed`: processes that own a window, which name the scope they're in.
pub fn groups(procs: &[Proc], windowed: &HashSet<i32>) -> Vec<Group> {
    let mut by_cg: HashMap<&str, Vec<&Proc>> = HashMap::new();
    for p in procs.iter().filter(|p| !p.kernel && !p.cgroup.is_empty() && p.cgroup != "/") {
        by_cg.entry(p.cgroup.as_str()).or_default().push(p);
    }
    let mut out = Vec::with_capacity(by_cg.len());
    for (cg, members) in by_cg {
        let unit = cg.rsplit('/').next().unwrap_or(cg).to_string();
        if members.iter().any(|p| p.name == "Hyprland") {
            out.extend(split_session(cg, &unit, &members));
            continue;
        }
        // Orphans are adopted from outside the group, so there can be several roots;
        // the leader is the one with the most processes under it.
        let root = roots(&members, |_| false);
        let mut under: HashMap<i32, usize> = HashMap::new();
        for r in root.values() {
            *under.entry(*r).or_default() += 1;
        }
        let leader = members
            .iter()
            .filter(|p| under.contains_key(&p.pid))
            .max_by_key(|p| (under[&p.pid], std::cmp::Reverse(p.pid)))
            .or(members.first())
            .copied();
        let Some(leader) = leader else { continue };
        // A terminal running a big program is still the terminal.
        let owner = members.iter().find(|p| windowed.contains(&p.pid));
        let name = owner.map_or_else(|| name_of(leader, &members), |p| p.app_name());
        let mut g = group_of(cg.to_string(), unit.clone(), kind(&unit), leader, name, &members);
        let stat = read(&format!("/sys/fs/cgroup{cg}/memory.stat"));
        if !stat.is_empty() {
            let (m, c) = charged(&parse_memory_stat(&stat));
            g.mem = Some(m);
            g.cache = c;
        }
        if let Ok(swap) = read(&format!("/sys/fs/cgroup{cg}/memory.swap.current")).trim().parse() {
            g.swap = swap;
        }
        out.push(g);
    }
    merge_apps(out)
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

    fn proc(pid: i32, ppid: i32, name: &str, cgroup: &str) -> Proc {
        Proc { pid, ppid, name: name.into(), cgroup: cgroup.into(), rss: 1 << 20, start: pid as f64, ..Default::default() }
    }

    #[test]
    fn splits_what_the_session_started() {
        let s = "/user.slice/session.slice/wayland-wm@hyprland.desktop.service";
        let ps = vec![
            proc(10, 1, "start-hyprland", s),
            proc(11, 10, "Hyprland", s),
            proc(12, 11, "Xwayland", s),
            proc(20, 11, "bash", s),
            proc(21, 20, "quickshell", s),
            proc(30, 1, "inotifywait", s),
            proc(31, 1, "inotifywait", s),
        ];
        let mut gs = groups(&ps, &HashSet::new());
        gs.sort_by_key(|g| g.name.clone());
        let names: Vec<(&str, usize)> = gs.iter().map(|g| (g.name.as_str(), g.procs)).collect();
        assert_eq!(names, [("Hyprland", 3), ("inotifywait", 2), ("quickshell", 2)]);
        assert_eq!(gs[0].leader, 11);
        assert!(gs[1].path.ends_with("#inotifywait"));
    }

    #[test]
    fn one_app_in_several_scopes_is_one_group() {
        let a = "/app.slice/app-Hyprland-foot-1.scope";
        let b = "/app.slice/app-Hyprland-foot-2.scope";
        let mut big = proc(4, 3, "nvim", b);
        big.rss = 1 << 30;
        let ps = vec![proc(1, 0, "foot", a), proc(2, 1, "fish", a), proc(3, 0, "foot", b), big];
        let gs = groups(&ps, &HashSet::from([1, 3]));
        assert_eq!(gs.len(), 1);
        assert_eq!((gs[0].procs, gs[0].scopes, gs[0].path.as_str()), (4, 2, "app:foot"));
    }
}
