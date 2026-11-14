use crate::widgets::{self, Page};
use crate::{live, prefs, window};

pub fn build(page: &Page) {
    let p = prefs::get();
    // ----- Monitoring -----
    let g = page.group("Monitoring");
    let (r, _) = widgets::choice_row(
        "Update every",
        "How often numbers and graphs refresh. Faster costs a little more CPU.",
        widgets::opts(&[("500", "0.5 seconds"), ("1000", "1 second"), ("2000", "2 seconds"), ("5000", "5 seconds")]),
        &p.interval_ms.to_string(),
        |v| {
            let ms: u64 = v.parse().unwrap_or(1000);
            prefs::update(|p| p.interval_ms = ms);
            live::set_interval(ms);
            window::refresh_pause();
        },
    );
    g.add(&r);
    let (r, _) = widgets::choice_row(
        "Graph history",
        "How far back the graphs reach.",
        widgets::opts(&[("60", "1 minute"), ("300", "5 minutes"), ("600", "10 minutes")]),
        &p.history_secs.to_string(),
        |v| {
            prefs::update(|p| p.history_secs = v.parse().unwrap_or(60));
            live::trim();
        },
    );
    g.add(&r);
    let (r, _) =
        widgets::switch_row("Network speeds in bits", "Show Mb/s like internet plans do, instead of MiB/s.", p.net_bits, |on| {
            prefs::update(|p| p.net_bits = on)
        });
    g.add(&r);
    let (r, _) = widgets::choice_row(
        "Temperature unit",
        "How temperatures are shown everywhere, graphs included.",
        widgets::opts(&[("c", "Celsius (°C)"), ("f", "Fahrenheit (°F)")]),
        if p.fahrenheit { "f" } else { "c" },
        |v| {
            prefs::update(|p| p.fahrenheit = v == "f");
            // Refresh the readouts now rather than at the next sample.
            live::poke();
        },
    );
    g.add(&r);
    let (r, _) = widgets::switch_row(
        "Rest while hidden",
        "Stop reading every process and the GPU while this window is out of sight. The graphs keep going.",
        p.pause_hidden,
        |on| {
            prefs::update(|p| p.pause_hidden = on);
            if !on {
                live::set_detail(true);
            }
        },
    );
    g.add(&r);

    let (r, _) = widgets::button_row(
        "Export graph history",
        "Save every graph's history as a CSV file. Right-click a graph to save just that one.",
        "Export…",
        |_| crate::actions::export_csv("activity-history", None),
    );
    g.add(&r);

    // ----- Warnings -----
    let g = page.group("Warnings");
    g.note("Past these, the Dashboard says so first thing, values turn red, and alerts (below) can notify you.");
    // Stored in °C; shown in the chosen unit.
    let f = p.fahrenheit;
    let to_unit = move |c: f64| if f { c * 9.0 / 5.0 + 32.0 } else { c };
    let (r, _) = widgets::spin_row(
        &format!("CPU temperature ({})", if f { "°F" } else { "°C" }),
        "How hot the CPU may get. The unit follows the setting above, from the next time this page opens.",
        if f { (120.0, 230.0, 1.0) } else { (50.0, 110.0, 1.0) },
        to_unit(p.warn_temp).round(),
        move |v| prefs::update(|p| p.warn_temp = if f { (v - 32.0) * 5.0 / 9.0 } else { v }),
    );
    g.add(&r);
    let (r, _) = widgets::spin_row("Memory in use (%)", "How full RAM may get.", (50.0, 100.0, 1.0), p.warn_mem, |v| {
        prefs::update(|p| p.warn_mem = v)
    });
    g.add(&r);
    let (r, _) = widgets::spin_row("Disk full (%)", "How full a mounted drive may get.", (50.0, 100.0, 1.0), p.warn_disk, |v| {
        prefs::update(|p| p.warn_disk = v)
    });
    g.add(&r);

    // ----- Alerts -----
    let g = page.group("Alerts");
    g.note(
        "Desktop notifications, at most one of each kind every 5 minutes. They work while Activity is open, \
         on screen or not. The process alert pauses while \"Rest while hidden\" has stopped reading processes.",
    );
    let (r, _) = widgets::switch_row("CPU too hot", "Past the temperature warning above.", p.alert_temp, |on| {
        prefs::update(|p| p.alert_temp = on)
    });
    g.add(&r);
    let (r, _) = widgets::switch_row("Memory running out", "Past the memory warning above.", p.alert_mem, |on| {
        prefs::update(|p| p.alert_mem = on)
    });
    g.add(&r);
    let (r, _) = widgets::switch_row("A disk filling up", "Past the disk warning above.", p.alert_disk, |on| {
        prefs::update(|p| p.alert_disk = on)
    });
    g.add(&r);
    let (r, _) =
        widgets::switch_row("A service failed", "A user or system service stops with an error.", p.alert_services, |on| {
            prefs::update(|p| p.alert_services = on)
        });
    g.add(&r);
    let (r, _) = widgets::switch_row(
        "A process keeps the CPU busy",
        "One process stays over the limit below for the time below.",
        p.alert_proc,
        |on| prefs::update(|p| p.alert_proc = on),
    );
    g.add(&r);
    let (r, _) =
        widgets::spin_row("Process CPU limit (%)", "Share of the whole processor.", (10.0, 100.0, 5.0), p.alert_proc_cpu, |v| {
            prefs::update(|p| p.alert_proc_cpu = v)
        });
    g.add(&r);
    let (r, _) = widgets::spin_row("For at least (seconds)", "", (5.0, 600.0, 5.0), p.alert_proc_secs as f64, |v| {
        prefs::update(|p| p.alert_proc_secs = v as u64)
    });
    g.add(&r);

    // ----- Processes -----
    let g = page.group("Processes");
    let (r, _) = widgets::switch_row(
        "Ask before ending tasks",
        "End task and Kill need a second click to confirm.",
        p.confirm_kill,
        |on| prefs::update(|p| p.confirm_kill = on),
    );
    g.add(&r);
    let (r, _) = widgets::switch_row(
        "Show kernel threads",
        "List the kernel's own workers (kworker, ksoftirqd…) in All, and under System in Apps.",
        p.kernel_threads,
        |on| {
            prefs::update(|p| p.kernel_threads = on);
            super::processes::refilter();
        },
    );
    g.add(&r);
    g.note("To open Activity with a key, bind <tt>nyx-activity --toggle</tt> in <tt>~/.config/hypr/bindings.lua</tt>, for example Ctrl+Shift+Esc.");
}
