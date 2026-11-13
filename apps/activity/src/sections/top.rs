//! The busiest apps and processes by one resource, as ranked bars.

use crate::apps;
use crate::charts::Bar;
use crate::fmt;
use crate::sampler::Snapshot;
use crate::sampler::groups::{Group, Kind};
use crate::sampler::procs::Proc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum By {
    Cpu,
    Mem,
    Swap,
    Io,
    Gpu,
    GpuMem,
}

impl By {
    fn proc(self, p: &Proc) -> f64 {
        match self {
            By::Cpu => p.cpu,
            By::Mem => p.rss as f64,
            By::Swap => p.swap as f64,
            By::Io => p.read_bps + p.write_bps,
            By::Gpu => p.gpu,
            By::GpuMem => p.gpu_mem as f64,
        }
    }

    fn group(self, g: &Group) -> f64 {
        match self {
            By::Cpu => g.cpu,
            By::Mem => g.memory() as f64,
            By::Swap => g.swap as f64,
            By::Io => g.io,
            By::Gpu => g.gpu,
            By::GpuMem => g.gpu_mem as f64,
        }
    }

    pub fn show(self, v: f64) -> String {
        match self {
            By::Cpu | By::Gpu => fmt::pct(v),
            By::Mem | By::Swap | By::GpuMem => fmt::bytes(v),
            By::Io => fmt::rate(v),
        }
    }

    /// Below this, a process isn't worth a row.
    fn floor(self) -> f64 {
        match self {
            By::Cpu | By::Gpu => 0.05,
            By::Mem | By::Swap | By::GpuMem => 1024.0 * 1024.0,
            By::Io => 1.0,
        }
    }
}

/// An app's display name and icon: from its window's class, else its process name.
pub fn app_identity(name: &str, pids: &[i32], snap: &Snapshot) -> (String, String) {
    let class = snap.windows.iter().find(|w| pids.contains(&w.pid)).map(|w| w.class.as_str());
    match class.and_then(apps::lookup).or_else(|| apps::lookup(name)) {
        Some(a) => (if a.name.is_empty() { name.to_string() } else { a.name }, a.icon),
        None => (name.to_string(), String::new()),
    }
}

pub fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::App => "App",
        Kind::Service => "Service",
        Kind::Other => "Group",
    }
}

/// A group's display name and icon. Only an app's own scope takes a window's name:
/// a window launched straight into the session or a service doesn't speak for it.
pub fn group_identity(g: &Group, snap: &Snapshot) -> (String, String) {
    let pids: &[i32] = if g.kind == Kind::App { &g.pids } else { &[] };
    app_identity(&g.name, pids, snap)
}

/// Bar lengths are relative to the biggest item, or to `whole` when given (all of RAM, say).
fn scale(values: &[f64], whole: Option<f64>) -> f64 {
    whole.unwrap_or_else(|| values.iter().copied().fold(0.0, f64::max)).max(1e-9)
}

/// The busiest processes (kernel threads left out).
pub fn procs(snap: &Snapshot, by: By, n: usize, whole: Option<f64>) -> Vec<Bar> {
    let Some(list) = &snap.procs else { return Vec::new() };
    let mut ps: Vec<&Proc> = list.iter().filter(|p| !p.kernel && by.proc(p) >= by.floor()).collect();
    ps.sort_by(|a, b| by.proc(b).total_cmp(&by.proc(a)));
    ps.truncate(n);
    let max = scale(&ps.iter().map(|p| by.proc(p)).collect::<Vec<_>>(), whole);
    ps.into_iter()
        .map(|p| {
            let (_, icon) = app_identity(&p.name, &[p.pid], snap);
            Bar {
                name: p.name.clone(),
                icon,
                frac: by.proc(p) / max,
                value: by.show(by.proc(p)),
                tip: String::new(),
                pid: p.pid,
                warn: false,
            }
        })
        .collect()
}

/// The busiest apps and services, each its processes added up.
pub fn groups(snap: &Snapshot, by: By, n: usize, whole: Option<f64>) -> Vec<Bar> {
    let mut gs: Vec<&Group> = snap.groups.iter().filter(|g| by.group(g) >= by.floor()).collect();
    gs.sort_by(|a, b| by.group(b).total_cmp(&by.group(a)));
    gs.truncate(n);
    let max = scale(&gs.iter().map(|g| by.group(g)).collect::<Vec<_>>(), whole);
    gs.into_iter()
        .map(|g| {
            let (name, icon) = group_identity(g, snap);
            let what = kind_name(g.kind);
            let procs = if g.procs == 1 { "1 process".to_string() } else { format!("{} processes", g.procs) };
            let mut tip = format!("{name} · {what} {} · {procs}", g.unit);
            if by == By::Mem && g.cache > 0 {
                tip.push_str(&format!("\nPlus {} of file cache it could give back", fmt::bytes(g.cache as f64)));
            }
            tip.push_str("\nClick to show it in Processes");
            Bar { name, icon, frac: by.group(g) / max, value: by.show(by.group(g)), tip, pid: g.leader, warn: false }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sampler::windows::Window;

    #[test]
    fn only_app_scopes_take_a_window_name() {
        let window = Window { pid: 42, class: "io.github.design_nexus.nyx.Activity".into(), ..Default::default() };
        let snap = Snapshot { windows: vec![window], ..Default::default() };
        let session = Group { name: "Hyprland".into(), kind: Kind::Other, pids: vec![1, 42], ..Default::default() };
        assert_eq!(group_identity(&session, &snap), app_identity("Hyprland", &[], &snap));
        let app = Group { kind: Kind::App, ..session.clone() };
        assert_eq!(group_identity(&app, &snap), app_identity("Hyprland", &[1, 42], &snap));
    }
}
