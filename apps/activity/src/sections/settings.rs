use crate::widgets::{self, Page};
use crate::{fmt, live, parts, prefs, window};
use gtk::prelude::*;

/// A dropdown row of `(value, label)` choices, with the current value added when it isn't one.
fn choice_row(title: &str, desc: &str, mut options: Vec<(f64, String)>, current: f64, set: impl Fn(f64) + 'static) -> gtk::Box {
    if !options.iter().any(|(v, _)| (v - current).abs() < 0.5) {
        let label = options.first().map_or_else(|| current.to_string(), |_| format!("{current}"));
        options.push((current, label));
        options.sort_by(|a, b| a.0.total_cmp(&b.0));
    }
    let opts: Vec<(String, String)> = options.iter().map(|(v, l)| (v.to_string(), l.clone())).collect();
    let now = options.iter().find(|(v, _)| (v - current).abs() < 0.5).map_or(String::new(), |(v, _)| v.to_string());
    let (row, _) = widgets::choice_row(title, desc, opts, &now, move |v| set(v.parse().unwrap_or(0.0)));
    row
}

pub fn build(page: &Page) {
    let p = prefs::get();
    let head = parts::head("Settings", "How often Activity reads the machine, and when it warns you");
    head.root.add_css_class("settings-head");
    page.body.append(&head.root);

    // ----- Monitoring -----
    let g = page.group("Monitoring");
    let (r, _) = widgets::choice_row(
        "Update every",
        "Faster costs a little more CPU",
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
        "Graphs show",
        "",
        widgets::opts(&[("60", "The last minute"), ("300", "The last 5 minutes"), ("600", "The last 10 minutes")]),
        &p.history_secs.to_string(),
        |v| {
            prefs::update(|p| p.history_secs = v.parse().unwrap_or(60));
            live::poke();
        },
    );
    g.add(&r);
    let (r, _) = widgets::switch_row(
        "Rest while hidden",
        "Stops reading the GPU and every process while the window is closed",
        p.pause_hidden,
        |on| {
            prefs::update(|p| p.pause_hidden = on);
            if !on {
                live::set_detail(true);
            }
        },
    );
    g.add(&r);
    g.add(&widgets::segmented_row(
        "Temperatures in",
        "",
        widgets::opts(&[("c", "°C"), ("f", "°F")]),
        if p.fahrenheit { "f" } else { "c" },
        |v| {
            prefs::update(|p| p.fahrenheit = v == "f");
            live::poke();
        },
    ));
    g.add(&widgets::segmented_row(
        "Network speeds in",
        "",
        widgets::opts(&[("bytes", "Bytes"), ("bits", "Bits")]),
        if p.net_bits { "bits" } else { "bytes" },
        |v| {
            prefs::update(|p| p.net_bits = v == "bits");
            live::poke();
        },
    ));

    // ----- Warn me when -----
    let g = page.group("Warn me when");
    // Stored in °C; shown in the chosen unit.
    let temps: Vec<(f64, String)> = [70.0, 75.0, 80.0, 85.0, 90.0, 95.0, 100.0].iter().map(|c| (*c, fmt::temp(*c))).collect();
    g.add(&choice_row("The processor is hotter than", "", temps, p.warn_temp.round(), |v| prefs::update(|p| p.warn_temp = v)));
    let pcts = |vals: &[f64]| vals.iter().map(|v| (*v, fmt::pct(*v))).collect::<Vec<_>>();
    g.add(&choice_row("Memory is fuller than", "", pcts(&[70.0, 75.0, 80.0, 85.0, 90.0, 95.0]), p.warn_mem.round(), |v| {
        prefs::update(|p| p.warn_mem = v)
    }));
    g.add(&choice_row("A disk is fuller than", "", pcts(&[80.0, 85.0, 90.0, 95.0, 98.0]), p.warn_disk.round(), |v| {
        prefs::update(|p| p.warn_disk = v)
    }));
    let mins = |s: f64| {
        if s < 60.0 {
            format!("{s:.0} seconds")
        } else if s < 120.0 {
            "1 minute".into()
        } else {
            format!("{:.0} minutes", s / 60.0)
        }
    };
    let mut busy: Vec<(f64, String)> = vec![(0.0, "Never".into())];
    busy.extend([30.0, 60.0, 120.0, 300.0, 600.0].iter().map(|s| (*s, mins(*s))));
    g.add(&choice_row(
        "One process keeps the processor busy for",
        &format!("Busy means over {} of the whole processor", fmt::pct(p.alert_proc_cpu)),
        busy,
        if p.alert_proc { p.alert_proc_secs as f64 } else { 0.0 },
        |v| {
            prefs::update(|p| {
                p.alert_proc = v > 0.0;
                if v > 0.0 {
                    p.alert_proc_secs = v as u64;
                }
            })
        },
    ));
    let (r, _) = widgets::switch_row(
        "A service fails",
        "A notification when one of yours or the system's stops with an error",
        p.alert_services,
        |on| prefs::update(|p| p.alert_services = on),
    );
    g.add(&r);
    let (r, _) = widgets::switch_row(
        "Also as a notification",
        "Past these, the Dashboard says so first; this tells you wherever you are, at most every 5 minutes",
        p.alert_temp || p.alert_mem || p.alert_disk,
        |on| {
            prefs::update(|p| {
                p.alert_temp = on;
                p.alert_mem = on;
                p.alert_disk = on;
            })
        },
    );
    g.add(&r);

    // ----- Processes -----
    let g = page.group("Processes");
    let (r, _) = widgets::switch_row("Ending an app", "Ask first: End task and Kill need a second click", p.confirm_kill, |on| {
        prefs::update(|p| p.confirm_kill = on)
    });
    g.add(&r);
    let (r, _) = widgets::switch_row(
        "Show kernel threads",
        "The kernel's own workers (kworker, ksoftirqd…), in All and under System",
        p.kernel_threads,
        |on| {
            prefs::update(|p| p.kernel_threads = on);
            super::processes::refilter();
        },
    );
    g.add(&r);

    // ----- Export, shortcuts, about -----
    let g = page.group("");
    let (r, _) = widgets::button_row("Export graph history", "Every graph's history as a CSV file", "Export…", |_| {
        crate::actions::export_csv("activity-history", None)
    });
    g.add(&r);
    let (r, _) = widgets::button_row("Keyboard shortcuts", "Ctrl+K goes to any page, process or service", "Show…", |_| {
        window::show_shortcuts()
    });
    g.add(&r);
    let (r, _) = widgets::switch_row("Reduce motion", "No sliding or fading between pages", p.reduce_motion, |on| {
        prefs::update(|p| p.reduce_motion = on)
    });
    g.add(&r);
    let (r, _) = widgets::button_row(
        &format!("Activity {}", env!("CARGO_PKG_VERSION")),
        "Theme and accent follow Nyx Settings",
        "Website",
        |_| crate::cmd::spawn(&["xdg-open", "https://github.com/design-nexus/nyx-activity"]),
    );
    g.add(&r);
    g.note("To open Activity with a key, bind <tt>nyx-activity --toggle</tt> in <tt>~/.config/hypr/bindings.lua</tt>, for example Ctrl+Shift+Esc.");
}
