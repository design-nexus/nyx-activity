//! Ctrl+K: one box to find a page, a process, a service or a startup item and
//! go to it. Enter opens the highlighted result; Alt+Enter ends a process.

use crate::startup::{self, Unit};
use crate::widgets;
use crate::{actions, cmd, live, prefs, sections, window};
use gtk::prelude::*;
use gtk::{gdk, glib, pango};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Most results shown at once.
const LIMIT: usize = 50;

#[derive(Clone)]
enum Target {
    Page(&'static str),
    Process(i32),
    Service(Unit, bool),
    Startup,
}

#[derive(Clone)]
struct Item {
    title: String,
    /// Searched too, shown under the title.
    detail: String,
    kind: &'static str,
    icon: String,
    target: Target,
}

fn static_items() -> Vec<Item> {
    let mut items: Vec<Item> = sections::all()
        .into_iter()
        .map(|s| Item {
            title: s.title.to_string(),
            detail: format!("{} {}", s.description, s.keywords),
            kind: "Page",
            icon: s.icon.to_string(),
            target: Target::Page(s.id),
        })
        .collect();
    if let Some(snap) = live::latest()
        && let Some(procs) = &snap.procs
    {
        let titles: std::collections::HashMap<i32, &str> = snap.windows.iter().map(|w| (w.pid, w.title.as_str())).collect();
        for p in procs.iter().filter(|p| !p.kernel) {
            let title = titles.get(&p.pid).map(|t| format!("{} — {t}", p.name)).unwrap_or_else(|| p.name.clone());
            items.push(Item {
                title,
                detail: format!("PID {} · {}", p.pid, p.cmdline),
                kind: "Process",
                icon: "utilities-system-monitor-symbolic".into(),
                target: Target::Process(p.pid),
            });
        }
    }
    for m in startup::load().hyprland {
        items.push(Item {
            title: m.name,
            detail: m.command,
            kind: "Startup",
            icon: "system-reboot-symbolic".into(),
            target: Target::Startup,
        });
    }
    for x in startup::xdg_entries() {
        items.push(Item {
            title: x.name,
            detail: x.comment,
            kind: "Startup",
            icon: "system-reboot-symbolic".into(),
            target: Target::Startup,
        });
    }
    items
}

/// Lower is better: name starts with the query, name contains it, details do.
fn score(item: &Item, q: &str) -> Option<u8> {
    let title = item.title.to_lowercase();
    if title.starts_with(q) {
        Some(0)
    } else if title.contains(q) {
        Some(1)
    } else if item.detail.to_lowercase().contains(q) {
        Some(2)
    } else {
        None
    }
}

fn rank(items: &[Item], query: &str) -> Vec<Item> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        // Before typing: just the pages.
        return items.iter().filter(|i| matches!(i.target, Target::Page(_))).cloned().collect();
    }
    let kind_order = |i: &Item| match i.target {
        Target::Page(_) => 0,
        Target::Process(_) => 1,
        Target::Service(..) => 2,
        Target::Startup => 3,
    };
    let mut hits: Vec<(u8, &Item)> = items.iter().filter_map(|i| score(i, &q).map(|s| (s, i))).collect();
    hits.sort_by(|a, b| a.0.cmp(&b.0).then(kind_order(a.1).cmp(&kind_order(b.1))).then(a.1.title.cmp(&b.1.title)));
    hits.into_iter().take(LIMIT).map(|(_, i)| i.clone()).collect()
}

fn row(item: &Item) -> gtk::ListBoxRow {
    let b = widgets::hbox(10);
    b.add_css_class("palette-row");
    let icon = gtk::Image::from_icon_name(&item.icon);
    b.append(&icon);
    let text = widgets::vbox(0);
    text.set_hexpand(true);
    let t = widgets::label(&item.title, "");
    t.set_ellipsize(pango::EllipsizeMode::End);
    text.append(&t);
    if !item.detail.is_empty() && !matches!(item.target, Target::Page(_)) {
        let d = widgets::label(&item.detail, "dim");
        d.add_css_class("palette-detail");
        d.set_ellipsize(pango::EllipsizeMode::End);
        text.append(&d);
    }
    b.append(&text);
    b.append(&widgets::tag(item.kind, ""));
    let r = gtk::ListBoxRow::new();
    r.set_child(Some(&b));
    r
}

pub fn open() {
    let (dialog, card) = widgets::dialog("Go to", 620);
    dialog.set_default_height(520);
    card.add_css_class("palette");
    let entry = gtk::SearchEntry::new();
    entry.set_placeholder_text(Some("Pages, processes, services, startup items"));
    card.append(&entry);
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.add_css_class("palette-list");
    let scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&list).build();
    card.append(&scroll);
    let hint = widgets::label("Enter opens · Alt+Enter ends a process · Esc closes", "dim");
    hint.add_css_class("palette-hint");
    card.append(&hint);

    let items = Rc::new(RefCell::new(static_items()));
    let shown: Rc<RefCell<Vec<Item>>> = Rc::default();

    let refresh = {
        let (items, shown, list, entry) = (items.clone(), shown.clone(), list.clone(), entry.clone());
        move || {
            while let Some(c) = list.first_child() {
                list.remove(&c);
            }
            let ranked = rank(&items.borrow(), &entry.text());
            for i in &ranked {
                list.append(&row(i));
            }
            if ranked.is_empty() {
                let none = gtk::ListBoxRow::new();
                none.set_child(Some(&widgets::label("Nothing found.", "empty-state")));
                none.set_selectable(false);
                none.set_activatable(false);
                list.append(&none);
            }
            *shown.borrow_mut() = ranked;
            if let Some(first) = list.row_at_index(0) {
                list.select_row(Some(&first));
            }
        }
    };
    let refresh: Rc<dyn Fn()> = Rc::new(refresh);
    refresh();

    // Services take a moment to list; they join the results when ready.
    let (items2, refresh2) = (items.clone(), refresh.clone());
    cmd::background(
        || {
            let mut out = Vec::new();
            for user in [true, false] {
                for u in startup::units(user) {
                    out.push(Item {
                        title: startup::unescape(u.name.trim_end_matches(".service")),
                        detail: u.description.clone(),
                        kind: if user { "Service" } else { "System service" },
                        icon: "emblem-system-symbolic".into(),
                        target: Target::Service(u, user),
                    });
                }
            }
            out
        },
        move |services: Vec<Item>| {
            items2.borrow_mut().extend(services);
            refresh2();
        },
    );

    let go = {
        let (shown, dialog) = (shown.clone(), dialog.clone());
        move |index: usize| {
            let Some(item) = shown.borrow().get(index).cloned() else { return };
            dialog.close();
            match item.target {
                Target::Page(id) => window::navigate(id),
                Target::Process(pid) => sections::processes::reveal(pid),
                Target::Service(unit, user) => sections::services::open_unit(unit, user),
                Target::Startup => window::navigate("startup"),
            }
        }
    };
    let go: Rc<dyn Fn(usize)> = Rc::new(go);
    let g = go.clone();
    list.connect_row_activated(move |_, r| g(r.index() as usize));
    let r = refresh.clone();
    entry.connect_search_changed(move |_| r());
    let (g, l) = (go.clone(), list.clone());
    entry.connect_activate(move |_| g(l.selected_row().map_or(0, |r| r.index() as usize)));

    let armed = Rc::new(Cell::new(None::<(i32, std::time::Instant)>));
    let keys = gtk::EventControllerKey::new();
    let (l, shown2, d, sc) = (list.clone(), shown.clone(), dialog.clone(), scroll.clone());
    keys.connect_key_pressed(move |_, key, _, mods| {
        let index = l.selected_row().map_or(0, |r| r.index());
        match key {
            gdk::Key::Down | gdk::Key::Up => {
                let next = if key == gdk::Key::Down { index + 1 } else { (index - 1).max(0) };
                // Focus stays in the search box so typing carries on; scroll by hand.
                if let Some(r) = l.row_at_index(next) {
                    l.select_row(Some(&r));
                    if let Some(p) = r.compute_point(&l, &gtk::graphene::Point::new(0.0, 0.0)) {
                        let adj = sc.vadjustment();
                        let (top, h) = (p.y() as f64, r.height() as f64);
                        if top < adj.value() {
                            adj.set_value(top);
                        } else if top + h > adj.value() + adj.page_size() {
                            adj.set_value(top + h - adj.page_size());
                        }
                    }
                }
                glib::Propagation::Stop
            }
            gdk::Key::Return | gdk::Key::KP_Enter if mods.contains(gdk::ModifierType::ALT_MASK) => {
                let Some(Item { target: Target::Process(pid), title, .. }) = shown2.borrow().get(index as usize).cloned() else {
                    return glib::Propagation::Stop;
                };
                let ready = matches!(armed.get(), Some((p, at)) if p == pid && at.elapsed().as_secs_f64() < 3.0);
                if !prefs::get().confirm_kill || ready {
                    armed.set(None);
                    actions::signal(&[(pid, title)], actions::Signal::Term);
                    d.close();
                } else {
                    armed.set(Some((pid, std::time::Instant::now())));
                    window::toast(&format!("Press Alt+Enter again to end {title}"));
                }
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    dialog.add_controller(keys);
    dialog.present();
    entry.grab_focus();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, detail: &str, target: Target) -> Item {
        Item { title: title.into(), detail: detail.into(), kind: "", icon: String::new(), target }
    }

    #[test]
    fn ranks_name_matches_first() {
        let items = vec![
            item("Network", "traffic firefox", Target::Page("network")),
            item("my-firefox-helper", "", Target::Process(2)),
            item("firefox", "", Target::Process(1)),
            item("Dashboard", "", Target::Page("overview")),
        ];
        let got: Vec<String> = rank(&items, "Firefox").into_iter().map(|i| i.title).collect();
        assert_eq!(got, ["firefox", "my-firefox-helper", "Network"]);
        // Empty query: pages only.
        assert_eq!(rank(&items, " ").len(), 2);
    }
}
