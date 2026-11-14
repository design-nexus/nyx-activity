//! Processes from `/proc/[pid]`, plus a periodic scan of open file descriptors
//! that finds GPU clients (and, through DRM fdinfo, how busy each one keeps the GPU).

use super::read;
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;

#[derive(Debug, Clone, Default)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub name: String,
    pub cmdline: String,
    pub uid: u32,
    pub user: String,
    pub state: char,
    pub threads: u32,
    pub nice: i32,
    /// % of all cores together.
    pub cpu: f64,
    pub rss: u64,
    pub read_bps: f64,
    pub write_bps: f64,
    /// Seconds after boot the process started.
    pub start: f64,
    pub kernel: bool,
    /// GPU engine busy %, from DRM fdinfo (Intel/AMD).
    pub gpu: f64,
    /// Holds the NVIDIA GPU open.
    pub nvidia: bool,
    pub fds: u32,
    /// The cgroup it runs in ("/user.slice/…/app-Hyprland-firefox-1234.scope").
    pub cgroup: String,
    /// Swapped-out memory (`VmSwap`), refreshed every few samples.
    pub swap: u64,
    /// GPU memory its DRM clients hold (Intel/AMD fdinfo).
    pub gpu_mem: u64,
}

impl Proc {
    /// A name for the app a process belongs to. A process can rename itself after its
    /// role or thread ("Web Content", "MainThread"); then it's the program's file name.
    pub fn app_name(&self) -> String {
        let threadish = self.name.ends_with("Thread") || self.name.contains(' ');
        let program = self.cmdline.split_whitespace().next().and_then(|a| a.rsplit('/').next()).unwrap_or("");
        if threadish && !program.is_empty() { program.to_string() } else { self.name.clone() }
    }
}

/// Fields of `/proc/[pid]/stat` we use.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stat {
    pub comm: String,
    pub state: char,
    pub ppid: i32,
    pub ticks: u64,
    pub nice: i32,
    pub threads: u32,
    pub start: u64,
    pub rss_pages: u64,
}

pub fn parse_stat(text: &str) -> Option<Stat> {
    // The name is in parentheses and may itself contain spaces or parentheses.
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = text.get(open + 1..close)?.to_string();
    let f: Vec<&str> = text.get(close + 2..)?.split_whitespace().collect();
    let n = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    Some(Stat {
        comm,
        state: f.first()?.chars().next()?,
        ppid: f.get(1)?.parse().ok()?,
        ticks: n(11) + n(12),
        nice: f.get(16).and_then(|v| v.parse().ok()).unwrap_or(0),
        threads: n(17) as u32,
        start: n(19),
        rss_pages: n(21),
    })
}

/// `read_bytes` and `write_bytes` from `/proc/[pid]/io` (storage I/O only).
pub fn parse_io(text: &str) -> Option<(u64, u64)> {
    let mut r = None;
    let mut w = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("read_bytes: ") {
            r = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("write_bytes: ") {
            w = v.trim().parse().ok();
        }
    }
    Some((r?, w?))
}

/// One DRM client's counters from an fdinfo file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DrmClient {
    pub pdev: String,
    pub id: String,
    /// Engine name -> (busy ns, capacity).
    pub engines: HashMap<String, (u64, u64)>,
    /// Memory resident in each region ("system", "vram0", "local0"…), bytes.
    pub resident: u64,
}

/// "123456 KiB" / "12 MiB" / "4096" as bytes.
fn fdinfo_bytes(v: &str) -> u64 {
    let mut parts = v.split_whitespace();
    let n: u64 = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    match parts.next() {
        Some("KiB") => n * 1024,
        Some("MiB") => n * 1024 * 1024,
        Some("GiB") => n * 1024 * 1024 * 1024,
        _ => n,
    }
}

pub fn parse_fdinfo(text: &str) -> Option<DrmClient> {
    let mut c = DrmClient::default();
    let mut caps: HashMap<String, u64> = HashMap::new();
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        if k == "drm-pdev" {
            c.pdev = v.to_string();
        } else if k == "drm-client-id" {
            c.id = v.to_string();
        } else if k.starts_with("drm-resident-") {
            c.resident += fdinfo_bytes(v);
        } else if let Some(engine) = k.strip_prefix("drm-engine-capacity-") {
            caps.insert(engine.to_string(), v.parse().unwrap_or(1));
        } else if let Some(engine) = k.strip_prefix("drm-engine-") {
            let ns = v.trim_end_matches("ns").trim().parse().unwrap_or(0);
            c.engines.insert(engine.to_string(), (ns, 1));
        }
    }
    for (engine, cap) in caps {
        if let Some(e) = c.engines.get_mut(&engine) {
            e.1 = cap.max(1);
        }
    }
    if c.id.is_empty() || c.engines.is_empty() {
        return None;
    }
    Some(c)
}

#[derive(Default)]
struct FdScan {
    count: u32,
    drm: Vec<String>,
    nvidia: bool,
    swap: u64,
    sockets: Vec<u64>,
}

/// `VmSwap` from `/proc/[pid]/status`, in bytes.
pub fn parse_swap(status: &str) -> u64 {
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmSwap:"))
        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        .map_or(0, |kb| kb * 1024)
}

fn scan_fds(pid: i32) -> FdScan {
    let mut scan = FdScan { swap: parse_swap(&read(&format!("/proc/{pid}/status"))), ..Default::default() };
    let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { return scan };
    for e in dir.flatten() {
        scan.count += 1;
        let Ok(target) = std::fs::read_link(e.path()) else { continue };
        let t = target.to_string_lossy();
        if let Some(inode) = t.strip_prefix("socket:[").and_then(|r| r.strip_suffix(']')).and_then(|n| n.parse().ok()) {
            scan.sockets.push(inode);
        } else if t.starts_with("/dev/dri/") {
            scan.drm.push(e.file_name().to_string_lossy().to_string());
        } else if let Some(rest) = t.strip_prefix("/dev/nvidia")
            && rest.chars().next().is_some_and(|c| c.is_ascii_digit())
        {
            scan.nvidia = true;
        }
    }
    scan
}

fn users() -> HashMap<u32, String> {
    read("/etc/passwd")
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            Some((f.get(2)?.parse().ok()?, f.first()?.to_string()))
        })
        .collect()
}

/// What the GPU side learns from the process scan.
#[derive(Debug, Clone, Default)]
pub struct GpuClients {
    /// PCI address -> busiest engine %.
    pub busy: HashMap<String, f64>,
    /// PCI address -> number of processes holding it open.
    pub clients: HashMap<String, usize>,
    /// Processes that hold an NVIDIA device node open.
    pub nvidia: usize,
    /// PCI address -> each engine's busy %, by engine name.
    pub engines: HashMap<String, Vec<(String, f64)>>,
    /// PCI address -> memory its clients hold.
    pub memory: HashMap<String, u64>,
    /// Socket inode -> the process holding it (from the last fd scan).
    pub sockets: HashMap<u64, i32>,
}

/// Per pid: start time, CPU ticks, and storage I/O byte counters.
type Prev = (u64, u64, Option<(u64, u64)>);

/// Per pid and start time: name, command line, cgroup.
type Names = HashMap<(i32, u64), (String, String, String)>;

pub struct ProcSampler {
    prev: HashMap<i32, Prev>,
    names: Names,
    users: HashMap<u32, String>,
    fds: HashMap<i32, FdScan>,
    drm_prev: HashMap<(String, String), HashMap<String, u64>>,
    tick: u64,
    clk_tck: f64,
    page: u64,
    ncpu: f64,
    me: i32,
}

impl ProcSampler {
    pub fn new() -> Self {
        let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
        let ncpu = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
        ProcSampler {
            prev: HashMap::new(),
            names: HashMap::new(),
            users: users(),
            fds: HashMap::new(),
            drm_prev: HashMap::new(),
            tick: 0,
            clk_tck,
            page,
            ncpu,
            me: std::process::id() as i32,
        }
    }

    pub fn sample(&mut self, elapsed: f64) -> (Vec<Proc>, GpuClients) {
        let rescan = self.tick.is_multiple_of(5);
        self.tick += 1;
        let pids: Vec<i32> = std::fs::read_dir("/proc")
            .map(|d| d.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect())
            .unwrap_or_default();
        if rescan {
            self.fds = pids.iter().map(|&p| (p, scan_fds(p))).collect();
        }

        let mut procs = Vec::with_capacity(pids.len());
        let mut next_prev = HashMap::with_capacity(pids.len());
        let mut live: HashSet<(i32, u64)> = HashSet::new();
        let mut gpu = GpuClients::default();
        let mut drm_next: HashMap<(String, String), HashMap<String, u64>> = HashMap::new();
        let mut device_busy: HashMap<String, HashMap<String, f64>> = HashMap::new();
        let mut clients_seen: HashSet<(String, String)> = HashSet::new();
        let mut client_pids: HashMap<String, HashSet<i32>> = HashMap::new();

        for pid in pids {
            let base = format!("/proc/{pid}");
            let Some(stat) = parse_stat(&read(&format!("{base}/stat"))) else { continue };
            let uid = std::fs::metadata(&base).map(|m| m.uid()).unwrap_or(0);
            let kernel = pid == 2 || stat.ppid == 2;
            let key = (pid, stat.start);
            live.insert(key);
            let (name, cmdline, cgroup) = self
                .names
                .entry(key)
                .or_insert_with(|| {
                    let raw = std::fs::read(format!("{base}/cmdline")).unwrap_or_default();
                    let args: Vec<String> = raw
                        .split(|b| *b == 0)
                        .filter(|a| !a.is_empty())
                        .map(|a| String::from_utf8_lossy(a).to_string())
                        .collect();
                    // comm is cut at 15 characters; the program path has the full name.
                    let mut name = stat.comm.clone();
                    // For scripts it's the first argument (bash /usr/bin/omarchy-…).
                    if name.len() >= 15 {
                        let full = args.iter().take(2).find_map(|a| {
                            let base = a.rsplit('/').next().unwrap_or(a);
                            let base = base.split_whitespace().next().unwrap_or(base);
                            base.starts_with(&name).then(|| base.to_string())
                        });
                        if let Some(full) = full {
                            name = full;
                        }
                    }
                    // A process named after its main thread ("MainThread") goes by its program.
                    if name.ends_with("Thread")
                        && let Some(program) = args.first().and_then(|a| a.rsplit('/').next()).filter(|p| !p.is_empty())
                    {
                        name = program.to_string();
                    }
                    let cgroup = read(&format!("{base}/cgroup"))
                        .lines()
                        .find_map(|l| l.strip_prefix("0::"))
                        .unwrap_or_default()
                        .to_string();
                    (name, args.join(" "), cgroup)
                })
                .clone();
            let io = if kernel { None } else { parse_io(&read(&format!("{base}/io"))) };
            let mut p = Proc {
                pid,
                ppid: stat.ppid,
                name,
                cmdline,
                uid,
                user: self.users.get(&uid).cloned().unwrap_or_else(|| uid.to_string()),
                state: stat.state,
                threads: stat.threads,
                nice: stat.nice,
                rss: stat.rss_pages * self.page,
                start: stat.start as f64 / self.clk_tck,
                kernel,
                cgroup,
                ..Default::default()
            };
            if let Some(&(start, ticks, prev_io)) = self.prev.get(&pid)
                && start == stat.start
                && elapsed > 0.0
            {
                let used = stat.ticks.saturating_sub(ticks) as f64 / self.clk_tck;
                p.cpu = (used / elapsed / self.ncpu * 100.0).clamp(0.0, 100.0);
                if let (Some((r0, w0)), Some((r1, w1))) = (prev_io, io) {
                    p.read_bps = r1.saturating_sub(r0) as f64 / elapsed;
                    p.write_bps = w1.saturating_sub(w0) as f64 / elapsed;
                }
            }
            next_prev.insert(pid, (stat.start, stat.ticks, io));

            if let Some(scan) = self.fds.get(&pid) {
                p.fds = scan.count;
                p.nvidia = scan.nvidia;
                p.swap = scan.swap;
                for s in &scan.sockets {
                    gpu.sockets.insert(*s, pid);
                }
                if scan.nvidia && pid != self.me {
                    gpu.nvidia += 1;
                }
                let mut pid_engines: HashMap<String, f64> = HashMap::new();
                for fd in &scan.drm {
                    let Some(client) = parse_fdinfo(&read(&format!("{base}/fdinfo/{fd}"))) else { continue };
                    let ckey = (client.pdev.clone(), client.id.clone());
                    if pid != self.me {
                        client_pids.entry(client.pdev.clone()).or_default().insert(pid);
                    }
                    // Several fds can share one client; count it once.
                    if !clients_seen.insert(ckey.clone()) {
                        continue;
                    }
                    p.gpu_mem += client.resident;
                    *gpu.memory.entry(client.pdev.clone()).or_default() += client.resident;
                    let prev = self.drm_prev.get(&ckey);
                    let mut now = HashMap::new();
                    for (engine, (ns, cap)) in &client.engines {
                        now.insert(engine.clone(), *ns);
                        if let Some(before) = prev.and_then(|p| p.get(engine))
                            && elapsed > 0.0
                        {
                            let pct = ns.saturating_sub(*before) as f64 / (elapsed * 1e9) / *cap as f64 * 100.0;
                            *pid_engines.entry(engine.clone()).or_default() += pct;
                            *device_busy.entry(client.pdev.clone()).or_default().entry(engine.clone()).or_default() += pct;
                        }
                    }
                    drm_next.insert(ckey, now);
                }
                p.gpu = pid_engines.values().copied().fold(0.0, f64::max).min(100.0);
            }
            procs.push(p);
        }
        self.prev = next_prev;
        self.names.retain(|k, _| live.contains(k));
        self.drm_prev = drm_next;
        gpu.busy = device_busy.iter().map(|(dev, e)| (dev.clone(), e.values().copied().fold(0.0, f64::max).min(100.0))).collect();
        gpu.engines = device_busy
            .into_iter()
            .map(|(dev, e)| {
                let mut list: Vec<(String, f64)> = e.into_iter().map(|(n, v)| (n, v.min(100.0))).collect();
                list.sort_by(|a, b| a.0.cmp(&b.0));
                (dev, list)
            })
            .collect();
        gpu.clients = client_pids.into_iter().map(|(dev, pids)| (dev, pids.len())).collect();
        (procs, gpu)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_programs_not_threads() {
        let p = |name: &str, cmdline: &str| Proc { name: name.into(), cmdline: cmdline.into(), ..Default::default() };
        assert_eq!(p("MainThread", "/opt/cursor-agent/bin/cursor-agent --use-x").app_name(), "cursor-agent");
        assert_eq!(p("Web Content", "/usr/lib/firefox/firefox -contentproc").app_name(), "firefox");
        assert_eq!(p("blender", "blender scene.blend").app_name(), "blender");
        assert_eq!(p("MainThread", "").app_name(), "MainThread");
    }

    #[test]
    fn parses_stat_with_odd_names() {
        let text = "1234 (my (odd) app) S 1 1234 1234 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 7 0 5000 100000 2560 \
                    18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0";
        let s = parse_stat(text).unwrap();
        assert_eq!(s.comm, "my (odd) app");
        assert_eq!(s.state, 'S');
        assert_eq!(s.ppid, 1);
        assert_eq!(s.ticks, 300);
        assert_eq!(s.threads, 7);
        assert_eq!(s.start, 5000);
        assert_eq!(s.rss_pages, 2560);
    }

    #[test]
    fn parses_io() {
        let text = "rchar: 1\nwchar: 2\nsyscr: 3\nsyscw: 4\nread_bytes: 4096\nwrite_bytes: 8192\ncancelled_write_bytes: 0\n";
        assert_eq!(parse_io(text), Some((4096, 8192)));
    }

    #[test]
    fn parses_drm_fdinfo() {
        let text = "pos:\t0\ndrm-driver:\ti915\ndrm-client-id:\t162\ndrm-pdev:\t0000:00:02.0\n\
                    drm-engine-render:\t93903783376 ns\ndrm-engine-video:\t0 ns\ndrm-engine-capacity-video:\t2\n\
                    drm-total-system0:\t2048 KiB\ndrm-resident-system0:\t1024 KiB\ndrm-resident-stolen:\t2 MiB\n";
        let c = parse_fdinfo(text).unwrap();
        assert_eq!(c.pdev, "0000:00:02.0");
        assert_eq!(c.id, "162");
        assert_eq!(c.engines["render"], (93903783376, 1));
        assert_eq!(c.engines["video"], (0, 2));
        assert_eq!(c.resident, 3 * 1024 * 1024);
    }

    #[test]
    fn parses_swap() {
        assert_eq!(parse_swap("Name:\tfoo\nVmRSS:\t 100 kB\nVmSwap:\t    12 kB\n"), 12 * 1024);
        assert_eq!(parse_swap("Name:\tkthreadd\n"), 0);
    }
}
