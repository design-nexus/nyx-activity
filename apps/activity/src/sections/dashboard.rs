//! The Dashboard, Activity's first page: what needs a look comes first, in plain
//! words with a button to deal with it, then big cards for the CPU (with a strip
//! of every thread), memory split into apps, kernel and cache, every disk's
//! fullness, the network, the GPU, the sensors and the busiest apps. Edit the
//! dashboard to show, hide and drag the cards around, and to set when to warn.

use crate::alerts::{self, Go, Problem};
use crate::graph::{self, Scale, Series, Tone};
use crate::sampler::Snapshot;
use crate::sampler::gpu::Power;
use crate::widgets::{self, Page};
use crate::{charts, cmd, fmt, live, prefs, window};
use gtk::prelude::*;
use gtk::{cairo, gdk, glib, pango};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Every card, in the default order, with its name.
pub const CARDS: &[(&str, &str)] = &[
    ("cpu", "CPU"),
    ("mem", "Memory"),
    ("storage", "Storage"),
    ("net", "Network"),
    ("gpu", "GPU"),
    ("sensors", "Sensors"),
    ("busiest", "Busiest right now"),
    ("battery", "Battery"),
    ("startup", "Startup"),
    ("load", "Load average"),
];

/// Letter tiles for apps take these swatches, picked by name.
const APP_SWATCHES: &[&str] = &["amber", "coral", "green", "blue", "teal", "violet", "pink", "red"];

/// The cards in the saved order; ones the order doesn't name yet go where they are by default.
fn order() -> Vec<&'static str> {
    let saved = prefs::get().dashboard_order;
    let mut out: Vec<&'static str> =
        saved.iter().filter_map(|s| CARDS.iter().find(|(id, _)| id == s).map(|(id, _)| *id)).collect();
    for (i, (id, _)) in CARDS.iter().enumerate() {
        if !out.contains(id) {
            // After the card that comes before it by default, so new cards land in place.
            let at = CARDS[..i].iter().rev().find_map(|(prev, _)| out.iter().position(|o| o == prev)).map_or(0, |p| p + 1);
            out.insert(at.min(out.len()), id);
        }
    }
    out
}

fn shown(id: &str) -> bool {
    !prefs::get().dashboard_hidden.iter().any(|h| h == id)
}

/// Move card `from` to where `to` is.
pub fn moved(order: &[&'static str], from: &str, to: &str) -> Vec<&'static str> {
    let mut out = order.to_vec();
    let (Some(fi), Some(_)) = (out.iter().position(|o| *o == from), out.iter().position(|o| *o == to)) else { return out };
    let id = out.remove(fi);
    let ti = out.iter().position(|o| *o == to).unwrap_or(out.len());
    out.insert(if fi <= ti { ti + 1 } else { ti }, id);
    out
}

// ---------- Layout ----------

const GAP: i32 = 12;

/// Three columns when there's room, two in a narrow window or beside the editor, one when tiny.
pub fn columns_for(width: i32) -> usize {
    if width >= 940 {
        3
    } else if width >= 520 {
        2
    } else {
        1
    }
}

/// Where each card goes, row by row as CSS grid does it: (row, column, span). A card
/// too wide for what's left of a row starts the next one.
pub fn place(spans: &[usize], cols: usize) -> Vec<(usize, usize, usize)> {
    let (mut row, mut col) = (0, 0);
    spans
        .iter()
        .map(|s| {
            let s = (*s).clamp(1, cols.max(1));
            if col + s > cols {
                row += 1;
                col = 0;
            }
            let at = (row, col, s);
            col += s;
            at
        })
        .collect()
}

fn span_of(w: &gtk::Widget) -> usize {
    if w.has_css_class("span-3") {
        3
    } else if w.has_css_class("span-2") {
        2
    } else {
        1
    }
}

fn cards_in(w: &gtk::Widget) -> Vec<gtk::Widget> {
    let mut out = Vec::new();
    let mut c = w.first_child();
    while let Some(child) = c {
        if child.should_layout() {
            out.push(child.clone());
        }
        c = child.next_sibling();
    }
    out
}

struct Placed {
    widget: gtk::Widget,
    x: i32,
    width: i32,
    row: usize,
}

fn layout(w: &gtk::Widget, width: i32) -> (Vec<Placed>, Vec<i32>) {
    let kids = cards_in(w);
    let cols = columns_for(width);
    let col_w = ((width - GAP * (cols as i32 - 1)) / cols as i32).max(1);
    let spans: Vec<usize> = kids.iter().map(span_of).collect();
    let places = place(&spans, cols);
    let rows = places.last().map_or(0, |p| p.0 + 1);
    let mut heights = vec![0; rows];
    let placed: Vec<Placed> = kids
        .into_iter()
        .zip(places)
        .map(|(widget, (row, col, span))| {
            let cw = col_w * span as i32 + GAP * (span as i32 - 1);
            let x = col as i32 * (col_w + GAP);
            let h = widget.measure(gtk::Orientation::Vertical, cw).1;
            heights[row] = heights[row].max(h);
            Placed { widget, x, width: cw, row }
        })
        .collect();
    (placed, heights)
}

mod imp {
    use gtk::glib;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;

    #[derive(Default)]
    pub struct DashLayout;

    #[glib::object_subclass]
    impl ObjectSubclass for DashLayout {
        const NAME: &'static str = "ActivityDashLayout";
        type Type = super::DashLayout;
        type ParentType = gtk::LayoutManager;
    }

    impl ObjectImpl for DashLayout {}

    impl LayoutManagerImpl for DashLayout {
        fn request_mode(&self, _: &gtk::Widget) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, w: &gtk::Widget, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            if orientation == gtk::Orientation::Horizontal {
                let min = super::cards_in(w).iter().map(|c| c.measure(orientation, -1).0).max().unwrap_or(0);
                return (min, min * 3 + super::GAP * 2, -1, -1);
            }
            let width = if for_size > 0 { for_size } else { 900 };
            let (_, heights) = super::layout(w, width);
            let total = heights.iter().sum::<i32>() + super::GAP * (heights.len() as i32 - 1).max(0);
            (total, total, -1, -1)
        }

        fn allocate(&self, w: &gtk::Widget, width: i32, _: i32, _: i32) {
            let (placed, heights) = super::layout(w, width);
            let mut tops = Vec::with_capacity(heights.len());
            let mut y = 0;
            for h in &heights {
                tops.push(y);
                y += h + super::GAP;
            }
            for p in placed {
                p.widget.size_allocate(&gtk::Allocation::new(p.x, tops[p.row], p.width, heights[p.row]), -1);
            }
        }
    }
}

glib::wrapper! {
    /// The Dashboard's grid: cards in columns, some two or three columns wide, each row as tall as its tallest card.
    pub struct DashLayout(ObjectSubclass<imp::DashLayout>) @extends gtk::LayoutManager;
}

// ---------- State ----------

struct Card {
    id: &'static str,
    root: gtk::Box,
    grip: gtk::Image,
    more: gtk::Button,
    hide: gtk::Button,
}

struct Dash {
    grid: gtk::Box,
    cards: Vec<Card>,
    editor: gtk::Box,
    editor_cards: gtk::Box,
    editing: bool,
}

thread_local! {
    static DASH: RefCell<Option<Dash>> = const { RefCell::new(None) };
}

pub fn editing() -> bool {
    DASH.with(|d| d.borrow().as_ref().is_some_and(|d| d.editing))
}

/// Put the cards in the saved order and show only the ones turned on.
fn arrange() {
    DASH.with(|d| {
        let d = d.borrow();
        let Some(d) = d.as_ref() else { return };
        let mut prev: Option<gtk::Widget> = None;
        for id in order() {
            if let Some(c) = d.cards.iter().find(|c| c.id == id) {
                d.grid.reorder_child_after(&c.root, prev.as_ref());
                c.root.set_visible(shown(id));
                prev = Some(c.root.clone().upcast());
            }
        }
        d.grid.queue_resize();
    });
}

/// Start or stop editing: the editor beside the cards, and each card's grip and Hide.
pub fn edit(on: bool) {
    window::navigate("overview");
    DASH.with(|d| {
        let mut d = d.borrow_mut();
        let Some(d) = d.as_mut() else { return };
        d.editing = on;
        d.editor.set_visible(on);
        for c in &d.cards {
            c.grip.set_visible(on);
            c.hide.set_visible(on);
            c.more.set_visible(!on);
            if on {
                c.root.add_css_class("editing");
            } else {
                c.root.remove_css_class("editing");
            }
        }
    });
    fill_editor_cards();
}

pub fn toggle_edit() {
    edit(!editing());
}

fn set_shown(id: &str, on: bool) {
    prefs::update(|p| {
        p.dashboard_hidden.retain(|h| h != id);
        if !on {
            p.dashboard_hidden.push(id.to_string());
        }
    });
    arrange();
    fill_editor_cards();
}

fn move_card(from: &str, to: &str) {
    if from == to {
        return;
    }
    let new = moved(&order(), from, to);
    prefs::update(|p| p.dashboard_order = new.iter().map(|s| s.to_string()).collect());
    arrange();
    fill_editor_cards();
}

// ---------- Cards ----------

/// A card: its icon and title (and a note beside it), a link to its page, then `body`.
/// While editing, a grip to drag it by and Hide take the link's place.
fn card(id: &'static str, icon: &str, more: &str, go: impl Fn() + 'static, span: usize) -> (Card, gtk::Label) {
    let title = CARDS.iter().find(|(c, _)| *c == id).map_or(id, |(_, t)| *t);
    let root = widgets::vbox(6);
    root.add_css_class("dash-card");
    if span > 1 {
        root.add_css_class(&format!("span-{span}"));
    }
    let head = widgets::hbox(7);
    head.add_css_class("dash-head");
    let grip = gtk::Image::from_icon_name("list-drag-handle-symbolic");
    grip.add_css_class("dash-grip");
    grip.set_visible(false);
    head.append(&grip);
    let ic = gtk::Image::from_icon_name(icon);
    ic.add_css_class("dash-icon");
    head.append(&ic);
    head.append(&widgets::label(title, "dash-title"));
    let meta = widgets::label("", "dash-meta");
    meta.add_css_class("tnum");
    meta.set_hexpand(true);
    meta.set_ellipsize(pango::EllipsizeMode::End);
    head.append(&meta);
    let more_b = gtk::Button::with_label(more);
    more_b.add_css_class("flat");
    more_b.add_css_class("dash-more");
    more_b.connect_clicked(move |_| go());
    head.append(&more_b);
    let hide = widgets::icon_button("view-reveal-symbolic", &format!("Hide {title}"));
    hide.add_css_class("dash-hide");
    hide.set_visible(false);
    hide.connect_clicked(move |_| set_shown(id, false));
    head.append(&hide);
    root.append(&head);

    // Drag a card onto another to put it there (while editing).
    let src = gtk::DragSource::new();
    src.set_actions(gdk::DragAction::MOVE);
    let r = root.downgrade();
    src.connect_prepare(move |src, _, _| {
        if !editing() {
            return None;
        }
        if let Some(r) = r.upgrade() {
            src.set_icon(Some(&gtk::WidgetPaintable::new(Some(&r))), 24, 16);
        }
        Some(gdk::ContentProvider::for_value(&id.to_value()))
    });
    root.add_controller(src);
    let drop = gtk::DropTarget::new(glib::Type::STRING, gdk::DragAction::MOVE);
    let r = root.downgrade();
    drop.connect_enter(move |_, _, _| {
        if let Some(r) = r.upgrade() {
            r.add_css_class("drop-here");
        }
        gdk::DragAction::MOVE
    });
    let r = root.downgrade();
    drop.connect_leave(move |_| {
        if let Some(r) = r.upgrade() {
            r.remove_css_class("drop-here");
        }
    });
    let r = root.downgrade();
    drop.connect_drop(move |_, value, _, _| {
        if let Some(r) = r.upgrade() {
            r.remove_css_class("drop-here");
        }
        match value.get::<String>() {
            Ok(from) => {
                let from = from.clone();
                // After the drop finishes, not while GTK is still delivering it.
                glib::idle_add_local_once(move || move_card(&from, id));
                true
            }
            Err(_) => false,
        }
    });
    root.add_controller(drop);

    (Card { id, root, grip, more: more_b, hide }, meta)
}

/// The big reading at the top of a card, with a smaller note after it.
fn big_value(card: &gtk::Box) -> (gtk::Label, gtk::Label) {
    let row = widgets::hbox(8);
    row.add_css_class("dash-big");
    let v = widgets::label("–", "dash-value");
    v.add_css_class("tnum");
    v.set_valign(gtk::Align::BaselineFill);
    let s = widgets::label("", "dash-sub");
    s.add_css_class("tnum");
    s.set_valign(gtk::Align::BaselineFill);
    s.set_ellipsize(pango::EllipsizeMode::End);
    row.append(&v);
    row.append(&s);
    card.append(&row);
    (v, s)
}

/// A dim line of small print at the foot of a card.
fn foot(card: &gtk::Box) -> gtk::Label {
    let l = widgets::label("", "dash-foot");
    l.add_css_class("tnum");
    l.set_ellipsize(pango::EllipsizeMode::End);
    l.set_xalign(0.0);
    card.append(&l);
    l
}

fn spark(key: &str, tone: &'static str, scale: Scale, height: i32, f: fn(f64) -> String) -> gtk::DrawingArea {
    let g = graph::sparkline(vec![Series::new(key, "", Tone::Swatch(tone))], scale, height, f);
    g.set_vexpand(true);
    g.set_valign(gtk::Align::End);
    g
}

fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(h / 2.0).min(w / 2.0).max(0.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
    cr.arc(x + r, y + r, r, std::f64::consts::PI, 3.0 * std::f64::consts::FRAC_PI_2);
    cr.close_path();
}

fn track_alpha() -> f64 {
    if crate::theme::palette().light { 0.09 } else { 0.07 }
}

/// One cell per thread, brighter the busier it is.
fn heat_strip() -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_height(14);
    area.set_hexpand(true);
    area.add_css_class("dash-heat");
    let cores: Rc<RefCell<Vec<f64>>> = Rc::default();
    {
        let cores = cores.clone();
        area.set_draw_func(move |_, cr, w, h| {
            let cores = cores.borrow();
            let n = cores.len().max(1) as f64;
            let gap = if n > 24.0 { 2.0 } else { 3.0 };
            let cw = ((w as f64 - gap * (n - 1.0)) / n).max(1.0);
            let c = charts::swatch("blue");
            for (i, v) in cores.iter().enumerate() {
                rounded(cr, i as f64 * (cw + gap), 0.0, cw, h as f64, 3.0);
                let a = 0.14 + 0.86 * (v / 100.0).clamp(0.0, 1.0);
                cr.set_source_rgba(c.0, c.1, c.2, a);
                let _ = cr.fill();
            }
        });
    }
    let motion = gtk::EventControllerMotion::new();
    {
        let (cores, a) = (cores.clone(), area.clone());
        motion.connect_motion(move |_, x, _| {
            let cores = cores.borrow();
            let n = cores.len();
            if n == 0 || a.width() == 0 {
                return;
            }
            let i = ((x / a.width() as f64) * n as f64).floor().clamp(0.0, n as f64 - 1.0) as usize;
            a.set_tooltip_text(Some(&format!("Thread {}: {}", i, fmt::pct(cores[i]))));
        });
    }
    area.add_controller(motion);
    let a = area.clone();
    live::on_tick(&area, move |s| {
        if *cores.borrow() != s.cpu.cores {
            *cores.borrow_mut() = s.cpu.cores.clone();
            a.queue_draw();
        }
    });
    area
}

fn cpu_card() -> Card {
    let (c, meta) = card("cpu", "activity-cpu-symbolic", "Details", || window::navigate("cpu"), 2);
    let (v, sub) = big_value(&c.root);
    c.root.append(&spark("cpu", "blue", Scale::Percent, 84, fmt::pct));
    c.root.append(&heat_strip());
    live::on_tick(&c.root, move |s| {
        meta.set_text(&format!("{} threads", s.cpu.cores.len()));
        v.set_text(&fmt::pct(s.cpu.usage));
        let avg = if s.cpu.freqs.is_empty() { 0.0 } else { s.cpu.freqs.iter().sum::<f64>() / s.cpu.freqs.len() as f64 };
        let mut parts = Vec::new();
        if avg > 0.0 {
            parts.push(fmt::mhz(avg));
        }
        parts.push(format!("load {:.2}", s.cpu.load[0]));
        sub.set_text(&parts.join(" · "));
    });
    c
}

/// The parts of a split bar and what they're out of.
type Parts = Rc<RefCell<(Vec<f64>, f64)>>;

/// A bar split into swatch-coloured parts over a track, out of `total`.
fn split_bar(colours: &'static [&'static str]) -> (gtk::DrawingArea, Parts) {
    let area = gtk::DrawingArea::new();
    area.set_content_height(10);
    area.set_hexpand(true);
    let parts: Parts = Rc::new(RefCell::new((Vec::new(), 1.0)));
    let p = parts.clone();
    area.set_draw_func(move |_, cr, w, h| {
        let (w, h) = (w as f64, h as f64);
        rounded(cr, 0.0, 0.0, w, h, h / 2.0);
        cr.clip_preserve();
        let t = crate::theme::Rgb::hex(&crate::theme::palette().text);
        cr.set_source_rgba(t.0, t.1, t.2, track_alpha());
        let _ = cr.fill();
        let (values, total) = &*p.borrow();
        let mut x = 0.0;
        for (i, v) in values.iter().enumerate() {
            let pw = (v / total.max(1e-9)).clamp(0.0, 1.0) * w;
            if pw <= 0.5 {
                continue;
            }
            cr.rectangle(x, 0.0, (pw - 2.0).max(1.0).min(w - x), h);
            let c = charts::swatch(colours[i % colours.len()]);
            cr.set_source_rgb(c.0, c.1, c.2);
            let _ = cr.fill();
            x += pw;
        }
    });
    (area, parts)
}

/// "■ Apps 16.0 GiB" keys under a split bar.
fn key_row(names: &[(&str, &'static str)]) -> (gtk::Box, Vec<gtk::Label>) {
    let row = widgets::hbox(14);
    row.add_css_class("dash-keys");
    let mut values = Vec::new();
    for (name, colour) in names {
        let item = widgets::hbox(5);
        let key = gtk::DrawingArea::new();
        key.set_content_width(8);
        key.set_content_height(8);
        key.set_valign(gtk::Align::Center);
        let colour: &'static str = colour;
        key.set_draw_func(move |_, cr, w, h| {
            rounded(cr, 0.0, 0.0, w as f64, h as f64, 2.0);
            let c = charts::swatch(colour);
            cr.set_source_rgb(c.0, c.1, c.2);
            let _ = cr.fill();
        });
        item.append(&key);
        let l = widgets::label(name, "dash-key");
        l.add_css_class("tnum");
        item.append(&l);
        row.append(&item);
        values.push(l);
    }
    (row, values)
}

/// "96 of 200 GiB": both in the bigger one's unit.
pub fn used_of(used: f64, total: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut i = 0;
    let mut div = 1.0;
    while total / div >= 1024.0 && i < UNITS.len() - 1 {
        div *= 1024.0;
        i += 1;
    }
    let num = |v: f64| {
        let v = v / div;
        if v >= 10.0 || i == 0 { format!("{v:.0}") } else { format!("{v:.1}") }
    };
    format!("{} of {} {}", num(used), num(total), UNITS[i])
}

fn mem_card() -> Card {
    const PARTS: &[&str] = &["amber", "violet", "teal"];
    let (c, _) = card("mem", "activity-memory-symbolic", "Details", || window::navigate("memory"), 1);
    let (v, sub) = big_value(&c.root);
    let (bar, parts) = split_bar(PARTS);
    bar.set_margin_top(4);
    c.root.append(&bar);
    let (keys, labels) = key_row(&[("Apps", "amber"), ("Kernel", "violet"), ("Cache", "teal")]);
    c.root.append(&keys);
    let swap = foot(&c.root);
    let area = bar.clone();
    live::on_tick(&c.root, move |s| {
        let m = &s.mem;
        v.set_text(&fmt::bytes(m.used as f64));
        widgets::set_warn(&v, m.total > 0 && m.used as f64 / m.total as f64 * 100.0 >= prefs::get().warn_mem);
        sub.set_text(&format!("of {}", fmt::bytes(m.total as f64)));
        let kernel = m.kernel.min(m.used);
        let apps = m.used.saturating_sub(kernel);
        let cache = m.cached + m.buffers;
        let values = vec![apps as f64, kernel as f64, cache as f64];
        if parts.borrow().0 != values {
            *parts.borrow_mut() = (values, m.total as f64);
            area.queue_draw();
        }
        labels[0].set_text(&format!("Apps {}", fmt::bytes(apps as f64)));
        labels[1].set_text(&format!("Kernel {}", fmt::bytes(kernel as f64)));
        labels[2].set_text(&format!("Cache {}", fmt::bytes(cache as f64)));
        area.set_tooltip_text(Some(
            "Apps: what programs hold. Kernel: what Linux keeps for itself. Cache: files kept in memory, given back whenever an app needs it.",
        ));
        swap.set_text(&if m.swap_total == 0 && m.zram_data == 0 {
            "No swap".to_string()
        } else if m.zram_used > 0 {
            let ratio = m.zram_data as f64 / m.zram_used as f64;
            format!("Swap and zram: {} used, packed {ratio:.1} to 1", fmt::bytes(m.swap_used as f64))
        } else {
            format!("Swap: {} of {} used", fmt::bytes(m.swap_used as f64), fmt::bytes(m.swap_total as f64))
        });
    });
    c
}

/// A thin fullness bar, red past `warn`.
fn fill_bar() -> (gtk::DrawingArea, Rc<Cell<(f64, bool)>>) {
    let area = gtk::DrawingArea::new();
    area.set_content_height(6);
    area.set_hexpand(true);
    area.set_valign(gtk::Align::Center);
    let state = Rc::new(Cell::new((0.0f64, false)));
    let st = state.clone();
    area.set_draw_func(move |_, cr, w, h| {
        let (w, h) = (w as f64, h as f64);
        let p = crate::theme::palette();
        let t = crate::theme::Rgb::hex(&p.text);
        rounded(cr, 0.0, 0.0, w, h, h / 2.0);
        cr.set_source_rgba(t.0, t.1, t.2, track_alpha());
        let _ = cr.fill();
        let (f, warn) = st.get();
        let fw = f.clamp(0.0, 1.0) * w;
        if fw > 0.0 {
            rounded(cr, 0.0, 0.0, fw.max(h), h, h / 2.0);
            let c = crate::theme::Rgb::hex(if warn { &p.danger } else { &p.accent });
            cr.set_source_rgb(c.0, c.1, c.2);
            let _ = cr.fill();
        }
    });
    (area, state)
}

fn storage_card() -> Card {
    let (c, _) = card("storage", "drive-harddisk-symbolic", "Details", || window::navigate("storage"), 1);
    let rows = gtk::Grid::new();
    rows.set_column_spacing(10);
    rows.set_row_spacing(6);
    rows.add_css_class("dash-mounts");
    c.root.append(&rows);
    let io = foot(&c.root);
    let shown: RefCell<Vec<String>> = RefCell::default();
    type Row = (gtk::Label, Rc<Cell<(f64, bool)>>, gtk::DrawingArea, gtk::Label);
    let cells: RefCell<Vec<Row>> = RefCell::default();
    live::on_tick(&c.root, move |s| {
        let mounts = alerts::mounts(s);
        let paths: Vec<String> = mounts.iter().map(|m| m.path.clone()).collect();
        if *shown.borrow() != paths {
            while let Some(ch) = rows.first_child() {
                rows.remove(&ch);
            }
            let mut out = Vec::new();
            for (i, path) in paths.iter().enumerate() {
                let name = widgets::label(path, "dash-mount");
                name.set_ellipsize(pango::EllipsizeMode::Middle);
                name.set_width_chars(9);
                name.set_max_width_chars(12);
                name.set_xalign(0.0);
                name.set_tooltip_text(Some(path));
                let (bar, state) = fill_bar();
                let val = widgets::label("", "dash-mount-value");
                val.add_css_class("tnum");
                val.set_xalign(1.0);
                val.set_width_chars(12);
                rows.attach(&name, 0, i as i32, 1, 1);
                rows.attach(&bar, 1, i as i32, 1, 1);
                rows.attach(&val, 2, i as i32, 1, 1);
                out.push((val, state, bar, name));
            }
            if paths.is_empty() {
                rows.attach(&widgets::label("No disks mounted.", "dash-foot"), 0, 0, 3, 1);
            }
            *cells.borrow_mut() = out;
            *shown.borrow_mut() = paths;
        }
        let warn = prefs::get().warn_disk;
        for (m, (val, state, bar, name)) in mounts.iter().zip(cells.borrow().iter()) {
            let f = m.used() as f64 / m.total as f64;
            let st = (f, f * 100.0 >= warn);
            if state.get() != st {
                state.set(st);
                bar.queue_draw();
            }
            val.set_text(&used_of(m.used() as f64, m.total as f64));
            name.set_tooltip_text(Some(&format!("{} · {} free · {}", m.path, fmt::bytes(m.free as f64), m.fs)));
        }
        io.set_text(&format!("↓ Read {}    ↑ Write {}", fmt::rate(s.disk_read()), fmt::rate(s.disk_write())));
    });
    c
}

fn net_card() -> Card {
    let (c, _) = card("net", "network-wireless-symbolic", "Details", || window::navigate("network"), 1);
    let (v, sub) = big_value(&c.root);
    let g = graph::sparkline(
        vec![
            Series::new("net.rx", "Down", Tone::Swatch("pink")),
            Series { dashed: true, ..Series::new("net.tx", "Up", Tone::Swatch("pink")) },
        ],
        Scale::Auto { floor: 64.0 * 1024.0 },
        54,
        fmt::net_rate,
    );
    g.set_vexpand(true);
    g.set_valign(gtk::Align::End);
    c.root.append(&g);
    let link = foot(&c.root);
    live::on_tick(&c.root, move |s| {
        v.set_text(&fmt::net_rate(s.net_rx()));
        sub.set_text(&format!("down · {} up", fmt::net_rate(s.net_tx())));
        link.set_text(&link_text(s));
    });
    c
}

/// "Wi-Fi 6 · 5 GHz · signal good", "Ethernet · 1000 Mb/s", or "Not connected".
fn link_text(s: &Snapshot) -> String {
    use crate::sampler::net::Kind;
    let real = || s.nets.iter().filter(|n| n.up && n.kind != Kind::Virtual);
    let Some(n) = real().find(|n| n.gateway.is_some()).or_else(|| real().max_by(|a, b| a.rx_total.cmp(&b.rx_total))) else {
        return "Not connected".into();
    };
    match n.kind {
        Kind::Wifi => {
            let w = n.wifi.as_ref();
            let mut parts = vec![w.and_then(|w| w.standard).unwrap_or("Wi-Fi").to_string()];
            if let Some(b) = w.and_then(|w| w.band()) {
                parts.push(b.to_string());
            }
            if let Some(dbm) = n.signal.or(w.and_then(|w| w.signal)) {
                let q = fmt::signal_quality(dbm);
                parts.push(format!(
                    "signal {}",
                    if q >= 70.0 {
                        "good"
                    } else if q >= 40.0 {
                        "fair"
                    } else {
                        "weak"
                    }
                ));
            }
            if let Some(ssid) = n.ssid.as_ref().or(w.and_then(|w| w.ssid.as_ref())) {
                parts.push(ssid.clone());
            }
            parts.join(" · ")
        }
        _ => match n.speed_mbps {
            Some(sp) => format!("Ethernet · {sp} Mb/s"),
            None => "Ethernet".into(),
        },
    }
}

fn short_gpu(name: &str) -> String {
    name.replace("NVIDIA ", "").replace("Intel ", "").replace("AMD ", "")
}

fn gpu_card() -> Card {
    let (c, _) = card("gpu", "activity-gpu-symbolic", "Details", || window::navigate("gpu"), 1);
    let (v, sub) = big_value(&c.root);
    let holder = widgets::vbox(0);
    holder.set_vexpand(true);
    holder.set_valign(gtk::Align::End);
    c.root.append(&holder);
    let engines = foot(&c.root);
    let shown_pci: RefCell<String> = RefCell::default();
    live::on_tick(&c.root, move |s| {
        let Some(g) = s.gpus.iter().max_by(|a, b| a.util.unwrap_or(-1.0).total_cmp(&b.util.unwrap_or(-1.0))) else {
            v.set_text("–");
            sub.set_text(if s.procs.is_none() { "Paused while hidden" } else { "No graphics card found" });
            engines.set_text("");
            return;
        };
        if *shown_pci.borrow() != g.pci {
            while let Some(ch) = holder.first_child() {
                holder.remove(&ch);
            }
            holder.append(&spark(&format!("gpu.{}.util", g.pci), "violet", Scale::Percent, 54, fmt::pct));
            *shown_pci.borrow_mut() = g.pci.clone();
        }
        let mut note = vec![short_gpu(&g.name)];
        if g.power_state == Power::Asleep {
            v.set_text("Asleep");
            engines.set_text("Powered down to save energy");
        } else {
            v.set_text(&fmt::pct(g.util.unwrap_or(0.0)));
            if let Some(m) = g.vram_used.or(g.client_mem) {
                note.push(fmt::bytes(m as f64));
            }
            let mut es: Vec<&(String, f64)> = g.engines.iter().collect();
            es.sort_by(|a, b| b.1.total_cmp(&a.1));
            let text: Vec<String> = es
                .iter()
                .take(3)
                .enumerate()
                .map(|(i, (n, p))| {
                    let n = n.replace(['-', '_'], " ");
                    let n = if i == 0 {
                        let mut c = n.chars();
                        c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
                    } else {
                        n
                    };
                    format!("{n} {}", fmt::pct(*p))
                })
                .collect();
            engines.set_text(&text.join(" · "));
        }
        sub.set_text(&note.join(" · "));
    });
    c
}

fn sensors_card() -> Card {
    let (c, _) = card("sensors", "activity-thermometer-symbolic", "Details", || window::navigate("sensors"), 1);
    let list = widgets::vbox(2);
    c.root.append(&list);
    let names = ["CPU package", "NVMe", "GPU", "Fans", "Battery"];
    let mut rows = Vec::new();
    for n in names {
        let r = widgets::hbox(8);
        r.add_css_class("dash-sensor");
        let k = widgets::label(n, "dash-sensor-name");
        k.set_hexpand(true);
        k.set_xalign(0.0);
        let v = widgets::label("", "dash-sensor-value");
        v.add_css_class("tnum");
        r.append(&k);
        r.append(&v);
        list.append(&r);
        rows.push((r, v));
    }
    let none = widgets::label("No sensors found.", "dash-foot");
    none.set_xalign(0.0);
    list.append(&none);
    live::on_tick(&c.root, move |s| {
        let p = prefs::get();
        let nvme = s.disks.iter().find_map(|d| d.temp);
        let gpu = s.gpus.iter().find_map(|g| g.temp);
        let fan = s.sensors.fans.iter().map(|f| f.value).fold(0.0, f64::max);
        let bat = s.sensors.batteries.first();
        let values: [Option<(String, bool)>; 5] = [
            s.sensors.cpu_temp.map(|t| (fmt::temp(t), t >= p.warn_temp)),
            nvme.map(|t| (fmt::temp(t), false)),
            gpu.map(|t| (fmt::temp(t), false)),
            (fan > 0.0).then(|| (format!("{} rpm", thousands(fan.round() as u64)), false)),
            bat.map(|b| (format!("{}, {}", fmt::pct(b.percent), b.status.to_lowercase()), false)),
        ];
        let mut any = false;
        for ((row, v), val) in rows.iter().zip(values) {
            row.set_visible(val.is_some());
            if let Some((text, warn)) = val {
                any = true;
                v.set_text(&text);
                widgets::set_warn(v, warn);
            }
        }
        none.set_visible(!any);
    });
    c
}

/// A square in a swatch picked by the app's name, with its first letter.
fn letter_tile(name: &str) -> gtk::Box {
    let tile = widgets::hbox(0);
    tile.add_css_class("app-letter");
    let h = name.bytes().fold(0usize, |a, b| a.wrapping_mul(31).wrapping_add(b as usize));
    // The swatch's class sets `color`, which the tile's CSS paints as its background.
    tile.add_css_class(&format!("section-swatch-{}", APP_SWATCHES[h % APP_SWATCHES.len()]));
    tile.set_valign(gtk::Align::Center);
    tile.set_halign(gtk::Align::Start);
    tile.set_hexpand(false);
    let l = gtk::Label::new(name.chars().next().map(|c| c.to_uppercase().to_string()).as_deref());
    l.set_halign(gtk::Align::Center);
    l.set_width_chars(2);
    tile.append(&l);
    tile
}

/// 2140 → "2,140".
fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn busiest_card() -> Card {
    let (c, _) = card("busiest", "view-list-bullet-symbolic", "All processes", || window::navigate("processes"), 2);
    let list = widgets::vbox(0);
    c.root.append(&list);
    let empty = widgets::label("", "dash-foot");
    empty.set_xalign(0.0);
    c.root.append(&empty);
    live::on_tick(&c.root, move |s| {
        while let Some(ch) = list.first_child() {
            list.remove(&ch);
        }
        let mut gs: Vec<_> = s.groups.iter().filter(|g| g.cpu > 0.0 || g.memory() > 0).collect();
        gs.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.memory().cmp(&a.memory())));
        for g in gs.iter().take(4) {
            let (name, _) = super::top::group_identity(g, s);
            let b = gtk::Button::new();
            b.add_css_class("flat");
            b.add_css_class("dash-app");
            let row = widgets::hbox(10);
            row.append(&letter_tile(&name));
            let n = widgets::label(&name, "dash-app-name");
            n.set_hexpand(true);
            n.set_xalign(0.0);
            n.set_ellipsize(pango::EllipsizeMode::End);
            row.append(&n);
            for text in [fmt::pct(g.cpu), fmt::bytes(g.memory() as f64)] {
                let v = widgets::label(&text, "dash-app-value");
                v.add_css_class("tnum");
                v.set_xalign(1.0);
                v.set_width_chars(9);
                row.append(&v);
            }
            b.set_child(Some(&row));
            let procs = if g.procs == 1 { "1 process".to_string() } else { format!("{} processes", g.procs) };
            b.set_tooltip_text(Some(&format!("{name} · {procs}\nShow it in Processes")));
            let pid = g.leader;
            b.connect_clicked(move |_| super::processes::reveal(pid));
            list.append(&b);
        }
        empty.set_text(if s.procs.is_none() { "Paused while the window is hidden." } else { "Nothing busy right now." });
        empty.set_visible(gs.is_empty());
    });
    c
}

fn battery_card() -> Card {
    let (c, _) = card("battery", "battery-symbolic", "Details", || window::navigate("sensors"), 1);
    let (v, sub) = big_value(&c.root);
    let holder = widgets::vbox(0);
    holder.set_vexpand(true);
    holder.set_valign(gtk::Align::End);
    c.root.append(&holder);
    let note = foot(&c.root);
    let shown: RefCell<String> = RefCell::default();
    live::on_tick(&c.root, move |s| {
        let Some(b) = s.sensors.batteries.first() else {
            v.set_text("–");
            sub.set_text("No battery");
            return;
        };
        if *shown.borrow() != b.name {
            while let Some(ch) = holder.first_child() {
                holder.remove(&ch);
            }
            holder.append(&spark(&format!("bat.{}", b.name), "green", Scale::Percent, 54, fmt::pct));
            *shown.borrow_mut() = b.name.clone();
        }
        v.set_text(&fmt::pct(b.percent));
        sub.set_text(&b.status);
        let mut parts = Vec::new();
        if let Some(h) = b.hours_left {
            parts.push(format!("{} left", fmt::hours(h)));
        }
        if b.watts > 0.1 {
            parts.push(fmt::watts(b.watts));
        }
        if let Some(health) = b.health() {
            parts.push(format!("health {}", fmt::pct(health)));
        }
        note.set_text(&parts.join(" · "));
    });
    c
}

fn load_card() -> Card {
    let (c, _) = card("load", "activity-cpu-symbolic", "Details", || window::navigate("cpu"), 1);
    let (v, sub) = big_value(&c.root);
    c.root.append(&spark("cpu.load", "teal", Scale::Auto { floor: 1.0 }, 54, |v| format!("{v:.2}")));
    let note = foot(&c.root);
    live::on_tick(&c.root, move |s| {
        v.set_text(&format!("{:.2}", s.cpu.load[0]));
        sub.set_text("over 1 min");
        note.set_text(&format!(
            "{:.2} over 5 min · {:.2} over 15 min · up {}",
            s.cpu.load[1],
            s.cpu.load[2],
            fmt::duration(s.cpu.uptime)
        ));
    });
    c
}

struct StartupInfo {
    apps: usize,
    login: f64,
    slowest: Option<(String, f64)>,
}

fn startup_info() -> StartupInfo {
    use crate::startup;
    let managed = startup::load().hyprland.iter().filter(|m| m.enabled).count();
    let xdg = startup::xdg_entries().iter().filter(|e| e.enabled && !e.other_desktop).count();
    let apps = startup::user_entries().len() + startup::omarchy_entries().len() + managed + xdg;
    let login: f64 = startup::boot_time(true).iter().map(|(_, s)| s).sum();
    // A unit that took longer than the whole login (a backup a timer started) wasn't part of it.
    let slowest = startup::blame(true)
        .into_iter()
        .filter(|(_, s)| login <= 0.0 || *s <= login)
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(u, s)| (super::startup::pretty_unit(&u), s));
    StartupInfo { apps, login, slowest }
}

fn startup_card() -> Card {
    let (c, _) = card("startup", "system-reboot-symbolic", "Manage", || window::navigate("startup"), 3);
    let row = widgets::hbox(28);
    row.add_css_class("dash-startup");
    let apps = widgets::label("", "dash-startup-item");
    let login = widgets::label("", "dash-startup-item");
    let failed = widgets::label("", "dash-startup-item");
    for l in [&apps, &login, &failed] {
        l.set_wrap(true);
        l.set_xalign(0.0);
        row.append(l);
    }
    c.root.append(&row);
    let loaded: Rc<Cell<Option<std::time::Instant>>> = Rc::default();
    let load = {
        let (apps, login, loaded) = (apps.clone(), login.clone(), loaded.clone());
        move || {
            if loaded.get().is_some_and(|t| t.elapsed().as_secs() < 300) {
                return;
            }
            loaded.set(Some(std::time::Instant::now()));
            let (apps, login) = (apps.clone(), login.clone());
            cmd::background(startup_info, move |i| {
                apps.set_markup(&format!("<b>{}</b> {} at login", i.apps, if i.apps == 1 { "app starts" } else { "apps start" }));
                let took = super::startup::short_time(i.login);
                login.set_markup(&match (&i.slowest, i.login > 0.0) {
                    (Some((name, s)), true) => format!(
                        "Your last login took <b>{took}</b>; {} was slowest at {}",
                        glib::markup_escape_text(name),
                        super::startup::short_time(*s)
                    ),
                    (_, true) => format!("Your last login took <b>{took}</b>"),
                    _ => "Login wasn't timed".into(),
                });
            });
        }
    };
    load();
    c.root.connect_map(move |_| load());
    let refresh_failed = move || {
        let n = alerts::failed().len();
        failed.set_markup(&match n {
            0 => "No service has failed".to_string(),
            1 => "<b>1</b> service failed".to_string(),
            n => format!("<b>{n}</b> services failed"),
        });
    };
    refresh_failed();
    alerts::connect_failed(refresh_failed);
    c
}

// ---------- Problems ----------

fn problem_banner(p: &Problem) -> gtk::Box {
    let b = widgets::hbox(12);
    b.add_css_class("dash-alert");
    b.add_css_class(if p.bad { "bad" } else { "warn" });
    let icon = gtk::Image::from_icon_name(p.icon);
    icon.add_css_class("dash-alert-icon");
    b.append(&icon);
    let text = widgets::vbox(1);
    text.set_hexpand(true);
    text.set_valign(gtk::Align::Center);
    let t = widgets::label(&p.title, "dash-alert-title");
    t.set_xalign(0.0);
    t.set_ellipsize(pango::EllipsizeMode::End);
    let d = widgets::label(&p.detail, "dash-alert-detail");
    d.set_xalign(0.0);
    d.set_ellipsize(pango::EllipsizeMode::End);
    d.set_tooltip_text(Some(&p.detail));
    text.append(&t);
    text.append(&d);
    b.append(&text);
    let act = gtk::Button::with_label(p.action);
    act.add_css_class("dash-alert-action");
    act.set_valign(gtk::Align::Center);
    let go = p.go.clone();
    act.connect_clicked(move |_| match &go {
        Go::Page(id) => window::navigate(id),
        Go::Sorted(col) => super::processes::show_sorted(col),
        Go::Process(pid) => super::processes::reveal(*pid),
        Go::Logs(unit, user) => super::services::open_logs(unit, *user),
    });
    b.append(&act);
    b
}

/// The problems, two side by side when there's room. The holder stays on screen, so
/// it keeps hearing the ticks while there's nothing to show.
fn problems_row() -> gtk::Box {
    let holder = widgets::vbox(0);
    let row = charts::flow_row(&[], 420, 2, true);
    row.add_css_class("dash-alerts");
    holder.append(&row);
    let shown: Rc<RefCell<Vec<Problem>>> = Rc::default();
    let refresh: Rc<dyn Fn(&Snapshot)> = {
        let row = row.clone();
        Rc::new(move |s: &Snapshot| {
            let now = alerts::problems(s);
            if *shown.borrow() == now {
                return;
            }
            while let Some(c) = row.first_child() {
                row.remove(&c);
            }
            for p in &now {
                row.append(&problem_banner(p));
            }
            row.set_visible(!now.is_empty());
            *shown.borrow_mut() = now;
        })
    };
    row.set_visible(false);
    let r = refresh.clone();
    live::on_tick(&holder, move |s| r(s));
    let w = holder.downgrade();
    alerts::connect_failed(move || {
        if let (Some(_), Some(s)) = (w.upgrade(), live::latest()) {
            refresh(&s);
        }
    });
    holder
}

// ---------- Editor ----------

fn small_switch(active: bool) -> gtk::Switch {
    let s = gtk::Switch::new();
    s.set_active(active);
    s.set_valign(gtk::Align::Center);
    s
}

fn editor_row(title: &str, control: &impl IsA<gtk::Widget>) -> gtk::Box {
    let r = widgets::hbox(10);
    r.add_css_class("dash-editor-row");
    let l = widgets::label(title, "dash-editor-key");
    l.set_hexpand(true);
    l.set_xalign(0.0);
    l.set_wrap(true);
    l.set_max_width_chars(18);
    r.append(&l);
    control.set_valign(gtk::Align::Center);
    r.append(control);
    r
}

fn fill_editor_cards() {
    let Some(list) = DASH.with(|d| d.borrow().as_ref().map(|d| d.editor_cards.clone())) else { return };
    while let Some(c) = list.first_child() {
        list.remove(&c);
    }
    for id in order() {
        let name = CARDS.iter().find(|(c, _)| *c == id).map_or(id, |(_, n)| *n);
        let sw = small_switch(shown(id));
        sw.update_property(&[gtk::accessible::Property::Label(&format!("Show {name}"))]);
        sw.connect_active_notify(move |s| {
            let on = s.is_active();
            // Not from inside the handler of a switch that's about to be rebuilt.
            glib::idle_add_local_once(move || set_shown(id, on));
        });
        list.append(&editor_row(name, &sw));
    }
}

/// A dropdown of `(value, label)` choices, with the current value added when it isn't one.
fn choice(
    mut options: Vec<(f64, String)>,
    current: f64,
    label: impl Fn(f64) -> String,
    set: impl Fn(f64) + 'static,
) -> gtk::DropDown {
    if !options.iter().any(|(v, _)| (v - current).abs() < 0.5) {
        options.push((current, label(current)));
        options.sort_by(|a, b| a.0.total_cmp(&b.0));
    }
    let opts: Vec<(String, String)> = options.iter().map(|(v, l)| (v.to_string(), l.clone())).collect();
    let dd = widgets::dropdown(
        &opts,
        &options.iter().find(|(v, _)| (v - current).abs() < 0.5).map_or(String::new(), |(v, _)| v.to_string()),
    );
    dd.add_css_class("compact");
    dd.connect_selected_notify(move |d| {
        if let Some((v, _)) = options.get(d.selected() as usize) {
            set(*v);
        }
    });
    dd
}

fn editor() -> (gtk::Box, gtk::Box) {
    let panel = widgets::vbox(0);
    panel.add_css_class("dash-editor");
    panel.set_size_request(300, -1);
    panel.set_hexpand(false);
    let head = widgets::vbox(2);
    head.add_css_class("dash-editor-head");
    head.append(&widgets::label("Edit the dashboard", "dash-editor-title"));
    let hint = widgets::label("Drag cards to reorder them.", "dash-editor-hint");
    hint.set_xalign(0.0);
    head.append(&hint);
    panel.append(&head);

    let body = widgets::vbox(0);
    body.add_css_class("dash-editor-body");
    body.append(&widgets::label("Cards", "dash-editor-label"));
    let cards = widgets::vbox(0);
    cards.add_css_class("dash-editor-card");
    body.append(&cards);

    body.append(&widgets::label("Warn me when", "dash-editor-label"));
    let warn = widgets::vbox(0);
    warn.add_css_class("dash-editor-card");
    let p = prefs::get();
    // Stored in °C; shown in the unit picked in Settings.
    let temps: Vec<(f64, String)> = [70.0, 75.0, 80.0, 85.0, 90.0, 95.0, 100.0].iter().map(|c| (*c, fmt::temp(*c))).collect();
    warn.append(&editor_row(
        "CPU is hotter than",
        &choice(temps, p.warn_temp.round(), fmt::temp, |v| prefs::update(|p| p.warn_temp = v)),
    ));
    let pcts = |vals: &[f64]| vals.iter().map(|v| (*v, fmt::pct(*v))).collect::<Vec<_>>();
    warn.append(&editor_row(
        "Memory is fuller than",
        &choice(pcts(&[70.0, 75.0, 80.0, 85.0, 90.0, 95.0]), p.warn_mem.round(), fmt::pct, |v| prefs::update(|p| p.warn_mem = v)),
    ));
    warn.append(&editor_row(
        "A disk is fuller than",
        &choice(pcts(&[80.0, 85.0, 90.0, 95.0, 98.0]), p.warn_disk.round(), fmt::pct, |v| prefs::update(|p| p.warn_disk = v)),
    ));
    let mins = |s: f64| if s < 60.0 { format!("{s:.0} s") } else { format!("{:.0} min", s / 60.0) };
    let busy =
        choice([30.0, 60.0, 120.0, 300.0, 600.0].iter().map(|s| (*s, mins(*s))).collect(), p.alert_proc_secs as f64, mins, |v| {
            prefs::update(|p| p.alert_proc_secs = v as u64)
        });
    let busy_row = editor_row("One process stays busy for", &busy);
    busy_row
        .set_tooltip_text(Some(&format!("Busy means over {} of the processor (set in Settings)", fmt::pct(p.alert_proc_cpu))));
    warn.append(&busy_row);
    let any = p.alert_temp || p.alert_mem || p.alert_disk || p.alert_proc || p.alert_services;
    let notify = small_switch(any);
    notify.update_property(&[gtk::accessible::Property::Label("Also as a notification")]);
    notify.connect_active_notify(|s| {
        let on = s.is_active();
        prefs::update(|p| {
            p.alert_temp = on;
            p.alert_mem = on;
            p.alert_disk = on;
            p.alert_proc = on;
            p.alert_services = on;
        });
    });
    warn.append(&editor_row("Also as a notification", &notify));
    body.append(&warn);

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .vexpand(true)
        .child(&body)
        .build();
    panel.append(&scroll);

    let footer = widgets::hbox(8);
    footer.add_css_class("dash-editor-foot");
    let done = gtk::Button::with_label("Done");
    done.add_css_class("suggested-action");
    done.set_hexpand(true);
    done.set_halign(gtk::Align::End);
    done.connect_clicked(|_| edit(false));
    footer.append(&done);
    panel.append(&footer);
    panel.set_visible(false);
    (panel, cards)
}

/// The header's button for this page.
pub fn header_button() -> gtk::Widget {
    let b = gtk::Button::new();
    b.add_css_class("bar-button");
    b.add_css_class("dash-edit");
    let content = widgets::hbox(6);
    content.append(&gtk::Image::from_icon_name("document-edit-symbolic"));
    content.append(&widgets::label("Edit", ""));
    b.set_child(Some(&content));
    b.set_valign(gtk::Align::Center);
    b.set_tooltip_text(Some("Show, hide and reorder the cards, and choose when to warn"));
    b.connect_clicked(|_| toggle_edit());
    b.upcast()
}

pub fn build(page: &Page) {
    let split = widgets::hbox(0);
    split.set_vexpand(true);
    page.body.append(&split);
    let main = widgets::vbox(0);
    main.set_hexpand(true);
    main.add_css_class("dash-main");
    split.append(&main);
    main.append(&problems_row());

    let grid = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    grid.add_css_class("dash-grid");
    grid.set_layout_manager(Some(glib::Object::new::<DashLayout>()));
    let cards = vec![
        cpu_card(),
        mem_card(),
        storage_card(),
        net_card(),
        gpu_card(),
        sensors_card(),
        busiest_card(),
        battery_card(),
        startup_card(),
        load_card(),
    ];
    for c in &cards {
        grid.append(&c.root);
    }
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .vexpand(true)
        .child(&grid)
        .build();
    scroll.add_css_class("dash-scroll");
    main.append(&scroll);

    let (panel, editor_cards) = editor();
    split.append(&panel);
    DASH.with(|d| *d.borrow_mut() = Some(Dash { grid, cards, editor: panel, editor_cards, editing: false }));
    arrange();
    fill_editor_cards();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_cards_like_css_grid() {
        // CPU (2) and Memory on one row; Busiest (2) after Sensors; Startup a row to itself.
        let spans = [2, 1, 1, 1, 1, 1, 2, 3];
        assert_eq!(place(&spans, 3), [(0, 0, 2), (0, 2, 1), (1, 0, 1), (1, 1, 1), (1, 2, 1), (2, 0, 1), (2, 1, 2), (3, 0, 3)]);
        // Two columns: wide cards take a row each, Sensors is left alone before Busiest.
        assert_eq!(place(&spans, 2), [(0, 0, 2), (1, 0, 1), (1, 1, 1), (2, 0, 1), (2, 1, 1), (3, 0, 1), (4, 0, 2), (5, 0, 2)]);
        assert_eq!(place(&[2, 1], 1), [(0, 0, 1), (1, 0, 1)]);
    }

    #[test]
    fn moves_cards() {
        let o = ["a", "b", "c", "d"];
        assert_eq!(moved(&o, "a", "c"), ["b", "c", "a", "d"]);
        assert_eq!(moved(&o, "d", "b"), ["a", "d", "b", "c"]);
        assert_eq!(moved(&o, "x", "b"), o);
    }

    #[test]
    fn sizes_in_one_unit() {
        let g = 1024.0 * 1024.0 * 1024.0;
        assert_eq!(used_of(96.0 * g, 200.0 * g), "96 of 200 GiB");
        assert_eq!(used_of(1.1 * 1024.0 * g, 1.6 * 1024.0 * g), "1.1 of 1.6 TiB");
    }

    #[test]
    fn picks_columns_by_width() {
        assert_eq!(columns_for(1180), 3);
        assert_eq!(columns_for(880), 2);
        assert_eq!(columns_for(400), 1);
    }
}
