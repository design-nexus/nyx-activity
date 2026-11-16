//! What the pages share: a head with the graphs' range, cards with an icon and a
//! title, notes that say what needs a look, and lists of apps with their tile.

use crate::charts::{self, Bar};
use crate::sampler::Snapshot;
use crate::sections::processes::{face, set_face};
use crate::sections::top::{self, By};
use crate::{fmt, live, prefs, theme, widgets};
use gtk::pango;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

// ---------- Page head ----------

pub struct Head {
    pub root: gtk::Box,
    pub title: gtk::Label,
    pub line: gtk::Label,
}

/// A page's own name and a line about it, with room at the right for a control.
pub fn head(title: &str, line: &str) -> Head {
    let root = widgets::hbox(12);
    root.add_css_class("page-head");
    let text = widgets::vbox(2);
    text.set_hexpand(true);
    let t = widgets::label(title, "page-head-title");
    t.set_xalign(0.0);
    t.set_ellipsize(pango::EllipsizeMode::End);
    let l = widgets::label(line, "page-head-line");
    l.add_css_class("tnum");
    l.set_xalign(0.0);
    l.set_ellipsize(pango::EllipsizeMode::End);
    text.append(&t);
    text.append(&l);
    root.append(&text);
    Head { root, title: t, line: l }
}

const RANGES: &[(u64, &str)] = &[(60, "1 min"), (300, "5 min"), (600, "10 min")];

/// How far back every graph reaches: the same setting as Settings' "Graphs show".
pub fn range() -> gtk::Box {
    let current = prefs::get().history_secs.to_string();
    let opts: Vec<(String, String)> = RANGES.iter().map(|(s, l)| (s.to_string(), l.to_string())).collect();
    let seg = widgets::segmented(&opts, &current, |v| {
        let secs = v.parse().unwrap_or(60);
        if prefs::get().history_secs != secs {
            prefs::update(|p| p.history_secs = secs);
            live::poke();
        }
    });
    seg.add_css_class("range");
    seg.set_valign(gtk::Align::Start);
    // Another page may have changed it.
    seg.connect_map(|seg| {
        let now = prefs::get().history_secs;
        let mut c = seg.first_child();
        for (secs, _) in RANGES {
            let Some(b) = c.clone().and_downcast::<gtk::ToggleButton>() else { break };
            if *secs == now && !b.is_active() {
                b.set_active(true);
            }
            c = b.next_sibling();
        }
    });
    seg
}

// ---------- Cards ----------

pub struct Card {
    pub root: gtk::Box,
    pub head: gtk::Box,
    pub meta: gtk::Label,
}

/// A card with an icon, a title and a dim note beside it; add to `head` for a control at its right.
pub fn card(icon: &str, title: &str) -> Card {
    let root = widgets::vbox(10);
    root.add_css_class("graph-card");
    root.add_css_class("page-card");
    let head = widgets::hbox(8);
    head.add_css_class("page-card-head");
    if !icon.is_empty() {
        let ic = gtk::Image::from_icon_name(icon);
        ic.add_css_class("page-card-icon");
        head.append(&ic);
    }
    let t = widgets::label(title, "graph-card-title");
    t.set_ellipsize(pango::EllipsizeMode::End);
    head.append(&t);
    let meta = widgets::label("", "page-card-meta");
    meta.add_css_class("tnum");
    meta.set_hexpand(true);
    meta.set_xalign(0.0);
    meta.set_ellipsize(pango::EllipsizeMode::End);
    head.append(&meta);
    root.append(&head);
    Card { root, head, meta }
}

/// Readings in a line, wrapping onto a second in a narrow window.
pub fn readings(tiles: &[&charts::Tile]) -> gtk::Box {
    let refs: Vec<&gtk::Widget> = tiles.iter().map(|t| t.root.upcast_ref()).collect();
    let row = charts::flow_row(&refs, 120, 0, true);
    row.add_css_class("metric-strip");
    row.add_css_class("graph-card");
    row
}

/// A bar's keys (`dashboard::key_row`) wrapping onto more lines when they don't fit.
pub fn wrap_keys(row: &gtk::Box) -> gtk::FlowBox {
    let flow = gtk::FlowBox::new();
    flow.set_selection_mode(gtk::SelectionMode::None);
    flow.set_column_spacing(14);
    flow.set_row_spacing(4);
    flow.set_max_children_per_line(8);
    flow.set_homogeneous(false);
    while let Some(item) = row.first_child() {
        row.remove(&item);
        flow.append(&item);
    }
    let mut c = flow.first_child();
    while let Some(x) = c {
        x.set_focusable(false);
        c = x.next_sibling();
    }
    flow.add_css_class("dash-keys");
    flow
}

/// A processor's name without the marks: "Intel Core Ultra 9 285H".
pub fn model_name(model: &str) -> String {
    model.replace("(R)", "").replace("(TM)", "").replace("(tm)", "").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cards side by side when there's room (two, or `max`), stacked in a narrow window.
pub fn columns(cards: &[&gtk::Widget], max: usize) -> gtk::Box {
    charts::flow_row(cards, charts::PAIR, max, true)
}

/// Like `columns`, stacking once a card would be narrower than `min`.
pub fn columns_at(cards: &[&gtk::Widget], min: i32, max: usize) -> gtk::Box {
    charts::flow_row(cards, min, max, true)
}

// ---------- Notes ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Bad,
    Warn,
    Plain,
}

#[derive(Clone)]
pub struct Note {
    pub root: gtk::Box,
    icon: gtk::Image,
    title: gtk::Label,
    detail: gtk::Label,
    pub action: gtk::Button,
}

/// A line that says what needs a look, with what to do about it.
pub fn note() -> Note {
    let root = widgets::hbox(12);
    root.add_css_class("dash-alert");
    root.add_css_class("page-note");
    let icon = gtk::Image::new();
    icon.add_css_class("dash-alert-icon");
    root.append(&icon);
    let text = widgets::vbox(1);
    text.set_hexpand(true);
    text.set_valign(gtk::Align::Center);
    let title = widgets::label("", "dash-alert-title");
    title.set_xalign(0.0);
    title.set_wrap(true);
    let detail = widgets::label("", "dash-alert-detail");
    detail.set_xalign(0.0);
    detail.set_wrap(true);
    text.append(&title);
    text.append(&detail);
    root.append(&text);
    let action = gtk::Button::new();
    action.add_css_class("dash-alert-action");
    action.set_valign(gtk::Align::Center);
    action.set_visible(false);
    root.append(&action);
    root.set_visible(false);
    Note { root, icon, title, detail, action }
}

impl Note {
    pub fn set(&self, icon: &str, tone: Tone, title: &str, detail: &str) {
        if self.icon.icon_name().as_deref() != Some(icon) {
            self.icon.set_icon_name(Some(icon));
        }
        for (class, on) in [("bad", tone == Tone::Bad), ("warn", tone == Tone::Warn), ("plain", tone == Tone::Plain)] {
            if on {
                self.root.add_css_class(class);
            } else {
                self.root.remove_css_class(class);
            }
        }
        if self.title.text() != title {
            self.title.set_text(title);
        }
        if self.detail.text() != detail {
            self.detail.set_text(detail);
        }
        self.detail.set_visible(!detail.is_empty());
        self.root.set_visible(true);
    }

    pub fn hide(&self) {
        self.root.set_visible(false);
    }

    /// The button at its end, labelled `label`.
    pub fn on_action(&self, label: &str, f: impl Fn() + 'static) {
        self.action.set_label(label);
        self.action.set_visible(true);
        self.action.connect_clicked(move |_| f());
    }
}

// ---------- Apps ----------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct App {
    pub name: String,
    pub icon: String,
    /// Smaller, after the name ("3.8 GB", "3 processes").
    pub sub: String,
    /// 0..1: how long its bar is (no bar when the list has none).
    pub frac: f64,
    pub value: String,
    /// Clicking shows it in Processes (0: not clickable).
    pub pid: i32,
    pub tip: String,
}

impl App {
    pub fn from_bar(b: Bar, sub: String) -> App {
        App { name: b.name, icon: b.icon, sub, frac: b.frac, value: b.value, pid: b.pid, tip: b.tip }
    }
}

struct AppRow {
    root: gtk::Button,
    face: gtk::Box,
    name: gtk::Label,
    bar: gtk::DrawingArea,
    fill: Rc<Cell<(f64, &'static str)>>,
    value: gtk::Label,
    pid: Rc<Cell<i32>>,
}

/// Apps one per row: their tile, name, a bar in the app's colour and a value.
pub struct AppList {
    pub root: gtk::Box,
    rows: Vec<AppRow>,
    empty: gtk::Label,
    shown: RefCell<Vec<App>>,
}

pub fn app_list(rows: usize, empty: &str, bars: bool) -> AppList {
    let root = widgets::vbox(0);
    root.add_css_class("app-list");
    let empty = widgets::label(empty, "dim");
    empty.add_css_class("app-list-empty");
    empty.set_xalign(0.0);
    empty.set_wrap(true);
    root.append(&empty);
    let mut out = Vec::new();
    for _ in 0..rows {
        let row = widgets::hbox(10);
        let f = face("app-row", 24);
        row.append(&f);
        let name = widgets::label("", "app-row-name");
        name.set_xalign(0.0);
        name.set_ellipsize(pango::EllipsizeMode::End);
        name.set_hexpand(!bars);
        if bars {
            name.set_width_chars(16);
            name.set_max_width_chars(22);
        }
        row.append(&name);
        let fill = Rc::new(Cell::new((0.0f64, "blue")));
        let bar = gtk::DrawingArea::new();
        bar.set_content_height(6);
        bar.set_hexpand(true);
        bar.set_valign(gtk::Align::Center);
        bar.set_visible(bars);
        {
            let fill = fill.clone();
            bar.set_draw_func(move |_, cr, w, h| {
                let (w, h) = (w as f64, h as f64);
                let t = theme::Rgb::hex(&theme::palette().text);
                rounded(cr, 0.0, 0.0, w, h, h / 2.0);
                cr.set_source_rgba(t.0, t.1, t.2, if theme::palette().light { 0.09 } else { 0.07 });
                let _ = cr.fill();
                let (f, swatch) = fill.get();
                let fw = f.clamp(0.0, 1.0) * w;
                if fw > 0.0 {
                    rounded(cr, 0.0, 0.0, fw.max(h), h, h / 2.0);
                    let c = charts::swatch(swatch);
                    cr.set_source_rgb(c.0, c.1, c.2);
                    let _ = cr.fill();
                }
            });
        }
        row.append(&bar);
        let value = widgets::label("", "app-row-value");
        value.add_css_class("tnum");
        value.set_xalign(1.0);
        value.set_width_chars(if bars { 8 } else { 6 });
        row.append(&value);
        let b = gtk::Button::new();
        b.add_css_class("flat");
        b.add_css_class("app-row");
        b.set_child(Some(&row));
        b.set_focusable(false);
        b.set_visible(false);
        let pid = Rc::new(Cell::new(0));
        {
            let pid = pid.clone();
            b.connect_clicked(move |_| {
                if pid.get() != 0 {
                    crate::sections::processes::reveal(pid.get());
                }
            });
        }
        root.append(&b);
        out.push(AppRow { root: b, face: f, name, bar, fill, value, pid });
    }
    AppList { root, rows: out, empty, shown: RefCell::default() }
}

impl AppList {
    pub fn set(&self, apps: &[App]) {
        if *self.shown.borrow() == apps {
            return;
        }
        *self.shown.borrow_mut() = apps.to_vec();
        self.empty.set_visible(apps.is_empty());
        let light = theme::palette().light;
        for (i, row) in self.rows.iter().enumerate() {
            let Some(a) = apps.get(i) else {
                row.root.set_visible(false);
                row.pid.set(0);
                continue;
            };
            row.root.set_visible(true);
            set_face(row.face.upcast_ref(), &a.icon, &a.name, light);
            let mut markup = glib_escape(&a.name);
            if !a.sub.is_empty() {
                markup.push_str(&format!(
                    "  <span size=\"smaller\" alpha=\"58%\" font_features=\"tnum\">{}</span>",
                    glib_escape(&a.sub)
                ));
            }
            if row.name.label() != markup {
                row.name.set_markup(&markup);
            }
            row.value.set_text(&a.value);
            let fill = (a.frac, crate::sections::dashboard::app_swatch(&a.name));
            if row.fill.get() != fill {
                row.fill.set(fill);
                row.bar.queue_draw();
            }
            row.pid.set(a.pid);
            if a.pid != 0 {
                row.root.add_css_class("clickable");
            } else {
                row.root.remove_css_class("clickable");
            }
            let tip = if a.tip.is_empty() && a.pid != 0 { format!("Show {} in Processes", a.name) } else { a.tip.clone() };
            row.root.set_tooltip_text(if tip.is_empty() { None } else { Some(&tip) });
        }
    }
}

fn glib_escape(s: &str) -> String {
    gtk::glib::markup_escape_text(s).to_string()
}

/// The busiest apps by `by`, with `sub` after each name.
pub fn busiest(snap: &Snapshot, by: By, n: usize, sub: impl Fn(&crate::sampler::groups::Group) -> String) -> Vec<App> {
    let bars = top::groups(snap, by, n, None);
    bars.into_iter()
        .map(|b| {
            let g = snap.groups.iter().find(|g| g.leader == b.pid);
            let s = g.map(&sub).unwrap_or_default();
            App::from_bar(b, s)
        })
        .collect()
}

/// "1 process", "3 processes".
pub fn processes(n: usize) -> String {
    if n == 1 { "1 process".into() } else { format!("{n} processes") }
}

/// An app's memory as the dim note beside its name.
pub fn memory_of(g: &crate::sampler::groups::Group) -> String {
    fmt::bytes(g.memory() as f64)
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
