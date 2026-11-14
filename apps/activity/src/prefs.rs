//! This app's own preferences (`~/.config/nyx-activity/settings.toml`).

pub use nyx_theme::ThemeMode;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub mode: ThemeMode,
    pub theme: String,
    pub reduce_motion: bool,
    /// The sidebar shows only icons, whatever the window width.
    pub sidebar_collapsed: bool,
    pub glow: bool,
    /// How often to sample, in milliseconds.
    pub interval_ms: u64,
    /// How much history the graphs keep, in seconds.
    pub history_secs: u64,
    /// Show network rates in bits per second instead of bytes.
    pub net_bits: bool,
    /// Show temperatures in °F instead of °C.
    pub fahrenheit: bool,
    /// Ask before ending a task.
    pub confirm_kill: bool,
    /// List kernel threads in Processes.
    pub kernel_threads: bool,
    /// Stop collecting per-process and GPU data while the window is hidden.
    pub pause_hidden: bool,
    /// Columns hidden in Processes.
    pub hidden_columns: Vec<String>,
    /// Processes view: apps, all or mine.
    pub process_view: String,
    /// All and Mine: processes indented under the ones that started them.
    pub process_tree: bool,
    /// 2 once Processes moved to apps first (the columns and view were reset to its defaults).
    /// Missing from older files, so it reads as 0 there rather than the default.
    #[serde(default)]
    pub processes_layout: u32,
    /// Processes column widths in pixels, by column id.
    pub column_widths: std::collections::HashMap<String, i32>,
    /// Processes column order, by column id.
    pub column_order: Vec<String>,
    /// Processes sort column id and direction.
    pub sort_column: String,
    pub sort_desc: bool,
    /// Dashboard cards in order, by id (see `dashboard::CARDS`); ones missing go at the end.
    pub dashboard_order: Vec<String>,
    /// Dashboard cards turned off.
    pub dashboard_hidden: Vec<String>,
    /// The sidebar is an icon rail on the Dashboard, whatever the window width.
    pub dashboard_rail: bool,
    /// The CPU page shows each thread as a small tile instead of a graph.
    pub cpu_grid: bool,
    /// Expanded diagnostic groups and selected consumer views, keyed by page/device.
    pub expanded_groups: std::collections::HashMap<String, bool>,
    pub consumer_views: std::collections::HashMap<String, String>,
    /// Warning thresholds: CPU temperature (°C), memory use and disk fullness (%).
    pub warn_temp: f64,
    pub warn_mem: f64,
    pub warn_disk: f64,
    /// Desktop notifications: CPU temperature, memory and a disk past their warning
    /// thresholds, a process over `alert_proc_cpu`% for `alert_proc_secs`, a failed service.
    pub alert_temp: bool,
    pub alert_mem: bool,
    pub alert_disk: bool,
    pub alert_proc: bool,
    pub alert_proc_cpu: f64,
    pub alert_proc_secs: u64,
    pub alert_services: bool,
    /// Services page: "user" or "system".
    pub services_scope: String,
    /// Services page: list timers instead of services.
    pub services_timers: bool,
    /// Services page: all, running or failed.
    pub services_show: String,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            mode: ThemeMode::Omarchy,
            theme: "tokyo-night".into(),
            reduce_motion: false,
            sidebar_collapsed: false,
            glow: true,
            interval_ms: 1000,
            history_secs: 60,
            net_bits: false,
            fahrenheit: false,
            confirm_kill: true,
            kernel_threads: false,
            pause_hidden: true,
            hidden_columns: DEFAULT_HIDDEN.iter().map(|c| c.to_string()).collect(),
            process_view: "apps".into(),
            process_tree: false,
            processes_layout: PROCESSES_LAYOUT,
            column_widths: Default::default(),
            column_order: Vec::new(),
            sort_column: "cpu".into(),
            sort_desc: true,
            dashboard_order: Vec::new(),
            dashboard_hidden: vec!["battery".into(), "load".into()],
            dashboard_rail: true,
            cpu_grid: false,
            expanded_groups: Default::default(),
            consumer_views: Default::default(),
            warn_temp: 85.0,
            warn_mem: 90.0,
            warn_disk: 95.0,
            alert_temp: false,
            alert_mem: false,
            alert_disk: false,
            alert_proc: false,
            alert_proc_cpu: 50.0,
            alert_proc_secs: 30,
            alert_services: false,
            services_scope: "user".into(),
            services_timers: false,
            services_show: "all".into(),
        }
    }
}

/// Processes columns hidden by default: Name, CPU, Memory, Disk and Network show.
pub const DEFAULT_HIDDEN: &[&str] = &["pid", "user", "gpu", "threads", "state"];
pub const PROCESSES_LAYOUT: u32 = 2;

impl Prefs {
    /// Settings saved before Processes put apps first: its columns and view go to the new
    /// defaults once (the old Tree view is All indented by parent).
    pub fn migrate_processes(&mut self) -> bool {
        if self.processes_layout >= PROCESSES_LAYOUT {
            return false;
        }
        self.processes_layout = PROCESSES_LAYOUT;
        self.process_tree = self.process_view == "tree";
        self.process_view = if self.process_tree { "all".into() } else { "apps".into() };
        self.hidden_columns = DEFAULT_HIDDEN.iter().map(|c| c.to_string()).collect();
        self.column_order.clear();
        self.column_widths.clear();
        true
    }
}

nyx_core::prefs_store!(Prefs);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_processes_settings_move_once() {
        let mut p = Prefs { processes_layout: 0, process_view: "tree".into(), hidden_columns: vec![], ..Default::default() };
        assert!(p.migrate_processes());
        assert_eq!((p.process_view.as_str(), p.process_tree), ("all", true));
        assert_eq!(p.hidden_columns, DEFAULT_HIDDEN);
        p.process_view = "mine".into();
        assert!(!p.migrate_processes());
        assert_eq!(p.process_view, "mine");
    }
}
