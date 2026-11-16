//! The pieces beside the line graphs: theme swatches, ranked items, reading tiles,
//! and the rows that keep cards side by side at one size.

use crate::theme::{self, Rgb};
use crate::widgets;
use gtk::glib;
use gtk::pango;
use gtk::prelude::*;

/// A swatch of the theme's by name ("blue"), or the accent when the theme has no such swatch.
pub fn swatch(name: &str) -> Rgb {
    theme::swatches()
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, c)| Rgb::hex(&c))
        .unwrap_or_else(|| Rgb::hex(&theme::palette().accent))
}

/// A ranked item: an app or process, how big its share is, and what it shows.
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
fn column_width(line: &[gtk::Widget], i: usize, width: i32) -> i32 {
    let n = line.len() as i32;
    let size = ((width - GAP * (n - 1)) / n).max(1);
    if i + 1 == line.len() { (width - (size + GAP) * i as i32).max(1) } else { size }
}

fn line_heights(children: &[gtk::Widget], cols: usize, width: i32) -> Vec<(i32, i32)> {
    lines(children, cols, width)
        .into_iter()
        .map(|(line, _)| {
            line.iter().enumerate().fold((0, 0), |(mn, nt), (i, c)| {
                let (m, n, _, _) = c.measure(gtk::Orientation::Vertical, column_width(line, i, width));
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
            let lines = line_heights(&kids, cols, width);
            let gaps = GAP * (lines.len() as i32 - 1).max(0);
            let (m, nt) = lines.iter().fold((0, 0), |(a, b), (m, n)| (a + m, b + n));
            (m + gaps, nt + gaps, -1, -1)
        }

        fn allocate(&self, w: &gtk::Widget, width: i32, _: i32, _: i32) {
            let kids = visible_children(w);
            let cols = self.cols(&kids, width);
            let heights = line_heights(&kids, cols, width);
            let mut y = 0;
            for ((line, _), (_, h)) in lines(&kids, cols, width).into_iter().zip(heights) {
                let mut x = 0;
                for (i, c) in line.iter().enumerate() {
                    let cw = column_width(line, i, width);
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

/// Cards side by side, all the same width and as tall as the tallest, at most
/// `max_cols` per line (0: no limit). With `min_width`, cards wrap when they'd get
/// narrower than that; with `stretch`, a short last line spreads across the width.
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
