//! The sampler thread. Every interval it reads `/proc`, `/sys` and friends into a
//! [`Snapshot`] and hands it to the UI thread over a channel. Nothing here
//! touches GTK.

pub mod conns;
pub mod cpu;
pub mod disk;
pub mod gpu;
pub mod groups;
pub mod mem;
pub mod net;
pub mod procs;
pub mod sensors;
pub mod windows;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Read a small file, or "" on any error (processes vanish, sysfs files come and go).
pub fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub cpu: cpu::Cpu,
    pub mem: mem::Mem,
    pub disks: Vec<disk::Disk>,
    pub nets: Vec<net::Iface>,
    pub gpus: Vec<gpu::Gpu>,
    pub sensors: sensors::Sensors,
    /// `None` while detail sampling is paused (window hidden).
    pub procs: Option<Vec<procs::Proc>>,
    pub windows: Vec<windows::Window>,
    pub process_count: usize,
    pub thread_count: u64,
    /// Processes added up by app or service (empty while detail sampling is paused).
    pub groups: Vec<groups::Group>,
    pub conns: Option<conns::Conns>,
    pub io_pressure: Option<cpu::Pressure>,
}

impl Snapshot {
    pub fn disk_read(&self) -> f64 {
        self.disks.iter().map(|d| d.read_bps).sum()
    }
    pub fn disk_write(&self) -> f64 {
        self.disks.iter().map(|d| d.write_bps).sum()
    }
    pub fn disk_busy(&self) -> f64 {
        self.disks.iter().map(|d| d.busy).fold(0.0, f64::max)
    }
    /// Physical interfaces only, so VPNs and bridges aren't counted twice.
    pub fn net_rx(&self) -> f64 {
        self.nets.iter().filter(|n| n.kind != net::Kind::Virtual).map(|n| n.rx_bps).sum()
    }
    pub fn net_tx(&self) -> f64 {
        self.nets.iter().filter(|n| n.kind != net::Kind::Virtual).map(|n| n.tx_bps).sum()
    }
}

/// Settings the UI can change while the sampler runs.
pub struct Control {
    pub interval_ms: AtomicU64,
    /// Collect processes and GPU data (off while the window is hidden).
    pub detail: AtomicBool,
    /// Frozen by the user: no samples at all.
    pub paused: AtomicBool,
    /// Take a sample now instead of waiting out the interval.
    pub poke: AtomicBool,
    /// Read a GPU that's asleep or idle once, though it wakes it.
    pub read_gpu: AtomicBool,
}

impl Control {
    pub fn new(interval_ms: u64) -> Arc<Control> {
        Arc::new(Control {
            interval_ms: AtomicU64::new(interval_ms.max(250)),
            detail: AtomicBool::new(true),
            paused: AtomicBool::new(false),
            poke: AtomicBool::new(false),
            read_gpu: AtomicBool::new(false),
        })
    }
}

pub fn start(control: Arc<Control>) -> async_channel::Receiver<Snapshot> {
    let (tx, rx) = async_channel::bounded(2);
    std::thread::Builder::new()
        .name("sampler".into())
        .spawn(move || {
            let mut cpu = cpu::CpuSampler::new();
            let mut mem = mem::MemSampler::new();
            let mut disks = disk::DiskSampler::new();
            let mut nets = net::NetSampler::new();
            let mut procs = procs::ProcSampler::new();
            let mut gpus = gpu::GpuSampler::new();
            let mut last = Instant::now();
            let mut last_detail = Instant::now();
            let mut tick: u64 = 0;
            let mut windows = Vec::new();
            loop {
                let now = Instant::now();
                let elapsed = now.duration_since(last).as_secs_f64();
                last = now;
                let detail = control.detail.load(Ordering::Relaxed);
                let mut snap = Snapshot {
                    cpu: cpu.sample(),
                    mem: mem.sample(),
                    disks: disks.sample(elapsed),
                    nets: nets.sample(elapsed),
                    sensors: sensors::sample(),
                    io_pressure: cpu::pressure("io"),
                    ..Default::default()
                };
                if detail {
                    let detail_elapsed = now.duration_since(last_detail).as_secs_f64();
                    last_detail = now;
                    let (list, clients) = procs.sample(detail_elapsed);
                    snap.process_count = list.len();
                    snap.thread_count = list.iter().map(|p| p.threads as u64).sum();
                    snap.gpus = gpus.sample(&clients, control.read_gpu.swap(false, Ordering::Relaxed));
                    if tick.is_multiple_of(2) {
                        windows = windows::clients();
                    }
                    let owners: std::collections::HashSet<i32> = windows.iter().map(|w: &windows::Window| w.pid).collect();
                    snap.groups = groups::groups(&list, &owners);
                    snap.conns = Some(conns::sample(&clients.sockets));
                    snap.windows = windows.clone();
                    snap.procs = Some(list);
                }
                tick += 1;
                if tx.send_blocking(snap).is_err() {
                    return;
                }
                // Sleep in short steps so interval changes and pokes apply quickly.
                let started = Instant::now();
                loop {
                    std::thread::sleep(Duration::from_millis(50));
                    if control.poke.swap(false, Ordering::Relaxed) {
                        break;
                    }
                    let interval = Duration::from_millis(control.interval_ms.load(Ordering::Relaxed));
                    if started.elapsed() >= interval && !control.paused.load(Ordering::Relaxed) {
                        break;
                    }
                }
            }
        })
        .expect("could not start the sampler thread");
    rx
}
