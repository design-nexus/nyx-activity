//! Charts beside the line graphs: rings, stacked bars, ranked bars and stat
//! tiles, plus the rows that keep cards side by side at one size. Colours come
//! from the theme tokens, like the graphs'.

use crate::theme::{self, Rgb};
use crate::widgets;
use gtk::cairo;
use gtk::glib;
use gtk::pango;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::f64::consts::{FRAC_PI_2, TAU};
use std::rc::Rc;

/// The colour of a stacked bar's `i`th part (and its legend key).
pub fn part_rgb(i: usize) -> Rgb {
    let p = theme::palette();
    let accent = Rgb::hex(&p.accent);
    let danger = Rgb::hex(&p.danger);
    let text = Rgb::hex(&p.text);
    let bg = Rgb::hex(&p.bg);
    match i % 6 {
        0 => accent,
        1 => accent.mix(danger, 0.55),
        2 => accent.mix(text, 0.55),
        3 => accent.mix(bg, 0.5),
        4 => danger.mix(bg, 0.2),
        _ => text.mix(bg, 0.6),
    }
}

/// A swatch of the theme's by name ("blue"), or the accent when the theme has no such swatch.
pub fn swatch(name: &str) -> Rgb {
    theme::swatches()
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, c)| Rgb::hex(&c))
        .unwrap_or_else(|| Rgb::hex(&theme::palette().accent))
}

fn set(cr: &cairo::Context, c: Rgb, a: f64) {
    cr.set_source_rgba(c.0, c.1, c.2, a);
}

fn track_rgb() -> Rgb {
    Rgb::hex(&theme::palette().text)
}

fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(h / 2.0).min(w / 2.0).max(0.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, FRAC_PI_2 * 2.0);
    cr.arc(x + r, y + r, r, FRAC_PI_2 * 2.0, FRAC_PI_2 * 3.0);
    cr.close_path();
}

// ---------- Ring ----------

/// A donut gauge with its value written in the middle.
#[derive(Clone)]
pub struct Ring {
    pub root: gtk::Overlay,
    area: gtk::DrawingArea,
    parts: Rc<RefCell<Vec<f64>>>,
    warn: Rc<Cell<bool>>,
    value: gtk::Label,
    sub: gtk::Label,
}

pub fn ring(size: i32) -> Ring {
    let area = gtk::DrawingArea::new();
    area.set_content_width(size);
    area.set_content_height(size);
    let parts: Rc<RefCell<Vec<f64>>> = Rc::default();
    let warn = Rc::new(Cell::new(false));
    {
        let (parts, warn) = (parts.clone(), warn.clone());
        area.set_draw_func(move |_, cr, w, h| {
            let (w, h) = (w as f64, h as f64);
            let thick = (w.min(h) * 0.085).clamp(5.0, 11.0);
            let r = w.min(h) / 2.0 - thick / 2.0 - 1.0;
            let (cx, cy) = (w / 2.0, h / 2.0);
            cr.set_line_width(thick);
            cr.arc(cx, cy, r, 0.0, TAU);
            set(cr, track_rgb(), 0.08);
            let _ = cr.stroke();
            let parts = parts.borrow();
            let single = parts.len() == 1;
            // One value gets round ends; several parts butt against each other.
            cr.set_line_cap(if single { cairo::LineCap::Round } else { cairo::LineCap::Butt });
            let mut at = 0.0;
            for (i, f) in parts.iter().enumerate() {
                let f = f.clamp(0.0, 1.0 - at);
                if f > 0.002 {
                    let c = if single && warn.get() { Rgb::hex(&theme::palette().danger) } else { part_rgb(i) };
                    cr.arc(cx, cy, r, -FRAC_PI_2 + at * TAU, -FRAC_PI_2 + (at + f) * TAU);
                    set(cr, c, 1.0);
                    let _ = cr.stroke();
                }
                at += f;
            }
        });
    }
    let root = gtk::Overlay::new();
    root.set_child(Some(&area));
    root.set_halign(gtk::Align::Center);
    root.set_valign(gtk::Align::Center);
    let text = widgets::vbox(0);
    text.set_halign(gtk::Align::Center);
    text.set_valign(gtk::Align::Center);
    text.set_can_target(false);
    let value = widgets::label("–", "ring-value");
    value.add_css_class("tnum");
    value.set_xalign(0.5);
    let sub = widgets::label("", "ring-sub");
    sub.set_xalign(0.5);
    sub.set_max_width_chars(10);
    sub.set_ellipsize(pango::EllipsizeMode::End);
    if size < 90 {
        value.add_css_class("small");
    }
    text.append(&value);
    text.append(&sub);
    root.add_overlay(&text);
    Ring { root, area, parts, warn, value, sub }
}

impl Ring {
    /// `frac` is 0..1 of the way round, in the accent (or danger, with `warn`).
    pub fn set(&self, frac: f64, value: &str, sub: &str, warn: bool) {
        self.warn.set(warn);
        self.set_parts(&[frac], value, sub);
    }

    /// Several parts one after another, in the stacked bars' colours.
    pub fn set_parts(&self, parts: &[f64], value: &str, sub: &str) {
        if *self.parts.borrow() != parts {
            *self.parts.borrow_mut() = parts.to_vec();
            self.area.queue_draw();
        }
        self.value.set_text(value);
        self.sub.set_text(sub);
        self.sub.set_visible(!sub.is_empty());
    }
}

// ---------- Legend ----------

/// A key: a small square in a part's colour (or the empty track's, for `None`).
pub fn key(part: Option<usize>) -> gtk::DrawingArea {
    let a = gtk::DrawingArea::new();
    a.set_content_width(10);
    a.set_content_height(10);
    a.set_valign(gtk::Align::Center);
    a.set_draw_func(move |_, cr, w, h| {
        rounded(cr, 0.0, 0.0, w as f64, h as f64, 3.0);
        match part {
            Some(i) => set(cr, part_rgb(i), 1.0),
            None => set(cr, track_rgb(), 0.14),
        }
        let _ = cr.fill();
    });
    a
}

/// Lines of "key  name ........ value", for the side of a ring or under a bar.
pub fn legend(items: &[(&str, Option<usize>)]) -> (gtk::Box, Vec<gtk::Label>) {
    let b = widgets::vbox(5);
    b.add_css_class("chart-legend");
    let mut values = Vec::new();
    for (name, part) in items {
        let row = widgets::hbox(8);
        if part.is_some() || items.iter().any(|(_, p)| p.is_some()) {
            row.append(&key(*part));
        }
        let n = widgets::label(name, "legend-label");
        n.set_hexpand(true);
        n.set_ellipsize(pango::EllipsizeMode::End);
        let v = widgets::label("–", "legend-value");
        v.add_css_class("tnum");
        v.set_xalign(1.0);
        row.append(&n);
        row.append(&v);
        b.append(&row);
        values.push(v);
    }
    (b, values)
}

// ---------- Stacked bar ----------

/// One bar split into parts, with a legend that wraps under it.
pub struct StackBar {
    pub root: gtk::Box,
    area: gtk::DrawingArea,
    parts: Rc<RefCell<(Vec<f64>, f64)>>,
    values: Vec<gtk::Label>,
}

/// `names` are the parts in order. With `free`, the rest of the bar is a named
/// part too ("Free"), shown in the track's colour.
pub fn stack_bar(names: &[&str], free: Option<&str>) -> StackBar {
    let root = widgets::vbox(10);
    let area = gtk::DrawingArea::new();
    area.set_content_height(14);
    area.set_hexpand(true);
    let parts: Rc<RefCell<(Vec<f64>, f64)>> = Rc::new(RefCell::new((Vec::new(), 1.0)));
    {
        let parts = parts.clone();
        area.set_draw_func(move |_, cr, w, h| {
            let (w, h) = (w as f64, h as f64);
            rounded(cr, 0.0, 0.0, w, h, h / 2.0);
            cr.clip_preserve();
            set(cr, track_rgb(), 0.08);
            let _ = cr.fill();
            let (values, total) = &*parts.borrow();
            if *total <= 0.0 {
                return;
            }
            let mut x = 0.0;
            for (i, v) in values.iter().enumerate() {
                let pw = (v / total).clamp(0.0, 1.0) * w;
                if pw <= 0.0 {
                    continue;
                }
                cr.rectangle(x, 0.0, pw.min(w - x), h);
                set(cr, part_rgb(i), 1.0);
                let _ = cr.fill();
                // A hairline of the card between parts keeps neighbours apart.
                x += pw;
                if x < w - 1.0 {
                    cr.rectangle(x - 0.5, 0.0, 1.0, h);
                    set(cr, Rgb::hex(&theme::palette().bg), 0.5);
                    let _ = cr.fill();
                }
            }
        });
    }
    root.append(&area);

    let flow = gtk::FlowBox::new();
    flow.set_selection_mode(gtk::SelectionMode::None);
    flow.set_column_spacing(18);
    flow.set_row_spacing(6);
    flow.set_max_children_per_line(8);
    flow.set_homogeneous(false);
    let mut values = Vec::new();
    let all: Vec<(&str, Option<usize>)> =
        names.iter().enumerate().map(|(i, n)| (*n, Some(i))).chain(free.map(|f| (f, None))).collect();
    for (name, part) in all {
        let item = widgets::hbox(6);
        item.append(&key(part));
        item.append(&widgets::label(name, "legend-label"));
        let v = widgets::label("–", "legend-value");
        v.add_css_class("tnum");
        item.append(&v);
        flow.append(&item);
        values.push(v);
    }
    let mut child = flow.first_child();
    while let Some(c) = child {
        c.set_focusable(false);
        child = c.next_sibling();
    }
    root.append(&flow);

    // Hover names the part under the pointer.
    let names: Vec<String> = names.iter().map(|s| s.to_string()).chain(free.map(String::from)).collect();
    let motion = gtk::EventControllerMotion::new();
    {
        let (parts, area2, values) = (parts.clone(), area.clone(), values.clone());
        motion.connect_motion(move |_, x, _| {
            let (vals, total) = &*parts.borrow();
            let w = area2.width().max(1) as f64;
            let mut at = 0.0;
            let mut hit = names.len().saturating_sub(1);
            for (i, v) in vals.iter().enumerate() {
                at += v / total.max(1e-9) * w;
                if x <= at {
                    hit = i;
                    break;
                }
            }
            let tip = names.get(hit).map(|n| format!("{n}: {}", values.get(hit).map(|l| l.text()).unwrap_or_default()));
            area2.set_tooltip_text(tip.as_deref());
        });
    }
    area.add_controller(motion);
    StackBar { root, area, parts, values }
}

impl StackBar {
    /// Parts in order, out of `total`; `texts` label each part (and "free" last).
    pub fn set(&self, parts: &[f64], total: f64, texts: &[String]) {
        *self.parts.borrow_mut() = (parts.to_vec(), total);
        self.area.queue_draw();
        for (l, t) in self.values.iter().zip(texts) {
            l.set_text(t);
        }
    }
}

// ---------- Ranked bars ----------

#[derive(Debug, Clone, Default)]
pub struct Bar {
    pub name: String,
    pub icon: String,
    /// 0..1, how long the bar is.
    pub frac: f64,
    pub value: String,
    pub tip: String,
    /// Clicking shows this process in Processes (0: not clickable).
    pub pid: i32,
    pub warn: bool,
}

struct BarRow {
    root: gtk::Widget,
    icon: gtk::Image,
    name: gtk::Label,
    frac: Rc<Cell<(f64, bool)>>,
    area: gtk::DrawingArea,
    value: gtk::Label,
    pid: Rc<Cell<i32>>,
}

/// Ranked rows that size to their contents, with five shown initially.
pub struct BarList {
    pub root: gtk::Box,
    rows: Rc<Vec<BarRow>>,
    empty: gtk::Label,
    more: gtk::Button,
    expanded: Rc<Cell<bool>>,
    count: Rc<Cell<usize>>,
}

pub fn bar_list(rows: usize, empty: &str, icons: bool) -> BarList {
    let root = widgets::vbox(0);
    root.add_css_class("bar-list");
    let list = widgets::vbox(0);
    let empty = widgets::label(empty, "dim");
    empty.add_css_class("ranking-empty");
    empty.set_wrap(true);
    root.append(&empty);
    root.append(&list);
    let mut out = Vec::new();
    for _ in 0..rows {
        let r = widgets::hbox(10);
        r.add_css_class("bar-row-content");
        let icon = gtk::Image::new();
        icon.set_pixel_size(16);
        icon.set_visible(icons);
        let name = widgets::label("", "bar-name");
        name.set_ellipsize(pango::EllipsizeMode::End);
        name.set_width_chars(18);
        name.set_max_width_chars(18);
        let frac = Rc::new(Cell::new((0.0f64, false)));
        let area = gtk::DrawingArea::new();
        area.set_content_height(5);
        area.set_hexpand(true);
        area.set_valign(gtk::Align::Center);
        {
            let frac = frac.clone();
            area.set_draw_func(move |_, cr, w, h| {
                let (w, h) = (w as f64, h as f64);
                rounded(cr, 0.0, 0.0, w, h, h / 2.0);
                set(cr, track_rgb(), 0.06);
                let _ = cr.fill();
                let (f, warn) = frac.get();
                let fw = (f.clamp(0.0, 1.0) * w).max(if f > 0.0 { h } else { 0.0 });
                if fw > 0.0 {
                    rounded(cr, 0.0, 0.0, fw, h, h / 2.0);
                    let p = theme::palette();
                    set(cr, if warn { Rgb::hex(&p.danger) } else { Rgb::hex(&p.accent) }, 1.0);
                    let _ = cr.fill();
                }
            });
        }
        let value = widgets::label("", "bar-value");
        value.add_css_class("tnum");
        value.set_width_chars(9);
        value.set_xalign(1.0);
        r.append(&icon);
        r.append(&name);
        r.append(&area);
        r.append(&value);
        let pid = Rc::new(Cell::new(0));
        let b = gtk::Button::new();
        b.add_css_class("flat");
        b.add_css_class("bar-row");
        b.set_child(Some(&r));
        b.set_focusable(false);
        let p = pid.clone();
        b.connect_clicked(move |_| {
            if p.get() > 0 {
                crate::sections::processes::reveal(p.get());
            }
        });
        b.set_visible(false);
        list.append(&b);
        out.push(BarRow { root: b.upcast(), icon, name, frac, area, value, pid });
    }
    let rows = Rc::new(out);
    let more = gtk::Button::with_label("Show more");
    more.add_css_class("flat");
    more.add_css_class("ranking-more");
    more.set_halign(gtk::Align::Start);
    more.set_visible(false);
    root.append(&more);
    let expanded = Rc::new(Cell::new(false));
    let count = Rc::new(Cell::new(0usize));
    {
        let (rows, expanded, count) = (rows.clone(), expanded.clone(), count.clone());
        more.connect_clicked(move |button| {
            expanded.set(!expanded.get());
            button.set_label(if expanded.get() { "Show less" } else { "Show more" });
            for (i, row) in rows.iter().enumerate() {
                row.root.set_visible(i < count.get() && (expanded.get() || i < 5));
            }
        });
    }
    BarList { root, rows, empty, more, expanded, count }
}

impl BarList {
    pub fn set(&self, bars: &[Bar]) {
        self.empty.set_visible(bars.is_empty());
        self.count.set(bars.len().min(self.rows.len()));
        self.more.set_visible(self.count.get() > 5);
        for (i, row) in self.rows.iter().enumerate() {
            match bars.get(i) {
                Some(b) => {
                    row.root.set_visible(self.expanded.get() || i < 5);
                    row.root.set_focusable(b.pid > 0);
                    row.root.set_can_target(b.pid > 0 || !b.tip.is_empty());
                    if b.pid > 0 {
                        row.root.add_css_class("clickable");
                    } else {
                        row.root.remove_css_class("clickable");
                    }
                    if row.name.text() != b.name {
                        row.name.set_text(&b.name);
                    }
                    if b.icon.is_empty() {
                        row.icon.set_icon_name(Some("application-x-executable-symbolic"));
                    } else if row.icon.icon_name().as_deref() != Some(b.icon.as_str()) {
                        row.icon.set_icon_name(Some(&b.icon));
                    }
                    row.value.set_text(&b.value);
                    if row.frac.get() != (b.frac, b.warn) {
                        row.frac.set((b.frac, b.warn));
                        row.area.queue_draw();
                    }
                    row.pid.set(b.pid);
                    let tip = if b.tip.is_empty() && b.pid > 0 {
                        format!("Show {} (PID {}) in Processes", b.name, b.pid)
                    } else {
                        b.tip.clone()
                    };
                    row.root.set_tooltip_text(if tip.is_empty() { None } else { Some(&tip) });
                }
                None => {
                    row.root.set_visible(false);
                    row.root.set_can_target(false);
                    row.pid.set(0);
                }
            }
        }
    }
}

// ---------- Cards ----------

/// A card: title on the left, a live summary on the right, then `body`.
pub fn card(title: &str, body: &impl IsA<gtk::Widget>) -> (gtk::Box, gtk::Label) {
    widgets::graph_card(title, body)
}

/// A compact secondary reading: caption, value and supporting detail.
pub struct Tile {
    pub root: gtk::Box,
    pub value: gtk::Label,
    pub sub: gtk::Label,
}

pub fn tile(title: &str) -> Tile {
    let root = widgets::vbox(2);
    root.add_css_class("graph-card");
    root.add_css_class("stat-card");
    root.add_css_class("metric-reading");
    root.append(&widgets::label(&title.to_uppercase(), "stat-title"));
    let value = widgets::label("–", "stat-value");
    value.add_css_class("tnum");
    value.set_ellipsize(pango::EllipsizeMode::End);
    root.append(&value);
    let sub = widgets::label("", "stat-sub");
    sub.set_ellipsize(pango::EllipsizeMode::End);
    sub.connect_notify_local(Some("label"), |label, _| {
        label.set_tooltip_text(Some(&label.text()));
    });
    root.append(&sub);
    Tile { root, value, sub }
}

/// A card with a ring on the left and legend lines beside it.
pub fn ring_card(title: &str, size: i32, items: &[(&str, Option<usize>)]) -> (gtk::Box, gtk::Label, Ring, Vec<gtk::Label>) {
    let body = widgets::hbox(12);
    let r = ring(size.min(88));
    body.append(&r.root);
    let (lg, values) = legend(items);
    lg.set_hexpand(true);
    lg.set_valign(gtk::Align::Center);
    body.append(&lg);
    let (c, summary) = card(title, &body);
    body.set_vexpand(true);
    body.set_valign(gtk::Align::Center);
    (c, summary, r, values)
}

const GAP: i32 = 12;

fn visible_children(w: &gtk::Widget) -> Vec<gtk::Widget> {
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

/// How many columns fit (at most `max`, 0 for no limit). Without `stretch`, only
/// counts that divide the cards evenly (4 → 2 → 1), so no line is left half empty;
/// with it, any count, and a short last line stretches across the width.
fn columns(n: usize, width: i32, min_width: i32, max: usize, stretch: bool) -> usize {
    if n == 0 {
        return 1;
    }
    let cap = if max > 0 { n.min(max) } else { n };
    if min_width <= 0 {
        return cap;
    }
    let fit = (((width + GAP) / (min_width + GAP)).max(1) as usize).min(cap);
    if stretch {
        return fit;
    }
    (1..=fit).rev().find(|c| n.is_multiple_of(*c)).unwrap_or(1)
}

/// Each line's cards with the width each gets.
fn lines(children: &[gtk::Widget], cols: usize, width: i32) -> Vec<(&[gtk::Widget], i32)> {
    children
        .chunks(cols)
        .map(|line| {
            let n = line.len() as i32;
            (line, ((width - GAP * (n - 1)) / n).max(1))
        })
        .collect()
}

/// Use the same column widths for measuring and allocating cards.
fn column_width(line: &[gtk::Widget], i: usize, width: i32, weighted: bool) -> i32 {
    if weighted && line.len() == 2 {
        let available = (width - GAP).max(2);
        let left_min = line[0].measure(gtk::Orientation::Horizontal, -1).0;
        let right_min = line[1].measure(gtk::Orientation::Horizontal, -1).0;
        let left = (available / 3).max(left_min).min((available - right_min).max(left_min));
        if i == 0 { left } else { (available - left).max(1) }
    } else {
        let n = line.len() as i32;
        let size = ((width - GAP * (n - 1)) / n).max(1);
        if i + 1 == line.len() { (width - (size + GAP) * i as i32).max(1) } else { size }
    }
}

fn line_heights(children: &[gtk::Widget], cols: usize, width: i32, weighted: bool) -> Vec<(i32, i32)> {
    lines(children, cols, width)
        .into_iter()
        .map(|(line, _)| {
            line.iter().enumerate().fold((0, 0), |(mn, nt), (i, c)| {
                let (m, n, _, _) = c.measure(gtk::Orientation::Vertical, column_width(line, i, width, weighted));
                (mn.max(m), nt.max(n))
            })
        })
        .collect()
}

mod imp {
    use super::{GAP, column_width, columns, line_heights, lines, visible_children};
    use gtk::glib;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use std::cell::Cell;

    #[derive(Default)]
    pub struct CardLayout {
        pub min_width: Cell<i32>,
        pub max_cols: Cell<usize>,
        pub stretch: Cell<bool>,
        pub weighted: Cell<bool>,
    }

    impl CardLayout {
        fn cols(&self, children: &[gtk::Widget], width: i32) -> usize {
            let requested = self.min_width.get();
            let min = if requested > 0 {
                children.iter().map(|c| c.measure(gtk::Orientation::Horizontal, -1).0).max().unwrap_or(0).max(requested)
            } else {
                0
            };
            columns(children.len(), width, min, self.max_cols.get(), self.stretch.get())
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CardLayout {
        const NAME: &'static str = "ActivityCardLayout";
        type Type = super::CardLayout;
        type ParentType = gtk::LayoutManager;
    }

    impl ObjectImpl for CardLayout {}

    impl LayoutManagerImpl for CardLayout {
        fn request_mode(&self, _: &gtk::Widget) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, w: &gtk::Widget, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let min_width = self.min_width.get();
            let kids = visible_children(w);
            let n = kids.len().max(1) as i32;
            if orientation == gtk::Orientation::Horizontal {
                let widest = kids.iter().map(|c| c.measure(orientation, -1).0).max().unwrap_or(0).max(min_width);
                let per_line = self.cols(&kids, i32::MAX / 4) as i32;
                let min = if min_width > 0 { widest } else { widest * n + GAP * (n - 1) };
                return (min, widest * per_line + GAP * (per_line - 1), -1, -1);
            }
            let width = if for_size > 0 { for_size } else { min_width.max(1) * n };
            let cols = self.cols(&kids, width);
            let lines = line_heights(&kids, cols, width, self.weighted.get());
            let gaps = GAP * (lines.len() as i32 - 1).max(0);
            let (m, nt) = lines.iter().fold((0, 0), |(a, b), (m, n)| (a + m, b + n));
            (m + gaps, nt + gaps, -1, -1)
        }

        fn allocate(&self, w: &gtk::Widget, width: i32, _: i32, _: i32) {
            let kids = visible_children(w);
            let cols = self.cols(&kids, width);
            let heights = line_heights(&kids, cols, width, self.weighted.get());
            let mut y = 0;
            for ((line, _), (_, h)) in lines(&kids, cols, width).into_iter().zip(heights) {
                let mut x = 0;
                for (i, c) in line.iter().enumerate() {
                    let cw = column_width(line, i, width, self.weighted.get());
                    c.size_allocate(&gtk::Allocation::new(x, y, cw, h), -1);
                    x += cw + GAP;
                }
                y += h + GAP;
            }
        }
    }
}

glib::wrapper! {
    /// Lays cards out in equal columns, each line as tall as its tallest card.
    pub struct CardLayout(ObjectSubclass<imp::CardLayout>) @extends gtk::LayoutManager;
}

/// Cards side by side, all the same width and as tall as the tallest, so a row
/// has no gaps. With `min_width`, cards stack (evenly) when they'd get narrower
/// than that; 0 keeps them on one line. Natural sizes don't widen the row, so
/// a long label can't push cards onto lines of their own.
pub fn row(cards: &[&gtk::Widget], min_width: i32) -> gtk::Box {
    let row = flow_row(cards, min_width, 0, false);
    if !cards.is_empty() && cards.iter().all(|c| c.has_css_class("metric-reading")) {
        row.add_css_class("metric-strip");
        row.add_css_class("graph-card");
    }
    row
}

/// Like `row`, but at most `max_cols` per line, wrapping at any count: a short
/// last line stretches its cards across the width instead of leaving a gap.
pub fn flow_row(cards: &[&gtk::Widget], min_width: i32, max_cols: usize, stretch: bool) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    row.add_css_class("card-row");
    let layout: CardLayout = glib::Object::new();
    {
        use gtk::subclass::prelude::ObjectSubclassIsExt;
        layout.imp().min_width.set(min_width);
        layout.imp().max_cols.set(max_cols);
        layout.imp().stretch.set(stretch);
    }
    row.set_layout_manager(Some(layout));
    // Cards stretch inside a line; the line itself doesn't take the page's spare height.
    row.set_vexpand(false);
    for c in cards {
        row.append(*c);
    }
    row
}

/// The width a chart card needs before cards stack (half of a narrow window's content).
pub const PAIR: i32 = 330;

/// Compact current usage beside a dominant history chart; stacks when narrow.
pub fn primary(summary: &gtk::Box, history: &gtk::Box) -> gtk::Box {
    let row = flow_row(&[summary.upcast_ref(), history.upcast_ref()], PAIR, 2, false);
    use gtk::subclass::prelude::ObjectSubclassIsExt;
    let layout = row.layout_manager().and_downcast::<CardLayout>().expect("card layout");
    layout.imp().weighted.set(true);
    summary.add_css_class("usage-summary");
    row
}
