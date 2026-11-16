use crate::charts::{self};
use crate::sampler::Snapshot;
use crate::sampler::sensors::Reading;
use crate::widgets::{self, Page};
use crate::{fmt, live, parts, prefs};
use gtk::prelude::*;

/// What a temperature is in, which says how hot is too hot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Processor,
    Storage,
    Board,
}

impl Part {
    fn title(self) -> &'static str {
        match self {
            Part::Processor => "Processor",
            Part::Storage => "Storage",
            Part::Board => "Board and Wi-Fi",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Part::Processor => "activity-cpu-symbolic",
            Part::Storage => "drive-harddisk-symbolic",
            Part::Board => "network-wireless-symbolic",
        }
    }

    /// The limit when the chip doesn't give one.
    fn default_limit(self) -> f64 {
        match self {
            Part::Processor => 100.0,
            Part::Storage => 80.0,
            Part::Board => 95.0,
        }
    }
}

fn part_of(t: &Reading) -> Part {
    if t.chip == "CPU" {
        Part::Processor
    } else if t.chip.starts_with("NVMe") {
        Part::Storage
    } else {
        Part::Board
    }
}

fn limit(t: &Reading) -> f64 {
    t.high.unwrap_or_else(|| part_of(t).default_limit())
}

/// The temperatures worth a row, with a name for people: the processor's package and its
/// hottest core, each drive's own reading, and one per other chip.
fn rows(s: &Snapshot) -> Vec<(Part, String, f64, f64)> {
    let mut out = Vec::new();
    let cpu: Vec<&Reading> = s.sensors.temps.iter().filter(|t| t.chip == "CPU").collect();
    if let Some(p) = cpu.iter().find(|t| t.label.starts_with("Package") || t.label == "Tctl").or(cpu.first()) {
        out.push((Part::Processor, "Package".to_string(), p.value, limit(p)));
    }
    if let Some(c) = cpu.iter().filter(|t| t.label.starts_with("Core")).max_by(|a, b| a.value.total_cmp(&b.value)) {
        out.push((Part::Processor, "Hottest core".to_string(), c.value, limit(c)));
    }
    let mut chips: Vec<&str> = Vec::new();
    for t in s.sensors.temps.iter().filter(|t| t.chip != "CPU") {
        if !chips.contains(&t.chip.as_str()) {
            chips.push(&t.chip);
        }
    }
    for chip in chips {
        let ts: Vec<&Reading> = s.sensors.temps.iter().filter(|t| t.chip == chip).collect();
        // A drive's "Composite" is the one it reports as its own.
        let Some(t) = ts.iter().find(|t| t.label == "Composite").or(ts.iter().max_by(|a, b| a.value.total_cmp(&b.value))) else {
            continue;
        };
        let name = if let Some(n) = chip.strip_prefix("NVMe ") {
            let disk = s.disks.iter().position(|d| d.name.starts_with(&format!("nvme{n}")));
            disk.map_or_else(|| format!("Drive {n}"), |i| format!("Disk {i} (NVMe)"))
        } else {
            match chip {
                "Wi-Fi" => "Wi-Fi card".to_string(),
                c => c.to_string(),
            }
        };
        out.push((part_of(t), name, t.value, limit(t)));
    }
    out
}

/// "CPU fan", "GPU fan", "System fan".
fn fan_name(f: &Reading) -> String {
    match f.label.as_str() {
        "cpu_fan" => "CPU fan".into(),
        "gpu_fan" => "GPU fan".into(),
        "mid_fan" => "Middle fan".into(),
        l if l.starts_with("fan") && f.chip == "ACPI" => "System fan".into(),
        l if l.starts_with("fan") => format!("{} fan {}", f.chip, &l[3..]),
        l => l.replace('_', " "),
    }
}

pub fn build(page: &Page) {
    let holder = widgets::vbox(0);
    page.body.append(&holder);
    let page = Page { root: page.root.clone(), body: holder.clone() };
    live::ready(move |snap| {
        let g = page.cards("");
        let hot = parts::note();
        g.add(&hot.root);
        let root = hot.root.clone();
        live::on_tick(&root, move |s| hot_note(&hot, s));
        if snap.sensors.temps.is_empty() {
            g.add(&widgets::row("No temperature sensors", "The kernel exposes no hwmon temperature inputs here.", None));
        } else {
            g.add(&temperatures(snap));
        }
        let mut cards: Vec<gtk::Widget> = Vec::new();
        if !snap.sensors.fans.is_empty() {
            cards.push(fans().upcast());
        }
        cards.push(power(snap).upcast());
        let refs: Vec<&gtk::Widget> = cards.iter().collect();
        g.add(&parts::columns(&refs, 2));
    });
}

/// A plain note when something is close to its limit.
fn hot_note(note: &parts::Note, s: &Snapshot) {
    let warn = prefs::get().warn_temp;
    let worst = rows(s).into_iter().map(|(p, n, v, l)| (p, n, v, l, v / l)).max_by(|a, b| a.4.total_cmp(&b.4));
    let Some((part, name, v, lim, f)) = worst else { return note.hide() };
    let over = if part == Part::Processor { v >= warn || f >= 0.9 } else { f >= 0.9 };
    if !over {
        return note.hide();
    }
    let (title, mut detail) = match part {
        Part::Processor => {
            // How long it's been this hot, from the graph's history.
            let hist = live::history("temp.cpu");
            let n = hist.iter().rev().take_while(|t| **t >= v.min(warn) - 2.0).count();
            let secs = n as f64 * prefs::get().interval_ms as f64 / 1000.0;
            let mut d = format!("{} for {}", fmt::temp(v), fmt::duration(secs));
            if let Some(g) = s.groups.iter().filter(|g| g.cpu >= 25.0).max_by(|a, b| a.cpu.total_cmp(&b.cpu)) {
                d.push_str(&format!(" while {} is busy", super::top::group_identity(g, s).0));
            }
            ("The processor is running hot".to_string(), d)
        }
        _ => (format!("{name} is running hot"), fmt::temp(v)),
    };
    detail.push_str(&format!(". It slows itself down at {}.", fmt::temp(lim)));
    note.set("activity-thermometer-symbolic", if f >= 0.97 { parts::Tone::Bad } else { parts::Tone::Warn }, &title, &detail);
}

/// A temperature's row: its name, value and limit labels, and its bar with the fill and level it draws.
type Slot = (String, gtk::Label, gtk::Label, gtk::DrawingArea, std::rc::Rc<std::cell::Cell<(f64, u8)>>);

/// A card per part, each temperature a bar against its limit.
fn temperatures(snap: &Snapshot) -> gtk::Box {
    let mut cards: Vec<gtk::Widget> = Vec::new();
    let mut slots: Vec<Slot> = Vec::new();
    for part in [Part::Processor, Part::Storage, Part::Board] {
        let names: Vec<String> = rows(snap).into_iter().filter(|r| r.0 == part).map(|r| r.1).collect();
        if names.is_empty() {
            continue;
        }
        let c = parts::card(part.icon(), part.title());
        for name in names {
            let row = widgets::vbox(6);
            row.add_css_class("temp-row");
            let top = widgets::hbox(6);
            let n = widgets::label(&name, "temp-name");
            n.set_hexpand(true);
            n.set_xalign(0.0);
            n.set_ellipsize(gtk::pango::EllipsizeMode::End);
            let v = widgets::label("", "temp-value");
            v.add_css_class("tnum");
            let l = widgets::label("", "temp-limit");
            l.add_css_class("tnum");
            top.append(&n);
            top.append(&v);
            top.append(&l);
            row.append(&top);
            let state = std::rc::Rc::new(std::cell::Cell::new((0.0f64, 0u8)));
            let bar = gtk::DrawingArea::new();
            bar.set_content_height(6);
            bar.set_hexpand(true);
            {
                let state = state.clone();
                bar.set_draw_func(move |_, cr, w, h| {
                    let (w, h) = (w as f64, h as f64);
                    let p = crate::theme::palette();
                    let t = crate::theme::Rgb::hex(&p.text);
                    round(cr, w, h, w);
                    cr.set_source_rgba(t.0, t.1, t.2, 0.08);
                    let _ = cr.fill();
                    let (f, level) = state.get();
                    let fw = (f.clamp(0.0, 1.0) * w).max(h);
                    round(cr, w, h, fw);
                    let c = match level {
                        2 => crate::theme::Rgb::hex(&p.danger),
                        1 => charts::swatch("amber"),
                        _ => charts::swatch("blue"),
                    };
                    cr.set_source_rgb(c.0, c.1, c.2);
                    let _ = cr.fill();
                });
            }
            row.append(&bar);
            c.root.append(&row);
            slots.push((name, v, l, bar, state));
        }
        cards.push(c.root.upcast());
    }
    let refs: Vec<&gtk::Widget> = cards.iter().collect();
    let row = charts::flow_row(&refs, 230, 3, true);
    live::on_tick(&row, move |s| {
        let now = rows(s);
        for (name, v, l, bar, state) in &slots {
            let Some((_, _, t, lim)) = now.iter().find(|r| &r.1 == name) else { continue };
            let f = t / lim;
            let level = if f >= 0.97 {
                2
            } else if f >= 0.85 {
                1
            } else {
                0
            };
            v.set_text(&fmt::temp(*t));
            l.set_text(&format!("of {}", fmt::temp(*lim).trim_end_matches(" °C").trim_end_matches(" °F")));
            for (class, on) in [("warn", level == 1), ("bad", level == 2)] {
                if on {
                    v.add_css_class(class);
                } else {
                    v.remove_css_class(class);
                }
            }
            if state.get() != (f, level) {
                state.set((f, level));
                bar.queue_draw();
            }
        }
    });
    row
}

fn round(cr: &gtk::cairo::Context, _w: f64, h: f64, upto: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    let r = h / 2.0;
    cr.new_sub_path();
    cr.arc(upto - r, r, r, -FRAC_PI_2, FRAC_PI_2);
    cr.arc(r, r, r, FRAC_PI_2, PI + FRAC_PI_2);
    cr.close_path();
}

fn fans() -> gtk::Box {
    let c = parts::card("activity-fan-symbolic", "Fans");
    let list = widgets::vbox(0);
    list.add_css_class("kv-list");
    c.root.append(&list);
    let shown = std::cell::RefCell::new(Vec::<String>::new());
    let labels = std::cell::RefCell::new(Vec::<gtk::Label>::new());
    live::on_tick(&c.root, move |s| {
        let names: Vec<String> = s.sensors.fans.iter().map(fan_name).collect();
        if *shown.borrow() != names {
            while let Some(ch) = list.first_child() {
                list.remove(&ch);
            }
            let mut out = Vec::new();
            for n in &names {
                let row = widgets::hbox(12);
                row.add_css_class("kv-row");
                let k = widgets::label(n, "kv-key");
                k.set_hexpand(true);
                k.set_xalign(0.0);
                let v = widgets::label("", "kv-value");
                v.add_css_class("tnum");
                row.append(&k);
                row.append(&v);
                list.append(&row);
                out.push(v);
            }
            *labels.borrow_mut() = out;
            *shown.borrow_mut() = names;
        }
        for (l, f) in labels.borrow().iter().zip(&s.sensors.fans) {
            l.set_text(&if f.value > 0.0 {
                format!("{} rpm", super::dashboard::thousands(f.value.round() as u64))
            } else {
                "Stopped".into()
            });
        }
        let spinning = s.sensors.fans.iter().filter(|f| f.value > 0.0).count();
        c.meta.set_text(&format!("{spinning} of {} spinning", s.sensors.fans.len()));
    });
    c.root
}

/// The battery, what the machine draws, and how worn the battery is.
fn power(snap: &Snapshot) -> gtk::Box {
    let c = parts::card("battery-symbolic", if snap.sensors.batteries.is_empty() { "Power" } else { "Battery and power" });
    let bat = charts::tile("Battery");
    let draw = charts::tile("Drawing");
    let health = charts::tile("Health");
    let profile = charts::tile("Power profile");
    c.root.append(&parts::readings(&[&bat, &draw, &health, &profile]));
    live::on_tick(&c.root, move |s| {
        let b = s.sensors.batteries.first();
        bat.root.set_visible(b.is_some());
        health.root.set_visible(b.is_some_and(|b| b.health().is_some()));
        if let Some(b) = b {
            bat.value.set_text(&fmt::pct(b.percent));
            let plug = match s.sensors.ac {
                Some(true) => " · plugged in",
                Some(false) => "",
                None => "",
            };
            let state = match b.status.as_str() {
                "Full" => "full".to_string(),
                "Charging" => b.hours_left.map_or("charging".into(), |h| format!("full in {}", fmt::hours(h))),
                "Discharging" => b.hours_left.map_or("on battery".into(), |h| format!("{} left", fmt::hours(h))),
                other => other.to_lowercase(),
            };
            bat.sub.set_text(&format!("{state}{plug}"));
            widgets::set_warn(&bat.value, b.percent <= 15.0 && b.status == "Discharging");
            if let Some(h) = b.health() {
                health.value.set_text(&fmt::pct(h));
                health.sub.set_text("of its original capacity");
                widgets::set_warn(&health.value, h < 70.0);
            }
        }
        let watts = s.sensors.batteries.iter().map(|b| b.watts).sum::<f64>();
        let on_battery = s.sensors.ac == Some(false);
        draw.value.set_text(&if watts > 0.1 { fmt::watts(watts) } else { "–".into() });
        let charger = s.sensors.sources.iter().find(|p| p.online).map(|p| {
            let w = if p.watts > 0.1 { format!(" at {}", fmt::watts(p.watts)) } else { String::new() };
            format!("{}{w}{}", p.name, if p.mode.is_empty() { String::new() } else { format!(" · {}", p.mode) })
        });
        draw.sub.set_text(&match (on_battery, b.map(|b| b.status.as_str())) {
            (true, _) => "from the battery".to_string(),
            (false, Some("Charging")) => "into the battery".to_string(),
            (false, _) if watts > 0.1 => "from the charger".to_string(),
            _ => String::new(),
        });
        draw.root.set_tooltip_text(charger.map(|c| format!("Charging from {c}")).as_deref());
        let cpu = &s.cpu;
        profile.root.set_visible(!cpu.platform_profile.is_empty());
        profile.value.set_text(&capitalise(&cpu.platform_profile));
        profile.sub.set_text(&if cpu.epp.is_empty() {
            String::new()
        } else {
            format!("CPU prefers {}", cpu.epp.replace('_', " "))
        });
    });
    c.root
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}
