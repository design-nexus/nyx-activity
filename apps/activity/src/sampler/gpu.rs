//! GPUs. Intel and AMD are read from sysfs (and DRM fdinfo, via the process
//! scan). NVIDIA is queried with `nvidia-smi`, but only while something else
//! already has it awake: asking a sleeping dGPU would wake it and cost battery.

use super::procs::GpuClients;
use super::read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Vendor {
    Intel,
    Nvidia,
    Amd,
    #[default]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Power {
    #[default]
    Active,
    /// Awake but nothing is using it; we don't query it so it can sleep.
    Idle,
    /// Runtime-suspended (powered down).
    Asleep,
}

#[derive(Debug, Clone, Default)]
pub struct Gpu {
    pub pci: String,
    pub name: String,
    pub vendor: Vendor,
    pub driver: String,
    pub power_state: Power,
    pub clients: usize,
    pub util: Option<f64>,
    pub vram_used: Option<u64>,
    pub vram_total: Option<u64>,
    pub temp: Option<f64>,
    pub watts: Option<f64>,
    pub watts_limit: Option<f64>,
    pub clock: Option<f64>,
    pub max_clock: Option<f64>,
    /// The clock it actually ran at (Intel: 0 while asleep in RC6).
    pub act_clock: Option<f64>,
    /// % of the time spent in its deepest power-saving state (Intel RC6).
    pub rc6: Option<f64>,
    /// Each engine's busy % (render, video, copy, compute), from the clients' fdinfo.
    pub engines: Vec<(String, f64)>,
    /// Memory the apps using it hold (fdinfo `drm-resident-*`).
    pub client_mem: Option<u64>,
    /// Video memory per process, where the driver reports it (NVIDIA compute apps).
    pub proc_mem: Vec<(i32, u64)>,
}

/// `nvidia-smi --query-compute-apps=pid,used_memory --format=csv,noheader,nounits` (MiB).
pub fn parse_compute_apps(text: &str) -> Vec<(i32, u64)> {
    text.lines()
        .filter_map(|l| {
            let (pid, mem) = l.split_once(',')?;
            Some((pid.trim().parse().ok()?, mem.trim().parse::<u64>().ok()? * 1024 * 1024))
        })
        .collect()
}

struct Card {
    dir: PathBuf,
    pci: String,
    vendor: Vendor,
    driver: String,
    name: String,
}

/// Look a device up in pci.ids ("GB203M / GN22-X9 [GeForce RTX 5080 Max-Q / Mobile]").
/// Reading sysfs ids and this file never touches the hardware, unlike `lspci`,
/// which reads config space and would wake a sleeping GPU.
pub fn pci_ids_name(ids: &str, vendor: &str, device: &str) -> Option<String> {
    let mut in_vendor = false;
    for line in ids.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if !line.starts_with('\t') {
            if in_vendor {
                return None;
            }
            in_vendor = line.starts_with(vendor) && line.as_bytes().get(4) == Some(&b' ');
            continue;
        }
        if in_vendor && !line.starts_with("\t\t") {
            let rest = &line[1..];
            if rest.starts_with(device) && rest.as_bytes().get(4) == Some(&b' ') {
                return Some(rest[4..].trim().to_string());
            }
        }
    }
    None
}

fn pretty_name(desc: &str, vendor: Vendor) -> String {
    let desc = desc.trim();
    let brand = match vendor {
        Vendor::Intel => "Intel",
        Vendor::Nvidia => "NVIDIA",
        Vendor::Amd => "AMD",
        Vendor::Other => "",
    };
    if let (Some(a), Some(b)) = (desc.rfind('['), desc.rfind(']'))
        && a < b
    {
        return format!("{brand} {}", &desc[a + 1..b]).trim().to_string();
    }
    if desc.is_empty() { format!("{brand} GPU").trim().to_string() } else { format!("{brand} {desc}").trim().to_string() }
}

fn cards() -> Vec<Card> {
    let mut out: Vec<Card> = Vec::new();
    let Ok(dir) = std::fs::read_dir("/sys/class/drm") else { return out };
    let mut entries: Vec<_> = dir.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
    entries.sort();
    for name in entries {
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let dir = PathBuf::from("/sys/class/drm").join(&name);
        let Ok(dev) = std::fs::canonicalize(dir.join("device")) else { continue };
        let pci = dev.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if out.iter().any(|c| c.pci == pci) {
            continue;
        }
        let vendor = match read(&dev.join("vendor").to_string_lossy()).trim() {
            "0x8086" => Vendor::Intel,
            "0x10de" => Vendor::Nvidia,
            "0x1002" => Vendor::Amd,
            _ => Vendor::Other,
        };
        let driver = std::fs::read_link(dev.join("driver"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_default();
        let hex = |f: &str| read(&dev.join(f).to_string_lossy()).trim().trim_start_matches("0x").to_lowercase();
        let ids = pci_ids_paths().iter().map(|p| read(&p.to_string_lossy())).find(|t| !t.is_empty());
        let desc = ids.and_then(|t| pci_ids_name(&t, &hex("vendor"), &hex("device"))).unwrap_or_default();
        out.push(Card { dir, pci, vendor, driver, name: pretty_name(&desc, vendor) });
    }
    out
}

/// Where pci.ids may be: the usual fixed places, then `hwdata/pci.ids` under each
/// XDG data folder (NixOS has no /usr/share).
fn pci_ids_paths() -> Vec<PathBuf> {
    let xdg = std::env::var("XDG_DATA_DIRS").unwrap_or_default();
    ["/usr/share/hwdata/pci.ids", "/usr/share/misc/pci.ids"]
        .map(PathBuf::from)
        .into_iter()
        .chain(xdg.split(':').filter(|d| d.starts_with('/')).map(|d| Path::new(d).join("hwdata/pci.ids")))
        .collect()
}

fn num(path: &Path) -> Option<f64> {
    read(&path.to_string_lossy()).trim().parse().ok()
}

fn hwmon(dev: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dev.join("hwmon")).ok()?.flatten().next().map(|e| e.path())
}

/// Parse one line of `nvidia-smi --query-gpu=utilization.gpu,memory.used,memory.total,
/// temperature.gpu,power.draw,power.limit,clocks.gr,clocks.max.gr --format=csv,noheader,nounits`.
pub fn parse_nvidia_smi(line: &str, gpu: &mut Gpu) {
    let f: Vec<Option<f64>> = line.split(',').map(|v| v.trim().parse::<f64>().ok()).collect();
    let g = |i: usize| f.get(i).copied().flatten();
    gpu.util = g(0);
    gpu.vram_used = g(1).map(|m| (m * 1024.0 * 1024.0) as u64);
    gpu.vram_total = g(2).map(|m| (m * 1024.0 * 1024.0) as u64);
    gpu.temp = g(3);
    gpu.watts = g(4);
    gpu.watts_limit = g(5);
    gpu.clock = g(6);
    gpu.max_clock = g(7);
}

pub struct GpuSampler {
    cards: Vec<Card>,
    tick: u64,
    last_nvidia: Option<Gpu>,
    /// Consecutive idle NVIDIA readings; after a few we stop asking for a while
    /// so our own queries don't keep the GPU from going back to sleep.
    idle_reads: u32,
    rest_until: Option<std::time::Instant>,
    /// Per card: when RC6 residency was last read, and its value (ms).
    rc6_prev: std::collections::HashMap<String, (std::time::Instant, f64)>,
}

impl GpuSampler {
    pub fn new() -> Self {
        GpuSampler {
            cards: cards(),
            tick: 0,
            last_nvidia: None,
            idle_reads: 0,
            rest_until: None,
            rc6_prev: std::collections::HashMap::new(),
        }
    }

    pub fn sample(&mut self, clients: &GpuClients) -> Vec<Gpu> {
        self.tick += 1;
        let mut out = Vec::new();
        for card in &self.cards {
            let dev = card.dir.join("device");
            let mut g = Gpu {
                pci: card.pci.clone(),
                name: card.name.clone(),
                vendor: card.vendor,
                driver: card.driver.clone(),
                clients: clients.clients.get(&card.pci).copied().unwrap_or(0),
                engines: clients.engines.get(&card.pci).cloned().unwrap_or_default(),
                client_mem: clients.memory.get(&card.pci).copied().filter(|m| *m > 0),
                ..Default::default()
            };
            let runtime = read(&dev.join("power/runtime_status").to_string_lossy()).trim().to_string();
            if runtime == "suspended" {
                g.power_state = Power::Asleep;
                out.push(g);
                continue;
            }
            match card.vendor {
                Vendor::Nvidia => {
                    g.clients = g.clients.max(clients.nvidia);
                    let resting = self.rest_until.is_some_and(|t| std::time::Instant::now() < t);
                    if g.clients == 0 || resting {
                        g.power_state = Power::Idle;
                        self.last_nvidia = None;
                    } else if self.tick.is_multiple_of(2) || self.last_nvidia.is_none() {
                        let out = Command::new("nvidia-smi")
                            .args([
                                "-i",
                                &card.pci,
                                "--query-gpu=utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,power.limit,clocks.gr,clocks.max.gr",
                                "--format=csv,noheader,nounits",
                            ])
                            .stdin(Stdio::null())
                            .stderr(Stdio::null())
                            .output();
                        if let Ok(o) = out {
                            parse_nvidia_smi(String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or(""), &mut g);
                        }
                        let apps = Command::new("nvidia-smi")
                            .args(["-i", &card.pci, "--query-compute-apps=pid,used_memory", "--format=csv,noheader,nounits"])
                            .stdin(Stdio::null())
                            .stderr(Stdio::null())
                            .output();
                        if let Ok(o) = apps {
                            g.proc_mem = parse_compute_apps(&String::from_utf8_lossy(&o.stdout));
                        }
                        if g.util.unwrap_or(0.0) < 1.0 {
                            self.idle_reads += 1;
                            if self.idle_reads >= 5 {
                                self.idle_reads = 0;
                                self.rest_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(30));
                            }
                        } else {
                            self.idle_reads = 0;
                        }
                        self.last_nvidia = Some(g.clone());
                    } else if let Some(last) = &self.last_nvidia {
                        let clients = g.clients;
                        g = last.clone();
                        g.clients = clients;
                    }
                }
                Vendor::Intel => {
                    g.util = Some(clients.busy.get(&card.pci).copied().unwrap_or(0.0));
                    g.clock = num(&card.dir.join("gt_cur_freq_mhz")).or_else(|| num(&dev.join("tile0/gt0/freq0/cur_freq")));
                    g.max_clock = num(&card.dir.join("gt_max_freq_mhz")).or_else(|| num(&dev.join("tile0/gt0/freq0/max_freq")));
                    g.act_clock = num(&card.dir.join("gt_act_freq_mhz")).or_else(|| num(&dev.join("tile0/gt0/freq0/act_freq")));
                    let rc6 = num(&card.dir.join("gt/gt0/rc6_residency_ms"))
                        .or_else(|| num(&dev.join("tile0/gt0/gtidle/idle_residency_ms")));
                    if let Some(now) = rc6 {
                        let t = std::time::Instant::now();
                        if let Some((at, before)) = self.rc6_prev.get(&card.pci) {
                            let wall = t.duration_since(*at).as_secs_f64() * 1000.0;
                            if wall > 0.0 {
                                g.rc6 = Some(((now - before) / wall * 100.0).clamp(0.0, 100.0));
                            }
                        }
                        self.rc6_prev.insert(card.pci.clone(), (t, now));
                    }
                }
                Vendor::Amd => {
                    g.util = num(&dev.join("gpu_busy_percent"));
                    g.vram_used = num(&dev.join("mem_info_vram_used")).map(|v| v as u64);
                    g.vram_total = num(&dev.join("mem_info_vram_total")).map(|v| v as u64);
                    if let Some(h) = hwmon(&dev) {
                        g.temp = num(&h.join("temp1_input")).map(|t| t / 1000.0);
                        g.watts = num(&h.join("power1_average")).or_else(|| num(&h.join("power1_input"))).map(|w| w / 1e6);
                        g.clock = num(&h.join("freq1_input")).map(|f| f / 1e6);
                    }
                }
                Vendor::Other => {
                    g.util = clients.busy.get(&card.pci).copied();
                }
            }
            out.push(g);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_from_pci_ids() {
        let ids = "# comment\n10de  NVIDIA Corporation\n\t2c58  Other\n\t2c59  GB203M / GN22-X9 [GeForce RTX 5080 Max-Q / Mobile]\n\t\t1043 3a1f  Sub\n10df  Emulex\n\t2c59  Wrong\n";
        let d = pci_ids_name(ids, "10de", "2c59").unwrap();
        assert_eq!(pretty_name(&d, Vendor::Nvidia), "NVIDIA GeForce RTX 5080 Max-Q / Mobile");
        assert_eq!(pci_ids_name(ids, "10de", "ffff"), None);
    }

    #[test]
    fn parses_compute_apps() {
        assert_eq!(parse_compute_apps("3264, 5120\n12, [N/A]\n"), vec![(3264, 5120 * 1024 * 1024)]);
    }

    #[test]
    fn parses_nvidia_smi_csv() {
        let mut g = Gpu::default();
        parse_nvidia_smi("37, 1024, 16303, 51, 23.45, [N/A], 1200, 3090", &mut g);
        assert_eq!(g.util, Some(37.0));
        assert_eq!(g.vram_used, Some(1024 * 1024 * 1024));
        assert_eq!(g.temp, Some(51.0));
        assert_eq!(g.watts_limit, None);
        assert_eq!(g.max_clock, Some(3090.0));
    }
}
