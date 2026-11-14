//! Every page in the sidebar, in order.

use crate::sampler::Snapshot;
use crate::widgets::Page;
use crate::{fmt, paths};
use std::path::PathBuf;

pub mod cpu;
pub mod dashboard;
pub mod gpu;
pub mod memory;
pub mod network;
pub mod processes;
pub mod sensors;
pub mod services;
pub mod settings;
pub mod startup;
pub mod storage;
pub mod top;

pub struct Section {
    pub id: &'static str,
    pub title: &'static str,
    pub icon: &'static str,
    pub group: &'static str,
    pub description: &'static str,
    /// Extra words the search should find this page by.
    pub keywords: &'static str,
    /// Files the "Open config" button offers.
    pub files: fn() -> Vec<PathBuf>,
    pub build: fn(&Page),
    /// A live value shown beside the page's name in the sidebar.
    pub readout: Option<fn(&Snapshot) -> String>,
    /// The page manages its own scrolling (tables).
    pub fill: bool,
    /// The page's own button for the header.
    pub header: Option<fn() -> gtk::Widget>,
    /// A count to call attention to, as a badge beside the page's name.
    pub badge: Option<fn() -> u64>,
}

pub fn all() -> Vec<Section> {
    vec![
        Section {
            id: "overview",
            title: "Dashboard",
            icon: "view-grid-symbolic",
            group: "",
            description: "What needs a look, then cards for everything: CPU, memory, disks, network, GPU, sensors and the busiest apps.",
            keywords: "overview dashboard summary glance top problems warnings cards",
            files: Vec::new,
            build: dashboard::build,
            readout: None,
            fill: true,
            header: Some(dashboard::header_button),
            badge: None,
        },
        Section {
            id: "processes",
            title: "Processes",
            icon: "activity-processes-symbolic",
            group: "",
            description: "Every running program. End, pause or reprioritize any of them.",
            keywords: "tasks apps kill end terminate stop signal nice priority pid tree",
            files: Vec::new,
            build: processes::build,
            readout: Some(|s| if s.process_count > 0 { s.process_count.to_string() } else { String::new() }),
            fill: true,
            header: None,
            badge: None,
        },
        // ----- Performance -----
        Section {
            id: "cpu",
            title: "CPU",
            icon: "activity-cpu-symbolic",
            group: "Performance",
            description: "Processor usage per core, clock speeds and load.",
            keywords: "processor cores threads frequency clock load average governor",
            files: Vec::new,
            build: cpu::build,
            readout: Some(|s| fmt::pct(s.cpu.usage)),
            fill: false,
            header: None,
            badge: None,
        },
        Section {
            id: "memory",
            title: "Memory",
            icon: "activity-memory-symbolic",
            group: "Performance",
            description: "RAM, cache, swap and compressed memory.",
            keywords: "ram swap zram cache buffers available",
            files: Vec::new,
            build: memory::build,
            readout: Some(
                |s| if s.mem.total > 0 { fmt::pct(s.mem.used as f64 / s.mem.total as f64 * 100.0) } else { String::new() },
            ),
            fill: false,
            header: None,
            badge: None,
        },
        Section {
            id: "storage",
            title: "Storage",
            icon: "drive-harddisk-symbolic",
            group: "Performance",
            description: "Disk activity and how full each drive is.",
            keywords: "disk ssd nvme drive read write space usage mount filesystem",
            files: Vec::new,
            build: storage::build,
            readout: Some(|s| fmt::pct(s.disk_busy())),
            fill: false,
            header: None,
            badge: None,
        },
        Section {
            id: "network",
            title: "Network",
            icon: "network-wireless-symbolic",
            group: "Performance",
            description: "Traffic on each connection, addresses and Wi-Fi signal.",
            keywords: "wifi ethernet download upload bandwidth ip address traffic",
            files: Vec::new,
            build: network::build,
            readout: Some(|s| format!("↓ {}", crate::fmt::net_rate(s.net_rx()))),
            fill: false,
            header: None,
            badge: None,
        },
        Section {
            id: "gpu",
            title: "GPU",
            icon: "activity-gpu-symbolic",
            group: "Performance",
            description: "Graphics usage, video memory, clocks and power.",
            keywords: "graphics nvidia intel amd vram video card",
            files: Vec::new,
            build: gpu::build,
            readout: Some(|s| {
                s.gpus
                    .iter()
                    .filter_map(|g| g.util)
                    .fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))))
                    .map(fmt::pct)
                    .unwrap_or_default()
            }),
            fill: false,
            header: None,
            badge: None,
        },
        Section {
            id: "sensors",
            title: "Sensors",
            icon: "activity-thermometer-symbolic",
            group: "Performance",
            description: "Temperatures, fans, the battery and power.",
            keywords: "temperature thermal fan rpm battery charge power watts hwmon charger usb-c profile health",
            files: Vec::new,
            build: sensors::build,
            readout: Some(|s| s.sensors.cpu_temp.map(fmt::temp).unwrap_or_default()),
            fill: false,
            header: None,
            badge: None,
        },
        // ----- System -----
        Section {
            id: "startup",
            title: "Startup",
            icon: "system-reboot-symbolic",
            group: "System",
            description: "What starts when you log in, and turning it off.",
            keywords: "autostart login boot exec once launch_on_start xdg desktop session",
            files: || vec![paths::user_autostart_lua(), paths::managed_lua()],
            build: startup::build,
            readout: None,
            fill: false,
            header: None,
            badge: None,
        },
        Section {
            id: "services",
            title: "Services",
            icon: "emblem-system-symbolic",
            group: "System",
            description: "Background services run by systemd.",
            keywords: "systemd units daemon enable disable restart failed logs journal",
            files: Vec::new,
            build: services::build,
            readout: None,
            fill: true,
            header: None,
            badge: Some(|| crate::alerts::failed().len() as u64),
        },
        // ----- App -----
        Section {
            id: "settings",
            title: "Settings",
            icon: "preferences-system-symbolic",
            group: "App",
            description: "How this window looks and how often it samples.",
            keywords: "theme colours colors follow omarchy accent motion interval history bits bytes temperature celsius fahrenheit units",
            files: || vec![paths::prefs_file()],
            build: settings::build,
            readout: None,
            fill: false,
            header: None,
            badge: None,
        },
    ]
}
