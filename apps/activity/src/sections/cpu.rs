use super::top::{self, By};
use crate::charts::{self};
use crate::graph::{self, Scale, Series, Tone};
use crate::sampler::Snapshot;
use crate::sampler::cpu::CoreKind;
use crate::widgets::{self, Page};
use crate::{fmt, live, parts};
use gtk::prelude::*;

/// The temperature coretemp reports for thread `i`'s core ("Core 8").
fn core_temp(s: &Snapshot, i: usize) -> Option<f64> {
    let id = s.cpu.layout.core_ids.get(i)?;
    let label = format!("Core {id}");
    s.sensors.temps.iter().find(|t| t.chip == "CPU" && t.label == label).map(|t| t.value)
}

/// "16 threads: 6 performance, 8 efficient, 2 low-power", or "8 cores · 16 threads".
fn threads_line(c: &crate::sampler::cpu::Cpu) -> String {
    let l = &c.layout;
    let n = c.cores.len();
    let mut kinds: Vec<(CoreKind, usize)> = Vec::new();
    for k in &l.kinds {
        match kinds.iter_mut().find(|(x, _)| x == k) {
            Some((_, n)) => *n += 1,
            None => kinds.push((*k, 1)),
        }
    }
    kinds.sort();
    if kinds.iter().all(|(k, _)| *k == CoreKind::Uniform) {
        return format!("{} cores · {n} threads", c.physical.max(1));
    }
    let parts: Vec<String> = kinds
        .iter()
        .map(|(k, n)| {
            let what = match k {
                CoreKind::Performance => "performance",
                CoreKind::Efficient => "efficient",
                CoreKind::LowPower => "low-power",
                CoreKind::Uniform => "other",
            };
            format!("{n} {what}")
        })
        .collect();
    format!("{n} threads: {}", parts.join(", "))
}

/// Who's using it, as apps or processes (the choice is kept).
pub fn using_it_most(key: &'static str, by: By, n: usize) -> gtk::Box {
    let card = parts::card("", "Using it most");
    let apps = parts::app_list(n, "Nothing busy right now.", true);
    let procs = parts::app_list(n, "Nothing busy right now.", true);
    let stack = gtk::Stack::new();
    stack.set_vhomogeneous(false);
    stack.add_named(&apps.root, Some("apps"));
    stack.add_named(&procs.root, Some("processes"));
    let chosen = crate::prefs::get().consumer_views.get(key).cloned().unwrap_or_else(|| "apps".into());
    stack.set_visible_child_name(if chosen == "processes" { "processes" } else { "apps" });
    let st = stack.clone();
    let seg = widgets::segmented(&widgets::opts(&[("apps", "Apps"), ("processes", "Processes")]), &chosen, move |v| {
        st.set_visible_child_name(&v);
        crate::prefs::update(|p| {
            p.consumer_views.insert(key.into(), v.clone());
        });
    });
    seg.add_css_class("small");
    card.head.append(&seg);
    card.root.append(&stack);
    live::on_tick(&card.root, move |s| {
        apps.set(&parts::busiest(s, by, n, |g| if g.procs > 1 { parts::processes(g.procs) } else { String::new() }));
        procs.set(&top::procs(s, by, n, None).into_iter().map(|b| parts::App::from_bar(b, String::new())).collect::<Vec<_>>());
    });
    card.root
}

/// Every thread as a bar, performance cores in blue and the others in teal.
fn thread_bars(snap: &Snapshot) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    row.set_homogeneous(true);
    row.add_css_class("thread-bars");
    row.set_vexpand(true);
    let layout = snap.cpu.layout.clone();
    let mut cells = Vec::new();
    for i in 0..snap.cpu.cores.len() {
        let kind = layout.kinds.get(i).copied().unwrap_or_default();
        let swatch: &'static str = if matches!(kind, CoreKind::Efficient | CoreKind::LowPower) { "teal" } else { "blue" };
        let col = widgets::vbox(4);
        let frac = std::rc::Rc::new(std::cell::Cell::new(0.0f64));
        let bar = gtk::DrawingArea::new();
        bar.set_vexpand(true);
        bar.set_content_height(60);
        {
            let frac = frac.clone();
            bar.set_draw_func(move |_, cr, w, h| {
                let (w, h) = (w as f64, h as f64);
                let t = crate::theme::Rgb::hex(&crate::theme::palette().text);
                cr.rectangle(0.0, 0.0, w, h);
                cr.set_source_rgba(t.0, t.1, t.2, 0.04);
                let _ = cr.fill();
                let bh = (frac.get() / 100.0).clamp(0.0, 1.0) * h;
                if bh > 0.0 {
                    cr.rectangle(0.0, h - bh.max(2.0), w, bh.max(2.0));
                    let c = charts::swatch(swatch);
                    cr.set_source_rgb(c.0, c.1, c.2);
                    let _ = cr.fill();
                }
            });
        }
        bar.set_overflow(gtk::Overflow::Hidden);
        bar.add_css_class("thread-bar");
        let v = widgets::label("", "thread-value");
        v.add_css_class("tnum");
        col.append(&bar);
        col.append(&v);
        row.append(&col);
        cells.push((i, col, bar, frac, v));
    }
    live::on_tick(&row, move |s| {
        for (i, col, bar, frac, v) in &cells {
            let busy = s.cpu.cores.get(*i).copied().unwrap_or(0.0);
            if (frac.get() - busy).abs() > 0.05 {
                frac.set(busy);
                bar.queue_draw();
            }
            v.set_text(&format!("{busy:.0}%"));
            let freq = s.cpu.freqs.get(*i).copied().unwrap_or(0.0);
            let split = s.cpu.core_split.get(*i).copied().unwrap_or_default();
            let kind = s.cpu.layout.kinds.get(*i).copied().unwrap_or_default();
            let temp = core_temp(s, *i).map(|t| format!(" · {}", fmt::temp(t))).unwrap_or_default();
            let what = if kind == CoreKind::Uniform {
                String::new()
            } else {
                format!(" ({})", kind.name().trim_end_matches('s').to_lowercase())
            };
            col.set_tooltip_text(Some(&format!(
                "CPU {i}{what}: {} busy at {}{temp}\nUser {} · kernel {} · I/O wait {}",
                fmt::pct(busy),
                fmt::mhz(freq),
                fmt::pct(split.user),
                fmt::pct(split.system + split.irq),
                fmt::pct(split.iowait),
            )));
        }
    });
    row
}

pub fn build(page: &Page) {
    let g = page.cards("");

    // ----- What it is, and how far back the graph reaches -----
    let head = parts::head("", "");
    head.root.append(&parts::range());
    g.add(&head.root);

    // ----- Readings -----
    let busy = charts::tile("Busy");
    let clock = charts::tile("Speed");
    let load = charts::tile("Load");
    let psi = charts::tile("Waiting for CPU");
    let temp = charts::tile("Temperature");
    g.add(&parts::readings(&[&busy, &clock, &load, &psi, &temp]));

    // ----- The graph -----
    let gr = graph::graph(
        vec![
            Series::new("cpu", "Busy", Tone::Swatch("blue")),
            Series { dashed: true, ..Series::new("cpu.system", "Kernel", Tone::Swatch("violet")) },
        ],
        Scale::Percent,
        fmt::pct,
        150,
    );
    let history = widgets::vbox(0);
    history.add_css_class("graph-card");
    history.append(&gr.root);
    g.add(&history);

    live::on_tick(&head.root, move |s| {
        let c = &s.cpu;
        head.title.set_text(&parts::model_name(&c.model));
        head.title.set_tooltip_text(Some(&c.model));
        head.line.set_text(&threads_line(c));
        busy.value.set_text(&fmt::pct(c.usage));
        busy.sub.set_text(&format!("of {} threads", c.cores.len()));

        let avg = if c.freqs.is_empty() { 0.0 } else { c.freqs.iter().sum::<f64>() / c.freqs.len() as f64 };
        let peak = c.freqs.iter().copied().fold(0.0, f64::max);
        clock.value.set_text(&fmt::mhz(avg));
        clock.sub.set_text(&format!("fastest {} · max {}", fmt::mhz(peak), fmt::mhz(c.max_freq)));
        load.value.set_text(&format!("{:.2}", c.load[0]));
        load.sub.set_text(&format!("5 min {:.2} · 15 min {:.2}", c.load[1], c.load[2]));
        widgets::set_warn(&load.value, c.load[0] > c.cores.len() as f64);
        match c.pressure {
            Some(p) => {
                psi.value.set_text(&fmt::pct(p.some10));
                psi.sub.set_text(&format!("1 min {} · 5 min {}", fmt::pct(p.some60), fmt::pct(p.some300)));
            }
            None => {
                psi.value.set_text("–");
                psi.sub.set_text("not reported by this kernel");
            }
        }
        temp.value.set_text(&s.sensors.cpu_temp.map(fmt::temp).unwrap_or_else(|| "–".into()));
        let hottest =
            (0..c.cores.len()).filter_map(|i| core_temp(s, i)).fold(None, |m: Option<f64>, t| Some(m.map_or(t, |m| m.max(t))));
        temp.sub.set_text(&hottest.map(|t| format!("hottest core {}", fmt::temp(t))).unwrap_or_else(|| "package".into()));
        widgets::set_warn(&temp.value, s.sensors.cpu_temp.is_some_and(|t| t >= crate::prefs::get().warn_temp));
    });

    // ----- Each thread, and who's using it -----
    let row_holder = widgets::vbox(0);
    g.add(&row_holder);
    live::ready(move |snap| {
        let each = parts::card("", "Each thread");
        let kinds = snap.cpu.layout.kinds.iter().any(|k| *k != CoreKind::Uniform);
        if kinds {
            each.meta.set_markup(&format!(
                "<span foreground=\"{}\">■</span> performance   <span foreground=\"{}\">■</span> efficient",
                charts::swatch("blue").to_hex(),
                charts::swatch("teal").to_hex()
            ));
        }
        each.root.append(&thread_bars(snap));
        let users = using_it_most("cpu", By::Cpu, 5);
        row_holder.append(&parts::columns(&[each.root.upcast_ref(), users.upcast_ref()], 2));
    });
}
