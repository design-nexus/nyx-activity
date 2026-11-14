//! The UI side of sampling: receives snapshots, keeps the graph history, and
//! tells visible widgets to refresh.

use crate::prefs;
use crate::sampler::{self, Control, Snapshot};
use gtk::glib;
use gtk::prelude::*;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

type Callback = Rc<dyn Fn(&Snapshot)>;
type Pending = Box<dyn FnOnce(&Snapshot)>;

#[derive(Default)]
struct Live {
    latest: Option<Rc<Snapshot>>,
    history: HashMap<String, VecDeque<f64>>,
    subs: Vec<(glib::WeakRef<gtk::Widget>, Callback)>,
    control: Option<Arc<Control>>,
    tracked: HashSet<i32>,
    /// Apps and services (by cgroup path) whose added-up CPU and memory are kept.
    tracked_groups: HashSet<String>,
    pending: Vec<Pending>,
    /// Called with every snapshot, on screen or not (alerts).
    always: Vec<Callback>,
}

thread_local! {
    static LIVE: RefCell<Live> = RefCell::new(Live::default());
}

/// How many samples a graph shows.
pub fn capacity() -> usize {
    let p = prefs::get();
    ((p.history_secs * 1000) / p.interval_ms.max(250)) as usize + 1
}

pub fn start() {
    let control = Control::new(prefs::get().interval_ms);
    let rx = sampler::start(control.clone());
    LIVE.with(|l| l.borrow_mut().control = Some(control));
    glib::spawn_future_local(async move {
        while let Ok(snap) = rx.recv().await {
            receive(snap);
        }
    });
}

pub fn control() -> Arc<Control> {
    LIVE.with(|l| l.borrow().control.clone()).expect("sampler not started")
}

pub fn set_interval(ms: u64) {
    control().interval_ms.store(ms.max(250), Ordering::Relaxed);
    trim();
}

pub fn set_detail(on: bool) {
    control().detail.store(on, Ordering::Relaxed);
}

pub fn paused() -> bool {
    control().paused.load(Ordering::Relaxed)
}

pub fn set_paused(on: bool) {
    let c = control();
    c.paused.store(on, Ordering::Relaxed);
    if !on {
        c.poke.store(true, Ordering::Relaxed);
    }
}

/// Take a fresh sample right away (after ending a task, for instance).
pub fn poke() {
    control().poke.store(true, Ordering::Relaxed);
}

/// Drop history beyond the current capacity (after the length or interval changes).
pub fn trim() {
    let cap = capacity();
    LIVE.with(|l| {
        for v in l.borrow_mut().history.values_mut() {
            while v.len() > cap {
                v.pop_front();
            }
        }
    });
}

pub fn latest() -> Option<Rc<Snapshot>> {
    LIVE.with(|l| l.borrow().latest.clone())
}

pub fn history(key: &str) -> Vec<f64> {
    LIVE.with(|l| l.borrow().history.get(key).map(|v| v.iter().copied().collect()).unwrap_or_default())
}

/// Keep a per-process CPU and memory history for `pid` (the details panel).
pub fn track(pid: i32) {
    LIVE.with(|l| l.borrow_mut().tracked.insert(pid));
}

/// Keep an app's or service's CPU and memory history (`group.<path>.cpu`, `.mem`).
pub fn track_group(path: &str) {
    LIVE.with(|l| l.borrow_mut().tracked_groups.insert(path.to_string()));
}

/// Run `f` once with the first snapshot (right away if one has arrived). Pages
/// whose layout depends on the hardware (disks, interfaces, GPUs) build with this.
pub fn ready(f: impl FnOnce(&Snapshot) + 'static) {
    if let Some(s) = latest() {
        f(&s);
    } else {
        LIVE.with(|l| l.borrow_mut().pending.push(Box::new(f)));
    }
}

/// Call `f` with each new snapshot while `widget` is on screen, and once whenever
/// it comes back on screen. Stops by itself when the widget is destroyed.
pub fn on_tick<W: IsA<gtk::Widget>>(widget: &W, f: impl Fn(&Snapshot) + 'static) {
    let f: Callback = Rc::new(f);
    let g = f.clone();
    widget.connect_map(move |_| {
        if let Some(s) = latest() {
            g(&s);
        }
    });
    if let Some(s) = latest() {
        f(&s);
    }
    let weak = widget.upcast_ref::<gtk::Widget>().downgrade();
    LIVE.with(|l| l.borrow_mut().subs.push((weak, f)));
}

/// Call `f` with every snapshot for as long as the app runs, whatever is on screen.
pub fn on_every(f: impl Fn(&Snapshot) + 'static) {
    LIVE.with(|l| l.borrow_mut().always.push(Rc::new(f)));
}

/// Graph history as CSV: a `seconds_ago` column, then one column per series
/// (sorted by name), oldest row first. `keys` limits it to those series.
pub fn csv(keys: Option<&[String]>) -> String {
    let interval = prefs::get().interval_ms as f64 / 1000.0;
    LIVE.with(|l| {
        let l = l.borrow();
        let series: Vec<(&str, Vec<f64>)> = l
            .history
            .iter()
            .filter(|(k, _)| keys.is_none_or(|keys| keys.contains(k)))
            .map(|(k, v)| (k.as_str(), v.iter().copied().collect()))
            .collect();
        to_csv(series, interval)
    })
}

fn to_csv(mut series: Vec<(&str, Vec<f64>)>, interval: f64) -> String {
    series.sort_by(|a, b| a.0.cmp(b.0));
    let rows = series.iter().map(|(_, v)| v.len()).max().unwrap_or(0);
    let mut out = String::from("seconds_ago");
    for (k, _) in &series {
        out.push(',');
        out.push_str(k);
    }
    out.push('\n');
    for i in 0..rows {
        let back = rows - 1 - i;
        out.push_str(&format!("{}", (back as f64 * interval * 1000.0).round() / 1000.0));
        for (_, v) in &series {
            out.push(',');
            // Shorter series end at "now" too; they have nothing that far back.
            if back < v.len() {
                out.push_str(&format!("{}", v[v.len() - 1 - back]));
            }
        }
        out.push('\n');
    }
    out
}

fn push(history: &mut HashMap<String, VecDeque<f64>>, cap: usize, key: String, value: f64) {
    let v = history.entry(key).or_default();
    v.push_back(value);
    while v.len() > cap {
        v.pop_front();
    }
}

fn record(history: &mut HashMap<String, VecDeque<f64>>, tracked: &HashSet<i32>, groups: &HashSet<String>, s: &Snapshot) {
    let cap = capacity();
    let mut put = |k: String, v: f64| push(history, cap, k, v);
    put("cpu".into(), s.cpu.usage);
    put("cpu.system".into(), s.cpu.system + s.cpu.irq);
    put("cpu.user".into(), s.cpu.user);
    for (i, c) in s.cpu.cores.iter().enumerate() {
        put(format!("cpu.{i}"), *c);
    }
    let avg_freq = if s.cpu.freqs.is_empty() { 0.0 } else { s.cpu.freqs.iter().sum::<f64>() / s.cpu.freqs.len() as f64 };
    put("cpu.freq".into(), avg_freq);
    put("cpu.freq.max".into(), s.cpu.freqs.iter().copied().fold(0.0, f64::max));
    put("cpu.load".into(), s.cpu.load[0]);
    if let Some(p) = s.cpu.pressure {
        put("cpu.psi".into(), p.some10);
    }
    let m = &s.mem;
    put("mem".into(), if m.total > 0 { m.used as f64 / m.total as f64 * 100.0 } else { 0.0 });
    put("mem.bytes".into(), m.used as f64);
    put("swap".into(), if m.swap_total > 0 { m.swap_used as f64 / m.swap_total as f64 * 100.0 } else { 0.0 });
    put("mem.swapin".into(), m.swap_in);
    put("mem.swapout".into(), m.swap_out);
    put("mem.faults".into(), m.major_faults);
    if let Some(p) = m.pressure {
        put("mem.psi".into(), p.some10);
        put("mem.psi.full".into(), p.full10);
    }
    if let Some(p) = s.io_pressure {
        put("io.psi".into(), p.some10);
        put("io.psi.full".into(), p.full10);
    }
    put("disk.read".into(), s.disk_read());
    put("disk.write".into(), s.disk_write());
    put("disk.busy".into(), s.disk_busy());
    for d in &s.disks {
        put(format!("disk.{}.read", d.name), d.read_bps);
        put(format!("disk.{}.write", d.name), d.write_bps);
        put(format!("disk.{}.busy", d.name), d.busy);
        put(format!("disk.{}.riops", d.name), d.read_iops);
        put(format!("disk.{}.wiops", d.name), d.write_iops);
        put(format!("disk.{}.lat", d.name), d.latency_ms);
    }
    put("net.rx".into(), s.net_rx());
    put("net.tx".into(), s.net_tx());
    for n in &s.nets {
        put(format!("net.{}.rx", n.name), n.rx_bps);
        put(format!("net.{}.tx", n.name), n.tx_bps);
        put(format!("net.{}.rxp", n.name), n.rx_pps);
        put(format!("net.{}.txp", n.name), n.tx_pps);
        if let Some(dbm) = n.signal {
            put(format!("net.{}.signal", n.name), crate::fmt::signal_quality(dbm));
        }
    }
    if let Some(c) = &s.conns {
        put("net.conns".into(), c.established as f64);
    }
    for g in &s.gpus {
        put(format!("gpu.{}.util", g.pci), g.util.unwrap_or(0.0));
        if let Some(f) = g.act_clock.or(g.clock) {
            put(format!("gpu.{}.freq", g.pci), f);
        }
        if let Some(m) = g.client_mem {
            put(format!("gpu.{}.mem", g.pci), m as f64);
        }
        if let (Some(u), Some(t)) = (g.vram_used, g.vram_total)
            && t > 0
        {
            put(format!("gpu.{}.vram", g.pci), u as f64 / t as f64 * 100.0);
        }
        put(format!("gpu.{}.watts", g.pci), g.watts.unwrap_or(0.0));
    }
    if let Some(g) = s.gpus.iter().max_by(|a, b| a.util.unwrap_or(0.0).total_cmp(&b.util.unwrap_or(0.0))) {
        put("gpu".into(), g.util.unwrap_or(0.0));
    }
    if let Some(t) = s.sensors.cpu_temp {
        put("temp.cpu".into(), t);
    }
    for t in &s.sensors.temps {
        put(format!("temp.{}.{}", t.chip, t.label), t.value);
    }
    for f in &s.sensors.fans {
        put(format!("fan.{}.{}", f.chip, f.label), f.value);
    }
    for b in &s.sensors.batteries {
        put(format!("bat.{}", b.name), b.percent);
        put(format!("bat.{}.watts", b.name), b.watts);
    }
    if let Some(procs) = &s.procs {
        for p in procs.iter().filter(|p| tracked.contains(&p.pid)) {
            put(format!("proc.{}.cpu", p.pid), p.cpu);
            put(format!("proc.{}.mem", p.pid), p.rss as f64);
        }
    }
    for g in s.groups.iter().filter(|g| groups.contains(&g.path)) {
        put(format!("group.{}.cpu", g.path), g.cpu);
        put(format!("group.{}.mem", g.path), g.memory() as f64);
    }
}

fn receive(snap: Snapshot) {
    let snap = Rc::new(snap);
    let pending = LIVE.with(|l| {
        let mut l = l.borrow_mut();
        let tracked = l.tracked.clone();
        let groups = l.tracked_groups.clone();
        record(&mut l.history, &tracked, &groups, &snap);
        // Keep the details graphs' pids only while they're alive.
        if let Some(procs) = &snap.procs {
            let alive: HashSet<i32> = procs.iter().map(|p| p.pid).collect();
            let gone: Vec<i32> = l.tracked.iter().filter(|p| !alive.contains(p)).copied().collect();
            for pid in gone {
                l.tracked.remove(&pid);
                l.history.remove(&format!("proc.{pid}.cpu"));
                l.history.remove(&format!("proc.{pid}.mem"));
            }
            let live: HashSet<&str> = snap.groups.iter().map(|g| g.path.as_str()).collect();
            let gone: Vec<String> = l.tracked_groups.iter().filter(|p| !live.contains(p.as_str())).cloned().collect();
            for path in gone {
                l.tracked_groups.remove(&path);
                l.history.remove(&format!("group.{path}.cpu"));
                l.history.remove(&format!("group.{path}.mem"));
            }
        }
        l.latest = Some(snap.clone());
        std::mem::take(&mut l.pending)
    });
    for f in pending {
        f(&snap);
    }
    let callbacks: Vec<Callback> = LIVE.with(|l| {
        let mut l = l.borrow_mut();
        l.subs.retain(|(w, _)| w.upgrade().is_some());
        l.subs.iter().filter(|(w, _)| w.upgrade().is_some_and(|w| w.is_mapped())).map(|(_, f)| f.clone()).collect()
    });
    let always: Vec<Callback> = LIVE.with(|l| l.borrow().always.clone());
    for f in callbacks.into_iter().chain(always) {
        f(&snap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_csv() {
        let csv = to_csv(vec![("mem", vec![1.0, 2.0, 3.0]), ("cpu", vec![9.5])], 0.5);
        assert_eq!(csv, "seconds_ago,cpu,mem\n1,,1\n0.5,,2\n0,9.5,3\n");
        assert_eq!(to_csv(vec![], 1.0), "seconds_ago\n");
    }
}
