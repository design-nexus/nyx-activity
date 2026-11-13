//! What needs a look: a hot CPU, memory or a disk running out, a process stuck at
//! high CPU, or a service that failed. The Dashboard shows them as problems, first
//! thing; desktop notifications (each kind on its own switch) tell you while Activity
//! is open, on screen or not, and each waits a while before repeating.

use crate::sampler::Snapshot;
use crate::startup::Unit;
use crate::{cmd, fmt, live, prefs};
use gtk::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// How long one alert stays quiet after it fires.
const COOLDOWN: Duration = Duration::from_secs(300);

/// A unit systemd lists as failed.
#[derive(Debug, Clone)]
pub struct Failed {
    pub unit: Unit,
    pub user: bool,
    /// When it failed, as Unix seconds.
    pub since: Option<i64>,
}

#[derive(Default)]
struct Alerts {
    /// When each alert (by key) last fired.
    fired: HashMap<String, Instant>,
    /// Since when each pid has been over the CPU limit.
    busy_since: HashMap<i32, Instant>,
    /// Failed units at the last check; None before the first check.
    failed: Option<Vec<Failed>>,
}

thread_local! {
    static ALERTS: RefCell<Alerts> = RefCell::new(Alerts::default());
}

pub fn start() {
    live::on_every(check);
    glib::timeout_add_seconds_local(60, || {
        check_services();
        glib::ControlFlow::Continue
    });
    check_services();
}

/// The units that are failed now (empty until the first check is back).
pub fn failed() -> Vec<Failed> {
    ALERTS.with(|a| a.borrow().failed.clone().unwrap_or_default())
}

/// Where a problem's button takes you.
#[derive(Debug, Clone, PartialEq)]
pub enum Go {
    Page(&'static str),
    /// Processes, sorted by this column.
    Sorted(&'static str),
    Process(i32),
    Logs(String, bool),
}

/// Something worth a look, in plain words, with what to do about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    pub key: String,
    /// Red (a failure, a hot CPU) rather than amber.
    pub bad: bool,
    pub icon: &'static str,
    pub title: String,
    pub detail: String,
    pub action: &'static str,
    pub go: Go,
}

/// "Blender and Firefox": the biggest apps by a measure, by name.
fn biggest(s: &Snapshot, by: impl Fn(&crate::sampler::groups::Group) -> f64, n: usize) -> Vec<(String, f64)> {
    let mut gs: Vec<_> = s.groups.iter().filter(|g| by(g) > 0.0).collect();
    gs.sort_by(|a, b| by(b).total_cmp(&by(a)));
    gs.into_iter().take(n).map(|g| (crate::sections::top::group_identity(g, s).0, by(g))).collect()
}

/// Mounted filesystems, once each (btrfs subvolumes and the like share a disk).
pub fn mounts(s: &Snapshot) -> Vec<&crate::sampler::disk::Mount> {
    let mut seen = HashSet::new();
    let mut out: Vec<_> = s.disks.iter().flat_map(|d| &d.mounts).filter(|m| m.total > 0 && seen.insert(&m.path)).collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// "at 03:12", "yesterday at 03:12", "on 9 Nov at 03:12".
fn at(unix: i64) -> String {
    let (Ok(t), Ok(now)) = (glib::DateTime::from_unix_local(unix), glib::DateTime::now_local()) else { return String::new() };
    let time = t.format("%H:%M").map(|s| s.to_string()).unwrap_or_default();
    let day = |d: &glib::DateTime| (d.year(), d.day_of_year());
    if day(&t) == day(&now) {
        format!("at {time}")
    } else if now.add_days(-1).is_ok_and(|y| day(&y) == day(&t)) {
        format!("yesterday at {time}")
    } else {
        format!("on {} at {time}", t.format("%-d %b").map(|s| s.to_string()).unwrap_or_default())
    }
}

/// Everything past its limit right now, the worst first.
pub fn problems(s: &Snapshot) -> Vec<Problem> {
    let p = prefs::get();
    let mut out = Vec::new();
    if let Some(t) = s.sensors.cpu_temp
        && t >= p.warn_temp
    {
        let detail = match biggest(s, |g| g.cpu, 1).first() {
            Some((name, cpu)) => format!("{name} is using {} of the processor.", fmt::pct(*cpu)),
            None => format!("That's past your {} limit.", fmt::temp(p.warn_temp)),
        };
        out.push(Problem {
            key: "temp".into(),
            bad: true,
            icon: "activity-thermometer-symbolic",
            title: format!("The CPU is at {}", fmt::temp(t)),
            detail,
            action: "See what's busy",
            go: Go::Sorted("cpu"),
        });
    }
    for f in failed() {
        let when = f.since.map(at).unwrap_or_default();
        let desc = &f.unit.description;
        let described = !desc.is_empty() && *desc != f.unit.name;
        let (title, detail) = if f.unit.name.ends_with(".mount") && desc.starts_with('/') {
            (format!("{desc} didn't mount {when}"), "The log says why.".to_string())
        } else {
            let name = crate::startup::unescape(f.unit.name.trim_end_matches(".service"));
            let what = if described { format!("{desc} stopped with an error.") } else { "It stopped with an error.".into() };
            (format!("{name} failed {when}"), what)
        };
        out.push(Problem {
            key: format!("unit.{}.{}", f.user, f.unit.name),
            bad: true,
            icon: "dialog-warning-symbolic",
            title: title.trim_end().to_string(),
            detail,
            action: "See log",
            go: Go::Logs(f.unit.name.clone(), f.user),
        });
    }
    if s.mem.total > 0 {
        let used = s.mem.used as f64 / s.mem.total as f64 * 100.0;
        if used >= p.warn_mem {
            let top = biggest(s, |g| g.memory() as f64, 2);
            let using = |(n, b): &(String, f64)| format!("{n} {}", fmt::bytes(*b));
            let detail = match top.as_slice() {
                [a, b] => format!("{} is using {} and {}.", a.0, fmt::bytes(a.1), using(b)),
                [a] => format!("{} is using {}.", a.0, fmt::bytes(a.1)),
                _ => format!("{} is still free.", fmt::bytes(s.mem.available as f64)),
            };
            out.push(Problem {
                key: "mem".into(),
                bad: false,
                icon: "activity-memory-symbolic",
                title: format!("Memory is {} full", fmt::pct(used)),
                detail,
                action: "Show them",
                go: Go::Sorted("mem"),
            });
        }
    }
    for m in mounts(s) {
        let used = m.used() as f64 / m.total as f64 * 100.0;
        if used >= p.warn_disk {
            out.push(Problem {
                key: format!("disk.{}", m.path),
                bad: false,
                icon: "drive-harddisk-symbolic",
                title: format!("{} is {} full", m.path, fmt::pct(used)),
                detail: format!("{} left of {}.", fmt::bytes(m.free as f64), fmt::bytes(m.total as f64)),
                action: "See storage",
                go: Go::Page("storage"),
            });
        }
    }
    if let Some((pid, name, cpu, secs)) = busiest_stuck(s) {
        out.push(Problem {
            key: format!("proc.{pid}"),
            bad: false,
            icon: "activity-cpu-symbolic",
            title: format!("{name} has kept the CPU busy for {}", fmt::duration(secs)),
            detail: format!("It's using {} of the processor.", fmt::pct(cpu)),
            action: "Show it",
            go: Go::Process(pid),
        });
    }
    out
}

/// The busiest process that has stayed over the CPU limit for the time set:
/// (pid, name, CPU %, seconds over).
fn busiest_stuck(s: &Snapshot) -> Option<(i32, String, f64, f64)> {
    let p = prefs::get();
    let procs = s.procs.as_ref()?;
    ALERTS.with(|a| {
        let a = a.borrow();
        procs
            .iter()
            .filter_map(|q| {
                let secs = a.busy_since.get(&q.pid)?.elapsed().as_secs_f64();
                (secs >= p.alert_proc_secs as f64).then(|| (q.pid, q.name.clone(), q.cpu, secs))
            })
            .max_by(|x, y| x.2.total_cmp(&y.2))
    })
}

/// Send `body` unless the alert `key` fired recently. `section` opens on click.
fn notify(key: &str, title: &str, body: &str, section: &str) {
    let due = ALERTS.with(|a| {
        let mut a = a.borrow_mut();
        let due = a.fired.get(key).is_none_or(|t| t.elapsed() >= COOLDOWN);
        if due {
            a.fired.insert(key.to_string(), Instant::now());
        }
        due
    });
    if !due {
        return;
    }
    let Some(app) = gio::Application::default() else { return };
    let n = gio::Notification::new(title);
    n.set_body(Some(body));
    n.set_default_action_and_target_value("app.show", Some(&section.to_variant()));
    app.send_notification(Some(key), &n);
}

fn check(s: &Snapshot) {
    let p = prefs::get();
    // Needs per-process data, which stops while the window rests hidden.
    if let Some(procs) = &s.procs {
        let now = Instant::now();
        ALERTS.with(|a| {
            let mut a = a.borrow_mut();
            let over: HashSet<i32> = procs.iter().filter(|q| q.cpu >= p.alert_proc_cpu).map(|q| q.pid).collect();
            a.busy_since.retain(|pid, _| over.contains(pid));
            for pid in over {
                a.busy_since.entry(pid).or_insert(now);
            }
        });
    }
    if p.alert_temp
        && let Some(t) = s.sensors.cpu_temp
        && t >= p.warn_temp
    {
        notify("temp", "CPU is hot", &format!("The CPU is at {}.", fmt::temp(t)), "sensors");
    }
    if p.alert_mem && s.mem.total > 0 {
        let used = s.mem.used as f64 / s.mem.total as f64 * 100.0;
        if used >= p.warn_mem {
            let body = format!("{} in use, {} available.", fmt::pct(used), fmt::bytes(s.mem.available as f64));
            notify("mem", "Memory is running low", &body, "memory");
        }
    }
    if p.alert_disk {
        for m in mounts(s) {
            let used = m.used() as f64 / m.total as f64 * 100.0;
            if used >= p.warn_disk {
                let body = format!("{} is {} full, {} left.", m.path, fmt::pct(used), fmt::bytes(m.free as f64));
                notify(&format!("disk.{}", m.path), "A disk is nearly full", &body, "storage");
            }
        }
    }
    if p.alert_proc
        && let Some((pid, name, cpu, _)) = busiest_stuck(s)
    {
        let body =
            format!("{name} (PID {pid}) has used {} CPU for over {}.", fmt::pct(cpu), fmt::duration(p.alert_proc_secs as f64));
        notify(&format!("proc.{pid}"), "A process is keeping the CPU busy", &body, "processes");
    }
}

/// `systemctl list-units --state=failed -o json` as units.
fn parse_failed(json: &str) -> Vec<Unit> {
    serde_json::from_str::<Vec<serde_json::Value>>(json)
        .unwrap_or_default()
        .iter()
        .filter_map(|u| {
            Some(Unit {
                name: u["unit"].as_str()?.to_string(),
                description: u["description"].as_str().unwrap_or("").to_string(),
                active: u["active"].as_str().unwrap_or("failed").to_string(),
                sub: u["sub"].as_str().unwrap_or("").to_string(),
                file_state: String::new(),
            })
        })
        .collect()
}

/// `StateChangeTimestamp=@1699999999` as Unix seconds.
fn parse_since(text: &str) -> Option<i64> {
    text.lines().find_map(|l| l.strip_prefix("StateChangeTimestamp=@"))?.trim().parse().ok()
}

fn check_services() {
    cmd::background(
        || {
            let mut out = Vec::new();
            for (scope, user) in [("--user", true), ("--system", false)] {
                let json = cmd::output(&["systemctl", scope, "list-units", "--state=failed", "-o", "json", "--no-pager"])
                    .unwrap_or_default();
                for unit in parse_failed(&json) {
                    let show = cmd::output(&[
                        "systemctl",
                        scope,
                        "show",
                        &unit.name,
                        "-p",
                        "StateChangeTimestamp",
                        "--timestamp=unix",
                        "--no-pager",
                    ])
                    .unwrap_or_default();
                    out.push(Failed { since: parse_since(&show), unit, user });
                }
            }
            out
        },
        |now: Vec<Failed>| {
            let key = |f: &Failed| format!("{} {}", f.user, f.unit.name);
            let new: Vec<Failed> = ALERTS.with(|a| {
                let mut a = a.borrow_mut();
                // The first check only learns what was already failed.
                let new = a
                    .failed
                    .as_ref()
                    .map(|old| {
                        let old: HashSet<String> = old.iter().map(key).collect();
                        now.iter().filter(|f| !old.contains(&key(f))).cloned().collect()
                    })
                    .unwrap_or_default();
                a.failed = Some(now);
                new
            });
            if prefs::get().alert_services {
                for f in new {
                    let name = crate::startup::unescape(f.unit.name.trim_end_matches(".service"));
                    notify(
                        &format!("unit.{}", key(&f)),
                        "A service failed",
                        &format!("{name} stopped with an error."),
                        "services",
                    );
                }
            }
            for f in ON_FAILED.with(|l| l.borrow().clone()) {
                f();
            }
        },
    );
}

thread_local! {
    static ON_FAILED: RefCell<Vec<std::rc::Rc<dyn Fn()>>> = const { RefCell::new(Vec::new()) };
}

/// Call `f` after each check of the failed units.
pub fn connect_failed(f: impl Fn() + 'static) {
    ON_FAILED.with(|l| l.borrow_mut().push(std::rc::Rc::new(f)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_failed_units() {
        let got = parse_failed(
            r#"[{"unit":"a.service","load":"loaded","active":"failed","description":"Back up"},{"unit":"b.timer"}]"#,
        );
        let names: Vec<&str> = got.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, ["a.service", "b.timer"]);
        assert_eq!(got[0].description, "Back up");
        assert!(got[1].failed());
        assert!(parse_failed("").is_empty());
    }

    #[test]
    fn reads_when_it_failed() {
        assert_eq!(parse_since("StateChangeTimestamp=@1731474720\n"), Some(1731474720));
        assert_eq!(parse_since("StateChangeTimestamp=\n"), None);
    }
}
