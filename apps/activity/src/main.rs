//! Activity — a task manager for Omarchy.

mod actions;
mod alerts;
mod apps;
mod charts;
mod cmd;
mod fmt;
mod graph;
mod live;
mod palette;
mod paths;
mod prefs;
mod sampler;
mod sections;
mod settings_dialog;
mod startup;
mod theme;
mod widgets;
mod window;

use gtk::prelude::*;
use gtk::{gio, glib};

const APP_ID: &str = "io.github.design_nexus.nyx.Activity";

const USAGE: &str = "Usage: nyx-activity [--section ID] [--toggle]\n\
\n\
  --section ID   open (or switch the open window) to a page: dashboard, processes, cpu,\n\
                 memory, storage, network, gpu, sensors, startup, services, settings\n\
  --toggle       close the window if it's open, otherwise open it (for a keybinding)\n\
  --top cpu|memory  print the busiest processes and the totals as JSON, then exit\n";

fn main() -> glib::ExitCode {
    nyx_core::app::init(nyx_core::app::App { slug: "nyx-activity", name: "Activity" });
    nyx_core::migrate::from(&["nexus-tasks", "nixe-tasks", "nyx-tasks"]);
    startup::move_old_include();
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return glib::ExitCode::SUCCESS;
    }

    // Nyx Assistant's look at the machine: the busiest processes and the totals as JSON,
    // from two samples half a second apart (no window; nothing is changed).
    if let Some(i) = args.iter().position(|a| a == "--top") {
        let by = args.get(i + 1).map(String::as_str).unwrap_or("cpu");
        let rx = sampler::start(sampler::Control::new(500));
        let snap = rx.recv_blocking().and_then(|_| rx.recv_blocking());
        let Ok(snap) = snap else {
            eprintln!("couldn't sample the system");
            return glib::ExitCode::FAILURE;
        };
        let mut procs = snap.procs.clone().unwrap_or_default();
        procs.retain(|p| !p.kernel);
        if by == "memory" {
            procs.sort_by_key(|p| std::cmp::Reverse(p.rss));
        } else {
            procs.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
        }
        let mib = |b: u64| b / (1024 * 1024);
        let top: Vec<serde_json::Value> = procs
            .iter()
            .take(10)
            .map(|p| serde_json::json!({"pid": p.pid, "name": p.name, "user": p.user, "cpu_percent": (p.cpu * 10.0).round() / 10.0, "memory_mib": mib(p.rss)}))
            .collect();
        let gpus: Vec<serde_json::Value> = snap
            .gpus
            .iter()
            .map(|g| serde_json::json!({"name": g.name, "busy_percent": g.util, "memory_mib": g.vram_used.map(mib), "memory_total_mib": g.vram_total.map(mib), "temperature": g.temp}))
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "cpu_percent": (snap.cpu.usage * 10.0).round() / 10.0,
                "load": snap.cpu.load,
                "memory_used_mib": mib(snap.mem.used),
                "memory_total_mib": mib(snap.mem.total),
                "swap_used_mib": mib(snap.mem.swap_used),
                "processes": snap.process_count,
                "gpus": gpus,
                "top": top,
                "by": if by == "memory" { "memory" } else { "cpu" },
            })
        );
        return glib::ExitCode::SUCCESS;
    }

    // GTK's Vulkan renderer enumerates every GPU at startup, which wakes a
    // sleeping discrete GPU on hybrid laptops. GL renders only on the one in use.
    if std::env::var_os("GSK_RENDERER").is_none() {
        // SAFETY: still single-threaded; nothing else reads the environment yet.
        unsafe { std::env::set_var("GSK_RENDERER", "ngl") };
    }

    // Developer aid: a different id gives a separate instance (a test copy on a virtual
    // monitor), so it doesn't hand its arguments to the copy that's open.
    let id = std::env::var("NYX_ACTIVITY_APP_ID").unwrap_or_else(|_| APP_ID.to_string());
    let app = gtk::Application::builder().application_id(id).flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE).build();
    // Notifications open the page they're about.
    let show = gio::SimpleAction::new("show", Some(glib::VariantTy::STRING));
    let a = app.clone();
    show.connect_activate(move |_, v| {
        let section = v.and_then(|v| v.get::<String>());
        window::present(&a, section.as_deref());
    });
    app.add_action(&show);
    app.connect_command_line(|app, cl| {
        let argv: Vec<String> = cl.arguments().iter().map(|a| a.to_string_lossy().to_string()).collect();
        let section = argv.iter().position(|a| a == "--section").and_then(|i| argv.get(i + 1)).cloned();
        if argv.iter().any(|a| a == "--toggle")
            && let Some(w) = window::window()
        {
            w.close();
            return glib::ExitCode::SUCCESS;
        }
        window::present(app, section.as_deref());
        glib::ExitCode::SUCCESS
    });
    app.connect_shutdown(|_| prefs::flush());
    app.run()
}
