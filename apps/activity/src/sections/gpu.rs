use super::top::{self, By};
use crate::charts::{self};
use crate::graph::{self, Scale, Series, Tone};
use crate::sampler::Snapshot;
use crate::sampler::gpu::{Gpu, Power, Vendor};
use crate::widgets::{self, Page};
use crate::{fmt, live, parts};
use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

pub fn build(page: &Page) {
    let holder = widgets::vbox(0);
    page.body.append(&holder);
    let page = Page { root: page.root.clone(), body: holder.clone() };
    live::ready(move |snap| {
        if snap.gpus.is_empty() {
            holder.append(&widgets::banner(
                "No GPU was found under <tt>/sys/class/drm</tt>, or sampling is paused while the window is hidden.",
                false,
            ));
            return;
        }
        let g = page.cards("");
        // The built-in GPU, which is always awake, gets the width; dedicated ones sit beside who's using it.
        let (wide, beside): (Vec<&Gpu>, Vec<&Gpu>) = snap.gpus.iter().partition(|g| g.vendor != Vendor::Nvidia);
        for gpu in &wide {
            g.add(&gpu_card(gpu));
        }
        let primary = wide.first().or(beside.first()).copied();
        let mut row: Vec<gtk::Widget> = beside.iter().map(|gpu| gpu_card(gpu).upcast()).collect();
        if let Some(p) = primary {
            row.push(using_card(p).upcast());
        }
        let refs: Vec<&gtk::Widget> = row.iter().collect();
        g.add(&parts::columns_at(&refs, 380, 2));
    });
}

/// "video-enhance" -> "Video enhance".
fn engine_name(e: &str) -> String {
    let e = e.replace(['-', '_'], " ");
    let mut c = e.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// "Intel Arc Pro 130T", "GeForce RTX 5080 Laptop GPU".
fn short_name(g: &Gpu) -> String {
    g.name.replace("NVIDIA ", "").replace("Intel ", "").replace("AMD ", "")
}

/// Whether Activity reads it: one that's asleep, or an NVIDIA GPU nothing uses, is left alone.
fn polled(g: &Gpu) -> bool {
    g.power_state != Power::Asleep && !(g.vendor == Vendor::Nvidia && g.power_state == Power::Idle)
}

/// A GPU: its readings and graph while it's in use, or that it's asleep and who keeps it awake.
fn gpu_card(g: &Gpu) -> gtk::Box {
    let c = parts::card("activity-gpu-symbolic", &g.name);
    let built_in = g.vendor == Vendor::Intel || (g.vendor == Vendor::Amd && g.vram_total.is_none());
    c.meta.set_text(&format!("{} · {}", if built_in { "built in" } else { "dedicated" }, g.driver));
    let state = widgets::label("", "gpu-state");
    c.head.append(&state);
    let pci = g.pci.clone();
    let vendor = g.vendor;

    // ----- In use -----
    let on = widgets::vbox(10);
    let busy = charts::tile("Busy");
    let clock = charts::tile("Clock");
    let memory = charts::tile("Memory");
    let power = charts::tile("Power");
    let temp = charts::tile("Temperature");
    on.append(&parts::readings(&[&busy, &clock, &memory, &power, &temp]));
    let gr =
        graph::graph(vec![Series::new(format!("gpu.{pci}.util"), "Busy", Tone::Swatch("violet"))], Scale::Percent, fmt::pct, 100);
    on.append(&gr.root);
    c.root.append(&on);

    // ----- Asleep -----
    let off = widgets::vbox(10);
    let note = parts::note();
    note.on_action("Read it now", || {
        live::control().read_gpu.store(true, std::sync::atomic::Ordering::Relaxed);
        live::poke();
    });
    note.action.set_tooltip_text(Some("Read it once. This wakes it for a moment."));
    off.append(&note.root);
    let awake_title = widgets::label("KEEPING IT AWAKE", "stat-title");
    awake_title.set_xalign(0.0);
    off.append(&awake_title);
    let awake = parts::app_list(5, "Nothing has it open, so it can sleep.", false);
    off.append(&awake.root);
    c.root.append(&off);

    // The last reading it gave, shown while it sleeps.
    let last: Rc<RefCell<Option<String>>> = Rc::default();
    live::on_tick(&c.root, move |s| {
        let Some(g) = s.gpus.iter().find(|x| x.pci == pci) else { return };
        let reading = polled(g);
        on.set_visible(reading);
        off.set_visible(!reading);
        state.set_text(match g.power_state {
            Power::Asleep => "Asleep",
            Power::Idle => "Idle",
            Power::Active if g.util.unwrap_or(0.0) >= 1.0 || g.clients > 0 => "In use",
            Power::Active => "Awake",
        });
        for (class, on) in [("on", reading), ("asleep", !reading)] {
            if on {
                state.add_css_class(class);
            } else {
                state.remove_css_class(class);
            }
        }
        if reading {
            let util = g.util.unwrap_or(0.0);
            busy.value.set_text(&fmt::pct(util));
            let mut es: Vec<&(String, f64)> = g.engines.iter().collect();
            es.sort_by(|a, b| b.1.total_cmp(&a.1));
            busy.sub.set_text(
                &es.iter()
                    .take(2)
                    .map(|(n, p)| format!("{} {}", engine_name(n).to_lowercase(), fmt::pct(*p)))
                    .collect::<Vec<_>>()
                    .join(" · "),
            );
            let now = g.act_clock.or(g.clock);
            clock.root.set_visible(now.is_some());
            if let Some(f) = now {
                clock.value.set_text(&fmt::mhz(f));
                clock.sub.set_text(&g.max_clock.map(|m| format!("of {}", fmt::mhz(m))).unwrap_or_default());
            }
            let mem = match (g.vram_used, g.vram_total, g.client_mem) {
                (Some(u), Some(t), _) => Some((fmt::bytes(u as f64), format!("of {}", fmt::bytes(t as f64)))),
                (_, _, Some(m)) => Some((fmt::bytes(m as f64), "shared with the system".into())),
                _ => None,
            };
            memory.root.set_visible(mem.is_some());
            if let Some((v, sub)) = mem {
                memory.value.set_text(&v);
                memory.sub.set_text(&sub);
            }
            power.root.set_visible(g.watts.is_some());
            if let Some(w) = g.watts {
                power.value.set_text(&fmt::watts(w));
                power.sub.set_text(&g.watts_limit.map(|l| format!("of {}", fmt::watts(l))).unwrap_or_default());
            }
            temp.root.set_visible(g.temp.is_some());
            if let Some(t) = g.temp {
                temp.value.set_text(&fmt::temp(t));
                temp.sub.set_text("");
            }
            if vendor == Vendor::Nvidia {
                let at = glib_time();
                let mut parts = vec![format!("{} busy", fmt::pct(util))];
                parts.extend(g.temp.map(fmt::temp));
                parts.extend(g.watts.map(fmt::watts));
                *last.borrow_mut() = Some(format!("Last read at {at}: {}.", parts.join(" · ")));
            }
            return;
        }
        let asleep = g.power_state == Power::Asleep;
        let why = if asleep {
            "Activity doesn't wake it to read it, so it can stay asleep and save power."
        } else {
            "Awake but unused. Activity doesn't read it now, so it can go back to sleep."
        };
        let detail = match last.borrow().as_ref() {
            Some(l) => format!("{why} {l}"),
            None => why.to_string(),
        };
        note.set("weather-clear-night-symbolic", parts::Tone::Plain, if asleep { "Asleep" } else { "Idle" }, &detail);
        note.action.set_visible(vendor == Vendor::Nvidia);
        awake.set(&holders(s));
    });
    c.root
}

fn glib_time() -> String {
    gtk::glib::DateTime::now_local().ok().and_then(|t| t.format("%H:%M").ok()).map(|s| s.to_string()).unwrap_or_default()
}

/// The apps holding the NVIDIA GPU open.
fn holders(s: &Snapshot) -> Vec<parts::App> {
    let Some(procs) = &s.procs else { return Vec::new() };
    let mut out: Vec<parts::App> = Vec::new();
    for p in procs.iter().filter(|p| p.nvidia) {
        let group = s.groups.iter().find(|g| g.pids.contains(&p.pid));
        let ((name, icon), pid) = match group {
            Some(g) => (top::group_identity(g, s), g.leader),
            None => (top::app_identity(&p.name, &[p.pid], s), p.pid),
        };
        if out.iter().any(|a| a.name == name) {
            continue;
        }
        out.push(parts::App { name, icon, value: "Has it open".into(), pid, ..Default::default() });
    }
    out
}

/// Who's using a GPU, by how busy they keep it or the memory they hold.
fn using_card(g: &Gpu) -> gtk::Box {
    let c = parts::card("", &format!("Using the {} most", short_name(g)));
    let busy = parts::app_list(5, "No app is using it.", true);
    let mem = parts::app_list(5, "No app holds GPU memory.", true);
    let stack = gtk::Stack::new();
    stack.set_vhomogeneous(false);
    stack.add_named(&busy.root, Some("busy"));
    stack.add_named(&mem.root, Some("memory"));
    let st = stack.clone();
    let seg = widgets::segmented(&widgets::opts(&[("busy", "Busy"), ("memory", "Memory")]), "busy", move |v| {
        st.set_visible_child_name(&v);
    });
    seg.add_css_class("small");
    c.head.append(&seg);
    c.root.append(&stack);
    live::on_tick(&c.root, move |s| {
        busy.set(&parts::busiest(s, By::Gpu, 5, |_| String::new()));
        mem.set(&parts::busiest(s, By::GpuMem, 5, |_| String::new()));
    });
    c.root
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_engines() {
        assert_eq!(engine_name("video-enhance"), "Video enhance");
        assert_eq!(engine_name("render"), "Render");
    }
}
