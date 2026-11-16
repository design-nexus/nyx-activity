//! CPU usage from `/proc/stat`, frequencies from cpufreq, load, pressure and
//! the processor's layout (core types).

use super::read;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Times {
    pub user: u64,
    pub system: u64,
    /// Idle, including I/O wait.
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub steal: u64,
    pub total: u64,
}

impl Times {
    fn parse(fields: &[&str]) -> Option<Times> {
        let n: Vec<u64> = fields.iter().take(8).map(|f| f.parse().ok()).collect::<Option<_>>()?;
        if n.len() < 5 {
            return None;
        }
        let get = |i: usize| n.get(i).copied().unwrap_or(0);
        // user nice system idle iowait irq softirq steal
        Some(Times {
            user: get(0) + get(1),
            system: get(2),
            idle: get(3) + get(4),
            iowait: get(4),
            irq: get(5) + get(6),
            steal: get(7),
            total: n.iter().sum(),
        })
    }
}

/// Parsed `/proc/stat`: the aggregate line, each core, and the counters.
#[derive(Debug, Clone, Default)]
pub struct Stat {
    pub total: Times,
    pub cores: Vec<Times>,
}

pub fn parse_stat(text: &str) -> Stat {
    let mut stat = Stat::default();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else { continue };
        match key {
            "cpu" => stat.total = Times::parse(&parts.collect::<Vec<_>>()).unwrap_or_default(),
            k if k.starts_with("cpu") => {
                if let Some(t) = Times::parse(&parts.collect::<Vec<_>>()) {
                    stat.cores.push(t);
                }
            }
            _ => {}
        }
    }
    stat
}

/// Where the time went between two readings, each 0–100.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Split {
    pub busy: f64,
    pub user: f64,
    pub system: f64,
    pub iowait: f64,
    pub irq: f64,
    pub steal: f64,
}

pub fn split(prev: &Times, now: &Times) -> Split {
    let total = now.total.saturating_sub(prev.total) as f64;
    if total <= 0.0 {
        return Split::default();
    }
    let d = |a: u64, b: u64| a.saturating_sub(b) as f64 / total * 100.0;
    let idle = d(now.idle, prev.idle);
    Split {
        busy: (100.0 - idle).clamp(0.0, 100.0),
        user: d(now.user, prev.user),
        system: d(now.system, prev.system),
        iowait: d(now.iowait, prev.iowait),
        irq: d(now.irq, prev.irq),
        steal: d(now.steal, prev.steal),
    }
}

/// `/proc/pressure/*`: the share of time some (or all) tasks were stalled.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Pressure {
    pub some10: f64,
    pub some60: f64,
    pub some300: f64,
    pub full10: f64,
    pub full60: f64,
}

pub fn parse_pressure(text: &str) -> Pressure {
    let mut p = Pressure::default();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let kind = parts.next();
        let mut avg = [0.0; 3];
        for f in parts {
            if let Some((k, v)) = f.split_once('=') {
                let v = v.parse().unwrap_or(0.0);
                match k {
                    "avg10" => avg[0] = v,
                    "avg60" => avg[1] = v,
                    "avg300" => avg[2] = v,
                    _ => {}
                }
            }
        }
        match kind {
            Some("some") => (p.some10, p.some60, p.some300) = (avg[0], avg[1], avg[2]),
            Some("full") => (p.full10, p.full60) = (avg[0], avg[1]),
            _ => {}
        }
    }
    p
}

pub fn pressure(resource: &str) -> Option<Pressure> {
    let text = read(&format!("/proc/pressure/{resource}"));
    (!text.is_empty()).then(|| parse_pressure(&text))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, PartialOrd, Ord, Hash)]
pub enum CoreKind {
    Performance,
    Efficient,
    LowPower,
    #[default]
    Uniform,
}

impl CoreKind {
    pub fn name(self) -> &'static str {
        match self {
            CoreKind::Performance => "Performance cores",
            CoreKind::Efficient => "Efficient cores",
            CoreKind::LowPower => "Low-power cores",
            CoreKind::Uniform => "Cores",
        }
    }
}

/// What doesn't change while the machine runs.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    pub model: String,
    /// Physical cores (distinct core ids), when known.
    pub physical: usize,
    pub kinds: Vec<CoreKind>,
    /// Each thread's core id (coretemp names its sensors "Core <id>").
    pub core_ids: Vec<u32>,
    pub max_freqs: Vec<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct Cpu {
    pub layout: std::sync::Arc<Layout>,
    pub model: String,
    /// Busy % of all cores together.
    pub usage: f64,
    pub user: f64,
    pub system: f64,
    pub iowait: f64,
    pub irq: f64,
    pub steal: f64,
    pub cores: Vec<f64>,
    pub core_split: Vec<Split>,
    /// Current frequency of each core, MHz.
    pub freqs: Vec<f64>,
    pub max_freq: f64,
    pub governor: String,
    /// Energy/performance preference (intel_pstate, amd-pstate).
    pub epp: String,
    pub platform_profile: String,
    pub turbo: Option<bool>,
    pub load: [f64; 3],
    pub uptime: f64,
    pub pressure: Option<Pressure>,
    /// Physical cores (distinct core ids), when known.
    pub physical: usize,
}

fn cpulist(text: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for part in text.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) {
                    out.extend(a..=b);
                }
            }
            None => out.extend(part.parse::<usize>().ok()),
        }
    }
    out
}

fn layout(threads: usize) -> Layout {
    let info = read("/proc/cpuinfo");
    let field = |name: &str| {
        info.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.split_whitespace().collect::<Vec<_>>().join(" "))
    };
    let model = field("model name").unwrap_or_else(|| "Processor".into());
    let sys = |i: usize, f: &str| read(&format!("/sys/devices/system/cpu/cpu{i}/{f}"));
    let core_ids: Vec<u32> = (0..threads).map(|i| sys(i, "topology/core_id").trim().parse().unwrap_or(i as u32)).collect();
    let pkg: Vec<String> = (0..threads).map(|i| sys(i, "topology/physical_package_id").trim().to_string()).collect();
    let mut ids: Vec<(String, u32)> = pkg.iter().cloned().zip(core_ids.iter().copied()).collect();
    ids.sort();
    ids.dedup();
    let max_freqs: Vec<f64> =
        (0..threads).map(|i| sys(i, "cpufreq/cpuinfo_max_freq").trim().parse::<f64>().unwrap_or(0.0) / 1000.0).collect();

    // Hybrid Intel parts list their P and E cores; the low-power island is in neither.
    let p = cpulist(&read("/sys/devices/cpu_core/cpus"));
    let e = cpulist(&read("/sys/devices/cpu_atom/cpus"));
    let kinds = (0..threads)
        .map(|i| {
            if p.is_empty() && e.is_empty() {
                CoreKind::Uniform
            } else if p.contains(&i) {
                CoreKind::Performance
            } else if e.contains(&i) {
                CoreKind::Efficient
            } else {
                CoreKind::LowPower
            }
        })
        .collect();

    Layout { model, physical: ids.len(), kinds, core_ids, max_freqs }
}

pub struct CpuSampler {
    prev: Option<Stat>,
    layout: std::sync::Arc<Layout>,
    max_freq: f64,
}

impl CpuSampler {
    pub fn new() -> Self {
        let threads = parse_stat(&read("/proc/stat")).cores.len();
        let layout = layout(threads);
        let max_freq = layout.max_freqs.iter().copied().fold(0.0, f64::max);
        CpuSampler { prev: None, layout: std::sync::Arc::new(layout), max_freq }
    }

    pub fn sample(&mut self) -> Cpu {
        let stat = parse_stat(&read("/proc/stat"));
        let mut cpu = Cpu {
            layout: self.layout.clone(),
            model: self.layout.model.clone(),
            physical: self.layout.physical,
            max_freq: self.max_freq,
            ..Default::default()
        };
        if let Some(prev) = &self.prev {
            let s = split(&prev.total, &stat.total);
            (cpu.usage, cpu.user, cpu.system, cpu.iowait, cpu.irq, cpu.steal) =
                (s.busy, s.user, s.system, s.iowait, s.irq, s.steal);
            cpu.core_split = stat.cores.iter().zip(&prev.cores).map(|(n, p)| split(p, n)).collect();
            cpu.cores = cpu.core_split.iter().map(|s| s.busy).collect();
        } else {
            cpu.cores = vec![0.0; stat.cores.len()];
            cpu.core_split = vec![Split::default(); stat.cores.len()];
        }
        cpu.freqs = (0..stat.cores.len())
            .map(|i| {
                read(&format!("/sys/devices/system/cpu/cpu{i}/cpufreq/scaling_cur_freq")).trim().parse::<f64>().unwrap_or(0.0)
                    / 1000.0
            })
            .collect();
        cpu.governor = read("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor").trim().to_string();
        cpu.epp = read("/sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference").trim().replace('_', " ");
        cpu.platform_profile = read("/sys/firmware/acpi/platform_profile").trim().replace('-', " ");
        cpu.turbo = match read("/sys/devices/system/cpu/intel_pstate/no_turbo").trim() {
            "0" => Some(true),
            "1" => Some(false),
            _ => match read("/sys/devices/system/cpu/cpufreq/boost").trim() {
                "1" => Some(true),
                "0" => Some(false),
                _ => None,
            },
        };
        let load = read("/proc/loadavg");
        for (i, v) in load.split_whitespace().take(3).enumerate() {
            cpu.load[i] = v.parse().unwrap_or(0.0);
        }
        cpu.uptime = read("/proc/uptime").split_whitespace().next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        cpu.pressure = pressure("cpu");
        self.prev = Some(stat);
        cpu
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "cpu  100 0 50 800 50 0 0 0 0 0\n\
                        cpu0 50 0 25 400 25 0 0 0 0 0\n\
                        cpu1 50 0 25 400 25 0 0 0 0 0\n\
                        intr 1234 2 3\nctxt 5000\nbtime 1\nprocesses 77\nprocs_running 3\nprocs_blocked 1\n";

    #[test]
    fn parses_proc_stat() {
        let s = parse_stat(STAT);
        assert_eq!(s.cores.len(), 2);
        assert_eq!(s.total.total, 1000);
        assert_eq!(s.total.idle, 850);
        assert_eq!(s.total.iowait, 50);
    }

    #[test]
    fn splits_time_between_readings() {
        let a = Times::default();
        let b = Times { user: 30, system: 10, idle: 55, iowait: 5, irq: 5, steal: 0, total: 100 };
        let s = split(&a, &b);
        assert!((s.busy - 45.0).abs() < 1e-9);
        assert!((s.user - 30.0).abs() < 1e-9);
        assert!((s.system - 10.0).abs() < 1e-9);
        assert!((s.iowait - 5.0).abs() < 1e-9);
        assert!((s.irq - 5.0).abs() < 1e-9);
    }

    #[test]
    fn parses_pressure() {
        let p = parse_pressure(
            "some avg10=1.50 avg60=0.21 avg300=4.49 total=4370012819\nfull avg10=0.25 avg60=0.10 avg300=0.00 total=0\n",
        );
        assert_eq!((p.some10, p.some60, p.some300), (1.5, 0.21, 4.49));
        assert_eq!((p.full10, p.full60), (0.25, 0.10));
    }

    #[test]
    fn reads_cpu_lists() {
        assert_eq!(cpulist("0-2,6,8-9\n"), vec![0, 1, 2, 6, 8, 9]);
        assert_eq!(cpulist(""), Vec::<usize>::new());
    }
}
