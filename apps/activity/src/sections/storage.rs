use crate::charts::{self};
use crate::graph::{self, Scale, Series, Tone};
use crate::sampler::disk::{Disk, Mount};
use crate::widgets::{self, Page};
use crate::{cmd, fmt, live, parts, prefs};
use gtk::pango;
use gtk::prelude::*;
use std::rc::Rc;

fn kind(d: &Disk) -> &'static str {
    if d.name.starts_with("nvme") {
        "NVMe"
    } else if d.removable {
        "Removable"
    } else if d.rotational {
        "Hard disk"
    } else {
        "SSD"
    }
}

/// A disk's places, once each: btrfs subvolumes and bind mounts of one filesystem
/// share its space, so it's shown under its shortest path ("/", not "/var/log").
pub fn places(d: &Disk) -> Vec<&Mount> {
    let mut out: Vec<&Mount> = Vec::new();
    for m in d.mounts.iter().filter(|m| m.total > 0) {
        match out.iter_mut().find(|o| o.device == m.device) {
            Some(o) if m.path.len() < o.path.len() => *o = m,
            Some(_) => {}
            None => out.push(m),
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

pub fn build(page: &Page) {
    let holder = widgets::vbox(0);
    page.body.append(&holder);
    let page = Page { root: page.root.clone(), body: holder.clone() };
    live::ready(move |snap| {
        if snap.disks.is_empty() {
            holder.append(&widgets::banner("No disks were found in <tt>/sys/block</tt>.", true));
            return;
        }
        let g = page.cards("");
        overall(&g);
        let full = Rc::new(parts::note());
        for (i, d) in snap.disks.iter().enumerate() {
            g.add(&disk_card(i, d));
        }
        g.add(&full.root);
        let f = full.clone();
        live::on_tick(&full.root, move |s| {
            let full = &f;
            let warn = prefs::get().warn_disk;
            let fullest = s
                .disks
                .iter()
                .flat_map(places)
                .map(|m| (m, m.used() as f64 / m.total as f64 * 100.0))
                .filter(|(_, p)| *p >= warn)
                .max_by(|a, b| a.1.total_cmp(&b.1));
            match fullest {
                Some((m, p)) => full.set(
                    "drive-harddisk-symbolic",
                    if p >= 98.0 { parts::Tone::Bad } else { parts::Tone::Warn },
                    &format!("{} is {} full", m.path, fmt::pct(p)),
                    &format!("{} left of {}.", fmt::bytes(m.free as f64), fmt::bytes(m.total as f64)),
                ),
                None => full.hide(),
            }
        });
        let target: Rc<std::cell::RefCell<String>> = Rc::default();
        {
            let target = target.clone();
            full.on_action("Open", move || {
                let t = target.borrow().clone();
                if !t.is_empty() {
                    cmd::spawn(&["xdg-open", &t]);
                }
            });
        }
        live::on_tick(&full.action, move |s| {
            let warn = prefs::get().warn_disk;
            if let Some(m) = s.disks.iter().flat_map(places).find(|m| m.used() as f64 / m.total as f64 * 100.0 >= warn) {
                *target.borrow_mut() = m.path.clone();
            }
        });
    });
}

/// Every disk together.
fn overall(g: &widgets::Group) {
    let read = charts::tile("Reading");
    let write = charts::tile("Writing");
    let busy = charts::tile("Busiest disk");
    let psi = charts::tile("Waiting on disk");
    psi.root.set_tooltip_text(Some("How much of the time programs were held up waiting for the disks"));
    let strip = parts::readings(&[&read, &write, &busy, &psi]);
    g.add(&strip);
    live::on_tick(&strip, move |s| {
        read.value.set_text(&fmt::rate(s.disk_read()));
        read.sub.set_text(&format!("{} since boot", fmt::bytes(s.disks.iter().map(|d| d.read_total as f64).sum())));
        write.value.set_text(&fmt::rate(s.disk_write()));
        write.sub.set_text(&format!("{} since boot", fmt::bytes(s.disks.iter().map(|d| d.written_total as f64).sum())));
        busy.value.set_text(&fmt::pct(s.disk_busy()));
        let top = s.disks.iter().enumerate().max_by(|a, b| a.1.busy.total_cmp(&b.1.busy));
        busy.sub.set_text(&top.map(|(i, _)| format!("Disk {i}")).unwrap_or_default());
        match s.io_pressure {
            Some(p) => {
                psi.value.set_text(&fmt::pct(p.some10));
                psi.sub.set_text("held up, last 10 s");
                widgets::set_warn(&psi.value, p.some10 >= 10.0);
            }
            None => {
                psi.value.set_text("–");
                psi.sub.set_text("not reported by this kernel");
            }
        }
    });
}

/// A disk: how full each of its places is, then a short graph of its reading and
/// writing beside how busy it is.
fn disk_card(i: usize, d: &Disk) -> gtk::Box {
    let c = parts::card("drive-harddisk-symbolic", &format!("Disk {i}"));
    c.meta.set_text(&format!("{} · {} · {}", d.model, fmt::bytes(d.size as f64), kind(d)));
    c.meta.set_tooltip_text(Some(&format!("{} ({})", d.model, d.name)));
    let temp = widgets::label("", "page-card-meta");
    temp.add_css_class("tnum");
    c.head.append(&temp);
    let name = d.name.clone();

    // ----- Its places -----
    let rows = gtk::Grid::new();
    rows.set_column_spacing(14);
    rows.set_row_spacing(8);
    rows.add_css_class("disk-places");
    let mut cells = Vec::new();
    for (r, m) in places(d).iter().enumerate() {
        let path = widgets::label(&m.path, "disk-place");
        path.set_xalign(0.0);
        path.set_ellipsize(pango::EllipsizeMode::Middle);
        path.set_width_chars(14);
        path.set_max_width_chars(24);
        path.set_tooltip_text(Some(&format!("{} · {} · {}", m.path, m.fs, m.device)));
        let state = Rc::new(std::cell::Cell::new((0.0f64, 0u8)));
        let bar = gtk::DrawingArea::new();
        bar.set_content_height(6);
        bar.set_hexpand(true);
        bar.set_valign(gtk::Align::Center);
        {
            let state = state.clone();
            bar.set_draw_func(move |_, cr, w, h| {
                let (w, h) = (w as f64, h as f64);
                let p = crate::theme::palette();
                let t = crate::theme::Rgb::hex(&p.text);
                rounded(cr, 0.0, 0.0, w, h, h / 2.0);
                cr.set_source_rgba(t.0, t.1, t.2, 0.08);
                let _ = cr.fill();
                let (f, level) = state.get();
                let fw = f.clamp(0.0, 1.0) * w;
                if fw > 0.0 {
                    rounded(cr, 0.0, 0.0, fw.max(h), h, h / 2.0);
                    let c = match level {
                        2 => crate::theme::Rgb::hex(&p.danger),
                        1 => charts::swatch("amber"),
                        _ => charts::swatch("blue"),
                    };
                    cr.set_source_rgb(c.0, c.1, c.2);
                    let _ = cr.fill();
                }
            });
        }
        let value = widgets::label("", "disk-place-value");
        value.add_css_class("tnum");
        value.set_xalign(1.0);
        let open = gtk::Button::from_icon_name("folder-open-symbolic");
        open.add_css_class("flat");
        open.add_css_class("disk-open");
        open.set_tooltip_text(Some("Open in the file manager"));
        let target = m.path.clone();
        open.connect_clicked(move |_| cmd::spawn(&["xdg-open", &target]));
        rows.attach(&path, 0, r as i32, 1, 1);
        rows.attach(&bar, 1, r as i32, 1, 1);
        rows.attach(&value, 2, r as i32, 1, 1);
        rows.attach(&open, 3, r as i32, 1, 1);
        cells.push((m.device.clone(), state, bar, value));
    }
    if cells.is_empty() {
        let none = widgets::label("Nothing on it is mounted.", "dim");
        none.set_xalign(0.0);
        c.root.append(&none);
    } else {
        c.root.append(&rows);
    }

    // ----- Reading and writing -----
    let gr = graph::graph(
        vec![
            Series::new(format!("disk.{name}.write"), "Write", Tone::Swatch("teal")),
            Series { dashed: true, ..Series::new(format!("disk.{name}.read"), "Read", Tone::Swatch("blue")) },
        ],
        Scale::Auto { floor: 1024.0 * 1024.0 },
        fmt::rate,
        56,
    );
    if let Some(legend) = gr.root.first_child() {
        legend.set_visible(false);
    }
    let read = charts::tile("Read");
    let write = charts::tile("Write");
    let busy = charts::tile("Busy");
    let latency = charts::tile("Response");
    let nums = charts::flow_row(
        &[read.root.upcast_ref(), write.root.upcast_ref(), busy.root.upcast_ref(), latency.root.upcast_ref()],
        84,
        4,
        true,
    );
    nums.add_css_class("metric-strip");
    nums.add_css_class("disk-nums");
    nums.set_valign(gtk::Align::Center);
    gr.root.add_css_class("disk-graph");
    c.root.append(&charts::flow_row(&[gr.root.upcast_ref(), nums.upcast_ref()], 300, 2, true));

    live::on_tick(&c.root, move |s| {
        let Some(d) = s.disks.iter().find(|x| x.name == name) else { return };
        temp.set_text(&d.temp.map(fmt::temp).unwrap_or_default());
        read.value.set_text(&fmt::rate(d.read_bps));
        write.value.set_text(&fmt::rate(d.write_bps));
        busy.value.set_text(&fmt::pct(d.busy));
        latency.value.set_text(&if d.read_iops + d.write_iops > 0.0 { fmt::ms(d.latency_ms) } else { "–".into() });
        let warn = prefs::get().warn_disk;
        for (dev, state, bar, value) in &cells {
            let Some(m) = d.mounts.iter().find(|m| &m.device == dev && m.total > 0) else { continue };
            let f = m.used() as f64 / m.total as f64;
            let level = if f * 100.0 >= warn.max(95.0) {
                2
            } else if f * 100.0 >= warn {
                1
            } else {
                0
            };
            if state.get() != (f, level) {
                state.set((f, level));
                bar.queue_draw();
            }
            value.set_text(&super::dashboard::used_of(m.used() as f64, m.total as f64));
        }
    });
    c.root
}

fn rounded(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    let r = r.min(h / 2.0).min(w / 2.0).max(0.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, 3.0 * FRAC_PI_2);
    cr.close_path();
}
