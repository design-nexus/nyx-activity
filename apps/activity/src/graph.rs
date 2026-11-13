//! Live line graphs drawn with cairo. Colours come only from the theme tokens:
//! the first series is `accent`, the second `mix(accent, danger, 0.55)` and
//! dashed (so the pair never relies on colour alone). Text stays in text tokens.

use crate::theme::{self, Rgb};
use crate::{live, prefs, widgets};
use gtk::cairo;
use gtk::prelude::*;
use std::cell::Cell;
use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Accent,
    Second,
    /// One of the theme's swatches by name ("blue"), for the Dashboard's cards.
    Swatch(&'static str),
}

impl Tone {
    pub fn rgb(self) -> Rgb {
        let p = theme::palette();
        let accent = Rgb::hex(&p.accent);
        match self {
            Tone::Accent => accent,
            Tone::Second => accent.mix(Rgb::hex(&p.danger), 0.55),
            Tone::Swatch(name) => crate::charts::swatch(name),
        }
    }

    fn class(self) -> &'static str {
        match self {
            Tone::Accent | Tone::Swatch(_) => "tone-accent",
            Tone::Second => "tone-second",
        }
    }

    /// How strongly the area under a line is filled.
    fn fill(self) -> f64 {
        if matches!(self, Tone::Swatch(_)) { 0.14 } else { 0.07 }
    }
}

#[derive(Clone)]
pub struct Series {
    pub key: String,
    pub label: String,
    pub tone: Tone,
    pub dashed: bool,
}

impl Series {
    pub fn new(key: impl Into<String>, label: &str, tone: Tone) -> Series {
        Series { key: key.into(), label: label.into(), tone, dashed: tone == Tone::Second }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Scale {
    /// 0–100.
    Percent,
    /// 0 to a rounded-up maximum of what's on screen, never below `floor`.
    Auto { floor: f64 },
}

fn nice_ceiling(v: f64) -> f64 {
    if v <= 0.0 {
        return 1.0;
    }
    let exp = 10f64.powf(v.log10().floor());
    let f = v / exp;
    let n = if f <= 1.0 {
        1.0
    } else if f <= 2.0 {
        2.0
    } else if f <= 2.5 {
        2.5
    } else if f <= 5.0 {
        5.0
    } else {
        10.0
    };
    n * exp
}

fn scale_max(scale: Scale, series: &[Vec<f64>]) -> f64 {
    match scale {
        Scale::Percent => 100.0,
        Scale::Auto { floor } => {
            let m = series.iter().flatten().copied().fold(0.0, f64::max);
            nice_ceiling((m * 1.1).max(floor))
        }
    }
}

fn set(cr: &cairo::Context, c: Rgb, a: f64) {
    cr.set_source_rgba(c.0, c.1, c.2, a);
}

/// Draw series right-aligned: the newest sample sits on the right edge.
fn draw_lines(cr: &cairo::Context, w: f64, h: f64, cap: usize, data: &[(Vec<f64>, Tone, bool)], max: f64, width: f64) {
    let step = w / (cap.max(2) - 1) as f64;
    let y = |v: f64| (h - 1.0) - (v / max).clamp(0.0, 1.0) * (h - 3.0);
    // Fills first, lines on top; the first series is drawn last so it stays in front.
    for (i, (values, tone, dashed)) in data.iter().enumerate().rev() {
        if values.len() < 2 {
            continue;
        }
        let n = values.len();
        let x0 = w - (n - 1) as f64 * step;
        let c = tone.rgb();
        if !*dashed && i == 0 {
            cr.move_to(x0, h);
            for (j, v) in values.iter().enumerate() {
                cr.line_to(x0 + j as f64 * step, y(*v));
            }
            cr.line_to(w, h);
            cr.close_path();
            set(cr, c, tone.fill());
            let _ = cr.fill();
        }
        for (j, v) in values.iter().enumerate() {
            let (px, py) = (x0 + j as f64 * step, y(*v));
            if j == 0 { cr.move_to(px, py) } else { cr.line_to(px, py) }
        }
        set(cr, c, 1.0);
        cr.set_line_width(width);
        cr.set_line_join(cairo::LineJoin::Round);
        cr.set_line_cap(cairo::LineCap::Round);
        if *dashed {
            cr.set_dash(&[5.0, 3.5], 0.0);
        }
        let _ = cr.stroke();
        cr.set_dash(&[], 0.0);
    }
}

/// Where the pointer sits in the history: samples back from now, and that sample's x.
fn hover_sample(w: f64, hx: f64, cap: usize) -> (usize, f64) {
    let step = w / (cap.max(2) - 1) as f64;
    let back = ((w - hx) / step).round().max(0.0) as usize;
    (back, w - back as f64 * step)
}

/// The hover readout: one line per series (marked solid or dashed when there
/// are several), then how long ago. None when there's no data that far back.
fn hover_lines(series: &[Series], raw: &[Vec<f64>], back: usize, fmt: fn(f64) -> String) -> Option<String> {
    let mut lines = Vec::new();
    for (s, values) in series.iter().zip(raw) {
        if back >= values.len() {
            continue;
        }
        let v = fmt(values[values.len() - 1 - back]);
        lines.push(if series.len() == 1 {
            v
        } else {
            let mark = if s.dashed { "┅" } else { "━" };
            format!("{mark} {}  {v}", s.label)
        });
    }
    if lines.is_empty() {
        return None;
    }
    let secs = back as f64 * prefs::get().interval_ms as f64 / 1000.0;
    lines.push(if back == 0 { "now".into() } else { format!("{} ago", crate::fmt::duration(secs)) });
    Some(lines.join("\n"))
}

/// The hover crosshair and a dot on each line.
fn draw_hover(cr: &cairo::Context, w: f64, h: f64, hx: f64, data: &[(Vec<f64>, Tone, bool)], max: f64, dot: f64) {
    let p = theme::palette();
    let (back, x) = hover_sample(w, hx, live::capacity());
    cr.move_to(x.round() + 0.5, 0.0);
    cr.line_to(x.round() + 0.5, h);
    set(cr, Rgb::hex(&p.text), 0.3);
    let _ = cr.stroke();
    for (values, tone, _) in data {
        if back >= values.len() {
            continue;
        }
        let v = values[values.len() - 1 - back];
        let y = (h - 1.0) - (v / max).clamp(0.0, 1.0) * (h - 3.0);
        cr.arc(x, y, dot + 1.5, 0.0, std::f64::consts::TAU);
        set(cr, Rgb::hex(&p.bg), 1.0);
        let _ = cr.fill();
        cr.arc(x, y, dot, 0.0, std::f64::consts::TAU);
        set(cr, tone.rgb(), 1.0);
        let _ = cr.fill();
    }
}

/// A small graph with no chrome, for tiles and per-core grids. Hovering shows
/// a crosshair, and the value (through `fmt`) as a tooltip.
pub fn sparkline(series: Vec<Series>, scale: Scale, height: i32, fmt: fn(f64) -> String) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_height(height);
    area.set_hexpand(true);
    area.add_css_class("sparkline");
    let series = Rc::new(series);
    let hover: Rc<Cell<Option<f64>>> = Rc::new(Cell::new(None));
    {
        let (series, hover) = (series.clone(), hover.clone());
        area.set_draw_func(move |_, cr, w, h| {
            let data: Vec<(Vec<f64>, Tone, bool)> = series.iter().map(|s| (live::history(&s.key), s.tone, s.dashed)).collect();
            let values: Vec<Vec<f64>> = data.iter().map(|d| d.0.clone()).collect();
            let max = scale_max(scale, &values);
            draw_lines(cr, w as f64, h as f64, live::capacity(), &data, max, 1.5);
            if let Some(hx) = hover.get() {
                draw_hover(cr, w as f64, h as f64, hx, &data, max, 2.5);
            }
        });
    }
    let tip: Rc<dyn Fn()> = {
        let (series, hover, area) = (series.clone(), hover.clone(), area.clone());
        Rc::new(move || {
            let text = hover.get().filter(|_| area.width() > 0).and_then(|hx| {
                let raw: Vec<Vec<f64>> = series.iter().map(|s| live::history(&s.key)).collect();
                let (back, _) = hover_sample(area.width() as f64, hx, live::capacity());
                hover_lines(&series, &raw, back, fmt)
            });
            area.set_tooltip_text(text.as_deref());
        })
    };
    let motion = gtk::EventControllerMotion::new();
    let (h2, a2, t2) = (hover.clone(), area.clone(), tip.clone());
    motion.connect_motion(move |_, x, _| {
        h2.set(Some(x));
        t2();
        a2.queue_draw();
    });
    let (h2, a2, t2) = (hover.clone(), area.clone(), tip.clone());
    motion.connect_leave(move |_| {
        h2.set(None);
        t2();
        a2.queue_draw();
    });
    area.add_controller(motion);
    let a = area.clone();
    live::on_tick(&area, move |_| {
        tip();
        a.queue_draw();
    });
    area
}

pub struct Graph {
    pub root: gtk::Box,
}

/// A full graph: legend with live values, the plot with a grid and a hover
/// crosshair, and a time axis.
pub fn graph(series: Vec<Series>, scale: Scale, fmt: fn(f64) -> String, height: i32) -> Graph {
    let root = widgets::vbox(6);
    root.add_css_class("graph");

    // Legend: a single series needs none (the card title names it), but its value still shows.
    let legend = widgets::hbox(16);
    legend.add_css_class("graph-legend");
    let mut values = Vec::new();
    for s in &series {
        let item = widgets::hbox(6);
        if series.len() > 1 {
            let sw = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            sw.add_css_class("legend-swatch");
            sw.add_css_class(s.tone.class());
            if s.dashed {
                sw.add_css_class("dashed");
            }
            sw.set_valign(gtk::Align::Center);
            item.append(&sw);
            item.append(&widgets::label(&s.label, "legend-label"));
        }
        let v = widgets::label("–", "legend-value");
        v.add_css_class("tnum");
        item.append(&v);
        values.push(v);
        legend.append(&item);
    }
    // One series: its card header already shows the value.
    legend.set_visible(series.len() > 1);
    root.append(&legend);

    let area = gtk::DrawingArea::new();
    area.set_content_height(height);
    area.set_hexpand(true);
    let hover: Rc<Cell<Option<f64>>> = Rc::new(Cell::new(None));

    let overlay = gtk::Overlay::new();
    overlay.add_css_class("graph-plot");
    overlay.set_child(Some(&area));
    let max_label = widgets::label("", "graph-axis");
    max_label.add_css_class("tnum");
    max_label.set_halign(gtk::Align::Start);
    max_label.set_valign(gtk::Align::Start);
    max_label.set_can_target(false);
    overlay.add_overlay(&max_label);
    let tip = widgets::label("", "graph-tip");
    tip.add_css_class("tnum");
    tip.set_halign(gtk::Align::Start);
    tip.set_valign(gtk::Align::Start);
    tip.set_visible(false);
    tip.set_can_target(false);
    overlay.add_overlay(&tip);
    root.append(&overlay);

    let axis = widgets::hbox(0);
    let ago = widgets::label("", "graph-axis");
    ago.set_hexpand(true);
    axis.append(&ago);
    axis.append(&widgets::label("now", "graph-axis"));
    root.append(&axis);

    let series = Rc::new(series);
    let max = Rc::new(Cell::new(1.0f64));

    // Labels are updated here, outside drawing, so the plot never relayouts mid-frame.
    let update: Rc<dyn Fn()> = {
        let series = series.clone();
        let hover = hover.clone();
        let max = max.clone();
        let area = area.clone();
        Rc::new(move || {
            let raw: Vec<Vec<f64>> = series.iter().map(|s| live::history(&s.key)).collect();
            let m = scale_max(scale, &raw);
            max.set(m);
            max_label.set_text(&fmt(m));
            let w = area.width() as f64;
            let cap = live::capacity();
            let Some(hx) = hover.get().filter(|_| w > 0.0) else {
                tip.set_visible(false);
                return;
            };
            let (back, x) = hover_sample(w, hx, cap);
            let Some(text) = hover_lines(&series, &raw, back, fmt) else {
                tip.set_visible(false);
                return;
            };
            tip.set_text(&text);
            tip.set_visible(true);
            let tw = tip.width().max(90) as f64;
            let left = if x + 12.0 + tw > w { x - 12.0 - tw } else { x + 12.0 };
            tip.set_margin_start(left.max(0.0) as i32);
            tip.set_margin_top(6);
        })
    };

    {
        let series = series.clone();
        let hover = hover.clone();
        let max = max.clone();
        area.set_draw_func(move |_, cr, w, h| {
            let (w, h) = (w as f64, h as f64);
            let p = theme::palette();
            let border = Rgb::hex(&p.border);
            cr.set_line_width(1.0);
            for i in 1..4 {
                let gy = (h * i as f64 / 4.0).round() + 0.5;
                cr.move_to(0.0, gy);
                cr.line_to(w, gy);
            }
            set(cr, border, 0.45);
            let _ = cr.stroke();
            cr.move_to(0.0, h - 0.5);
            cr.line_to(w, h - 0.5);
            set(cr, border, 0.8);
            let _ = cr.stroke();

            let data: Vec<(Vec<f64>, Tone, bool)> = series.iter().map(|s| (live::history(&s.key), s.tone, s.dashed)).collect();
            let max = max.get();
            let cap = live::capacity();
            draw_lines(cr, w, h, cap, &data, max, 2.0);

            if let Some(hx) = hover.get() {
                draw_hover(cr, w, h, hx, &data, max, 3.0);
            }
        });
    }
    let motion = gtk::EventControllerMotion::new();
    {
        let hover = hover.clone();
        let a = area.clone();
        let update = update.clone();
        motion.connect_motion(move |_, x, _| {
            hover.set(Some(x));
            update();
            a.queue_draw();
        });
    }
    {
        let hover = hover.clone();
        let a = area.clone();
        let update = update.clone();
        motion.connect_leave(move |_| {
            hover.set(None);
            update();
            a.queue_draw();
        });
    }
    area.add_controller(motion);

    // Right-click: save what this graph shows.
    let menu = gtk::Popover::new();
    menu.set_has_arrow(false);
    let export = gtk::Button::with_label("Export this graph (CSV)");
    export.add_css_class("flat");
    menu.set_child(Some(&export));
    menu.set_parent(&area);
    let m2 = menu.clone();
    area.connect_destroy(move |_| m2.unparent());
    {
        let (series, menu) = (series.clone(), menu.clone());
        export.connect_clicked(move |_| {
            menu.popdown();
            let keys: Vec<String> = series.iter().map(|s| s.key.clone()).collect();
            let name = keys.first().map_or("graph".to_string(), |k| k.replace('.', "-"));
            crate::actions::export_csv(&name, Some(keys));
        });
    }
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_SECONDARY);
    click.connect_pressed(move |_, _, x, y| {
        menu.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        menu.popup();
    });
    area.add_controller(click);

    {
        let a = area.clone();
        let values = values.clone();
        let series = series.clone();
        live::on_tick(&area, move |_| {
            for (label, s) in values.iter().zip(series.iter()) {
                if let Some(v) = live::history(&s.key).last() {
                    label.set_text(&fmt(*v));
                }
            }
            let secs = prefs::get().history_secs as f64;
            ago.set_text(&format!("{} ago", crate::fmt::duration(secs)));
            update();
            a.queue_draw();
        });
    }
    Graph { root }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_scale_up() {
        assert_eq!(nice_ceiling(0.7), 1.0);
        assert_eq!(nice_ceiling(3.0), 5.0);
        assert_eq!(nice_ceiling(2.2), 2.5);
        assert_eq!(nice_ceiling(1200.0), 2000.0);
    }
}
