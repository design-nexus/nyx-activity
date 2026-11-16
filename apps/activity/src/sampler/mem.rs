//! Memory, swap and zram from `/proc/meminfo` and `/sys/block/zram*`, paging
//! activity from `/proc/vmstat`, and memory pressure.

use super::cpu::{Pressure, pressure};
use super::read;
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct Mem {
    pub total: u64,
    pub available: u64,
    pub used: u64,
    /// Page cache the kernel can drop (file pages, reclaimable slab, buffers), less shared memory.
    pub cached: u64,
    pub buffers: u64,
    pub shared: u64,
    /// Program memory (heaps and stacks): `AnonPages`.
    pub anon: u64,
    /// Memory the kernel holds for itself: unreclaimable slab, page tables, stacks, vmalloc.
    pub kernel: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    /// Uncompressed data held in zram, and the RAM it actually takes.
    pub zram_data: u64,
    pub zram_used: u64,
    pub pressure: Option<Pressure>,
}

/// `/proc/meminfo` as bytes by key.
pub fn parse_meminfo(text: &str) -> HashMap<String, u64> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            let mut parts = v.split_whitespace();
            let n: u64 = parts.next()?.parse().ok()?;
            let mult = if parts.next() == Some("kB") { 1024 } else { 1 };
            Some((k.to_string(), n * mult))
        })
        .collect()
}

pub fn from_meminfo(info: &HashMap<String, u64>) -> Mem {
    let g = |k: &str| info.get(k).copied().unwrap_or(0);
    let total = g("MemTotal");
    let available = if info.contains_key("MemAvailable") { g("MemAvailable") } else { g("MemFree") + g("Cached") };
    Mem {
        total,
        available,
        used: total.saturating_sub(available),
        cached: (g("Cached") + g("SReclaimable")).saturating_sub(g("Shmem")),
        buffers: g("Buffers"),
        shared: g("Shmem"),
        anon: g("AnonPages"),
        kernel: g("SUnreclaim") + g("PageTables") + g("KernelStack") + g("VmallocUsed") + g("Percpu"),
        swap_total: g("SwapTotal"),
        swap_used: g("SwapTotal").saturating_sub(g("SwapFree")),
        ..Default::default()
    }
}

#[derive(Default)]
pub struct MemSampler;

impl MemSampler {
    pub fn new() -> Self {
        MemSampler
    }

    pub fn sample(&mut self) -> Mem {
        let mut mem = from_meminfo(&parse_meminfo(&read("/proc/meminfo")));
        if let Ok(entries) = std::fs::read_dir("/sys/block") {
            for e in entries.flatten() {
                if !e.file_name().to_string_lossy().starts_with("zram") {
                    continue;
                }
                // orig_data_size compr_data_size mem_used_total …
                let stat = read(&e.path().join("mm_stat").to_string_lossy());
                let n: Vec<u64> = stat.split_whitespace().filter_map(|v| v.parse().ok()).collect();
                if n.len() >= 3 {
                    mem.zram_data += n[0];
                    mem.zram_used += n[2];
                }
            }
        }
        mem.pressure = pressure("memory");
        mem
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_meminfo() {
        let text = "MemTotal:       16000 kB\nMemFree:  1000 kB\nMemAvailable:   6000 kB\nBuffers: 100 kB\n\
                    Cached: 4000 kB\nSReclaimable: 500 kB\nSUnreclaim: 200 kB\nShmem: 300 kB\nAnonPages: 7000 kB\n\
                    PageTables: 50 kB\nKernelStack: 20 kB\nSwapTotal: 8000 kB\nSwapFree: 7000 kB\n\
                    HugePages_Total: 0\n";
        let m = from_meminfo(&parse_meminfo(text));
        assert_eq!(m.total, 16000 * 1024);
        assert_eq!(m.used, 10000 * 1024);
        assert_eq!(m.cached, 4200 * 1024);
        assert_eq!(m.anon, 7000 * 1024);
        assert_eq!(m.kernel, 270 * 1024);
        assert_eq!(m.swap_used, 1000 * 1024);
    }
}
