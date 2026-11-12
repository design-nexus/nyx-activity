//! The main window: the header (the app and the page, the sidebar toggle; what's
//! running, the live/paused pill, the page's config files, search and settings at the
//! end), the navigation sidebar and a stack of section pages built the first time
//! they're shown. Settings opens as a dialog over the window (see `settings_dialog`).

use crate::sections::{self, Section};
use crate::widgets;
use crate::{live, prefs, settings_dialog, theme};
use gtk::prelude::*;
use gtk::{gdk, glib};
use nyx_ui::chrome;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

struct Ui {
    window: gtk::ApplicationWindow,
    stack: gtk::Stack,
    nav_items: HashMap<&'static str, gtk::Button>,
    pages: HashMap<&'static str, gtk::ScrolledWindow>,
    sections: Vec<Section>,
    current: &'static str,
    /// The one toast, reused so quick messages replace each other.
    toast: gtk::Label,
    toast_timer: Option<glib::SourceId>,
    overlay: gtk::Overlay,
    /// Holds the current page's "open the config file" button.
    edit: gtk::Box,
    header: chrome::AppHeader,
}

thread_local! {
    static UI: RefCell<Option<Rc<RefCell<Ui>>>> = const { RefCell::new(None) };
}

fn ui() -> Option<Rc<RefCell<Ui>>> {
    UI.with(|u| u.borrow().clone())
}

pub fn present(app: &gtk::Application, section: Option<&str>) {
    if let Some(ui) = ui() {
        let window = ui.borrow().window.clone();
        if let Some(s) = section {
            navigate(s);
        }
        window.present();
        return;
    }
    theme::install();
    install_icons();
    live::start();
    crate::alerts::start();
    build(app);
    navigate(section.unwrap_or("overview"));
    if let Some(ui) = ui() {
        ui.borrow().window.present();
    }
}

/// Our own symbolic icons (the icon theme may lack a CPU or memory glyph).
/// They're written to the cache once and added to the icon search path.
fn install_icons() {
    const ICONS: &[(&str, &str)] = &[
        ("activity-cpu-symbolic.svg", include_str!("../data/icons/activity-cpu-symbolic.svg")),
        ("activity-memory-symbolic.svg", include_str!("../data/icons/activity-memory-symbolic.svg")),
        ("activity-gpu-symbolic.svg", include_str!("../data/icons/activity-gpu-symbolic.svg")),
        ("activity-thermometer-symbolic.svg", include_str!("../data/icons/activity-thermometer-symbolic.svg")),
    ];
    let dir = crate::paths::cache_dir().join("icons");
    for (name, svg) in ICONS {
        let path = dir.join(name);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(*svg) {
            let _ = crate::cmd::atomic_write(&path, svg);
        }
    }
    if let Some(display) = gdk::Display::default() {
        gtk::IconTheme::for_display(&display).add_search_path(&dir);
    }
}

fn build(app: &gtk::Application) {
    let window =
        gtk::ApplicationWindow::builder().application(app).title("Activity").default_width(1280).default_height(800).build();
    window.add_css_class("activity-window");
    // No client-side titlebar: Hyprland manages the window.
    window.set_titlebar(Some(&gtk::Box::new(gtk::Orientation::Horizontal, 0)));
    nyx_ui::keys::install(
        &window,
        nyx_ui::keys::Common {
            settings: Some(settings_dialog::open),
            shortcuts: Some(show_shortcuts),
            close: false,
            palette: Some(crate::palette::open),
        },
    );
    window.set_icon_name(Some("io.github.design_nexus.nyx.Activity"));

    let sections = sections::all();

    // ----- Sidebar -----
    let nav = gtk::Box::new(gtk::Orientation::Vertical, 0);
    nav.add_css_class("settings-navigation");
    // Labels inside expand; don't let that widen the sidebar itself.
    nav.set_hexpand(false);
    let mut compact_hide: Vec<gtk::Widget> = Vec::new();

    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let mut nav_items = HashMap::new();
    let mut last_group = "";
    // Settings opens as a dialog from the top bar, so it has no nav item.
    for s in sections.iter().filter(|s| s.id != "settings") {
        if s.group != last_group {
            let g = widgets::label(&s.group.to_uppercase(), "nav-group");
            if last_group.is_empty() {
                g.add_css_class("first");
            }
            compact_hide.push(g.clone().upcast());
            list.append(&g);
            last_group = s.group;
        }
        let button = gtk::Button::new();
        button.add_css_class("nav-item");
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        content.append(&gtk::Image::from_icon_name(s.icon));
        let l = widgets::label(s.title, "nav-label");
        l.set_hexpand(true);
        compact_hide.push(l.clone().upcast());
        content.append(&l);
        if let Some(readout) = s.readout {
            let r = widgets::label("", "nav-readout");
            r.add_css_class("tnum");
            // A long reading ("↓ 12.3 MiB/s") mustn't widen the sidebar.
            r.set_max_width_chars(11);
            r.set_ellipsize(gtk::pango::EllipsizeMode::End);
            compact_hide.push(r.clone().upcast());
            content.append(&r);
            let r2 = r.clone();
            live::on_tick(&r, move |snap| r2.set_text(&readout(snap)));
        }
        button.set_child(Some(&content));
        button.set_tooltip_text(Some(s.description));
        let id = s.id;
        button.connect_clicked(move |_| navigate(id));
        list.append(&button);
        nav_items.insert(s.id, button);
    }
    let nav_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        // Scrolls with wheel, trackpad and keyboard; no visible scrollbar.
        .vscrollbar_policy(gtk::PolicyType::External)
        .vexpand(true)
        .child(&list)
        .build();
    nav.append(&nav_scroll);

    // ----- Content -----
    let stack = gtk::Stack::new();
    stack.add_css_class("settings-content");
    stack.set_hexpand(true);
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(if prefs::get().reduce_motion { 0 } else { 160 });

    // ----- Header -----
    // The app and the page, the sidebar toggle; what's running, the live/paused pill,
    // the page's config files, search and Settings at the end.
    let header = chrome::app_header(&window, "utilities-system-monitor-symbolic", "Activity");
    let collapse = widgets::bar_button("sidebar-show-symbolic", "Collapse the sidebar (Ctrl+B)");
    collapse.connect_clicked(|_| toggle_sidebar());
    header.start.append(&collapse);
    header.end.set_spacing(4);
    header.end.append(&pause_button());
    // The page's config files, beside search and settings.
    let edit = widgets::hbox(0);
    header.end.append(&edit);
    let search = widgets::bar_button("system-search-symbolic", "Filter processes, or go to anything (Ctrl+F)");
    search.connect_clicked(|_| find());
    header.end.append(&search);
    header.end.append(&chrome::settings_button(settings_dialog::open));

    let frame = nyx_ui::frame::AppFrame::new(&header.root);
    frame.set_sidebar(&nav);
    frame.set_page(&stack);
    let overlay = frame.install(&window);
    let toast = gtk::Label::new(None);
    toast.set_wrap(true);
    toast.set_max_width_chars(70);
    let toast_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    toast_box.add_css_class("toast");
    toast_box.append(&toast);
    toast_box.set_halign(gtk::Align::Center);
    toast_box.set_valign(gtk::Align::End);
    toast_box.set_can_target(false);
    toast_box.set_visible(false);
    overlay.add_overlay(&toast_box);

    // ----- Keys -----
    let keys = gtk::EventControllerKey::new();
    let w2 = window.clone();
    keys.connect_key_pressed(move |_, key, _, mods| {
        let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
        if settings_dialog::is_open() {
            return match key {
                gdk::Key::Escape => {
                    settings_dialog::escape();
                    glib::Propagation::Stop
                }
                gdk::Key::f if ctrl => {
                    settings_dialog::focus_search();
                    glib::Propagation::Stop
                }
                gdk::Key::q | gdk::Key::w if ctrl => {
                    w2.close();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            };
        }
        match key {
            gdk::Key::f if ctrl => {
                find();
                glib::Propagation::Stop
            }
            gdk::Key::F1 => {
                show_shortcuts();
                glib::Propagation::Stop
            }
            gdk::Key::k if ctrl => {
                crate::palette::open();
                glib::Propagation::Stop
            }
            gdk::Key::b if ctrl => {
                toggle_sidebar();
                glib::Propagation::Stop
            }
            gdk::Key::q | gdk::Key::w if ctrl => {
                w2.close();
                glib::Propagation::Stop
            }
            gdk::Key::p if ctrl => {
                toggle_pause();
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    window.add_controller(keys);

    // Narrow windows (a tiled half-screen) get an icon-only sidebar.
    SIDEBAR.with(|s| *s.borrow_mut() = Some(Sidebar { nav: nav.clone(), hide: compact_hide }));
    let apply_width = {
        let nav = nav.clone();
        move |w: &gtk::ApplicationWindow| {
            let width = if w.width() > 0 { w.width() } else { w.default_width() };
            let narrow = width > 0 && width < 980;
            settings_dialog::fit(w);
            if narrow == NARROW.with(|n| n.get()) && nav.has_css_class("sized") {
                return;
            }
            nav.add_css_class("sized");
            set_narrow(narrow);
            apply_compact(narrow || prefs::get().sidebar_collapsed);
        }
    };
    let aw = apply_width.clone();
    window.connect_default_width_notify(move |w| aw(w));
    let aw = apply_width.clone();
    window.connect_realize(move |w| aw(w));
    // Tiled windows are resized by the compositor without touching the default
    // size: an invisible layer over the whole window reports each real size
    // change, and the layout follows on the next frame.
    let probe = gtk::DrawingArea::new();
    probe.set_can_target(false);
    probe.set_can_focus(false);
    overlay.add_overlay(&probe);
    overlay.set_measure_overlay(&probe, false);
    {
        let (aw, w2) = (apply_width.clone(), window.clone());
        probe.connect_resize(move |_, _, _| {
            let (aw, w2) = (aw.clone(), w2.clone());
            // After this layout pass, when the window's width is the new one.
            glib::idle_add_local_once(move || aw(&w2));
        });
    }
    // A slow fallback, in case a resize slips by.
    let w2 = window.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(400), move || {
        apply_width(&w2);
        // Pages fill in as readings arrive; keep their first heading tucked up.
        if let Some(page) = ui().and_then(|u| u.borrow().pages.get(u.borrow().current).cloned()) {
            mark_first_heading(page.upcast_ref());
        }
        glib::ControlFlow::Continue
    });

    // Hidden (another workspace, or covered in a monocle layout): stop the
    // expensive per-process and GPU sampling until it's back.
    window.connect_suspended_notify(|w| {
        live::set_detail(!(w.is_suspended() && prefs::get().pause_hidden));
    });

    let ui = Ui {
        window,
        stack,
        nav_items,
        pages: HashMap::new(),
        sections,
        current: "",
        toast,
        toast_timer: None,
        overlay,
        edit,
        header,
    };
    UI.with(|u| *u.borrow_mut() = Some(Rc::new(RefCell::new(ui))));
}

/// The parts of the sidebar that change when it collapses to icons.
struct Sidebar {
    nav: gtk::Box,
    /// Hidden in the icon-only sidebar.
    hide: Vec<gtk::Widget>,
}

thread_local! {
    static SIDEBAR: RefCell<Option<Sidebar>> = const { RefCell::new(None) };
    static NARROW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Ctrl+F: filter the process list on Processes; anywhere else, Go to.
fn find() {
    if current() == "processes" {
        crate::sections::processes::focus_search();
    } else {
        crate::palette::open();
    }
}

/// Every keyboard shortcut, for the shortcuts dialog and Settings.
pub const SHORTCUTS: &[(&[&str], &str)] = &[
    (&["Ctrl", "K"], "Go to a page, process, service or startup item"),
    (&["Ctrl", "F"], "Filter processes on the Processes page, or go to anything"),
    (&["/"], "Filter processes (from the process list)"),
    (&["Ctrl", "P"], "Pause or resume updates"),
    (&["Ctrl", "B"], "Collapse or expand the sidebar"),
    (&["Enter"], "Open a group, or switch to the process's window"),
    (&["← / →"], "Close or open a branch in Apps and Tree"),
    (&["Delete"], "End the selected processes"),
    (&["Shift", "Delete"], "Kill the selected processes"),
    (&["Right-click"], "Actions for a process, or export a graph"),
    (&["Esc"], "Clear the search"),
    (&["Ctrl", ","], "Settings"),
    (&["F1"], "Show these shortcuts (or ?)"),
    (&["Ctrl", "W"], "Close (Ctrl+Q too)"),
];

pub fn key_caps(keys: &[&str]) -> gtk::Box {
    let caps = widgets::hbox(4);
    for (i, k) in keys.iter().enumerate() {
        if i > 0 {
            caps.append(&widgets::label("+", "dim"));
        }
        caps.append(&widgets::label(k, "key-cap"));
    }
    caps
}

pub fn show_shortcuts() {
    let (dialog, card) = widgets::dialog("Keyboard shortcuts", 520);
    let list = widgets::vbox(0);
    list.add_css_class("group-list");
    for (keys, what) in SHORTCUTS {
        list.append(&widgets::row(what, "", Some(key_caps(keys).upcast_ref())));
    }
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        // Scrolls without a scrollbar, which would cover the key caps.
        .vscrollbar_policy(gtk::PolicyType::External)
        .propagate_natural_height(true)
        .max_content_height(560)
        .child(&list)
        .build();
    card.append(&scroll);
    let close = gtk::Button::with_label("Close");
    close.set_halign(gtk::Align::End);
    let d = dialog.clone();
    close.connect_clicked(move |_| d.close());
    card.append(&close);
    dialog.present();
}

/// The layer over the window, for the settings dialog.
pub fn overlay() -> Option<gtk::Overlay> {
    ui().map(|u| u.borrow().overlay.clone())
}

/// The sidebar shows only icons: hide the labels, centre the icons and the toggle.
fn apply_compact(compact: bool) {
    SIDEBAR.with(|s| {
        let s = s.borrow();
        let Some(s) = s.as_ref() else { return };
        if compact {
            s.nav.add_css_class("compact");
        } else {
            s.nav.remove_css_class("compact");
        }
        for w in &s.hide {
            w.set_visible(!compact);
        }
        centre_icons(s.nav.upcast_ref(), compact);
    });
}

fn centre_icons(w: &gtk::Widget, compact: bool) {
    if w.has_css_class("nav-item")
        && let Some(content) = w.downcast_ref::<gtk::Button>().and_then(|b| b.child())
    {
        content.set_halign(if compact { gtk::Align::Center } else { gtk::Align::Fill });
    }
    let mut child = w.first_child();
    while let Some(c) = child {
        centre_icons(&c, compact);
        child = c.next_sibling();
    }
}

pub fn toggle_sidebar() {
    prefs::update(|p| p.sidebar_collapsed = !p.sidebar_collapsed);
    apply_compact(NARROW.with(|n| n.get()) || prefs::get().sidebar_collapsed);
}

/// Mark pages for a narrow tiled window.
fn set_narrow(narrow: bool) {
    NARROW.with(|n| n.set(narrow));
    let Some(ui) = ui() else { return };
    let u = ui.borrow();
    for page in u.pages.values() {
        mark_page(page, narrow);
    }
}

fn mark_page(page: &gtk::ScrolledWindow, narrow: bool) {
    if let Some(body) = page.child().and_then(|v| v.first_child()) {
        if narrow {
            body.add_css_class("narrow");
        } else {
            body.remove_css_class("narrow");
        }
    }
}

fn ensure_built(id: &'static str) {
    let Some(ui) = ui() else { return };
    if ui.borrow().pages.contains_key(id) {
        return;
    }
    let section = {
        let u = ui.borrow();
        u.sections.iter().find(|s| s.id == id).map(|s| (s.id, s.build, s.fill))
    };
    let Some((sid, build, fill)) = section else { return };
    let page = widgets::page(sid);
    if fill {
        page.fill();
    }
    build(&page);
    mark_page(&page.root, NARROW.with(|n| n.get()));
    // Some pages add their content once the first readings arrive, so the
    // heading to tuck under the top edge is found each time the page shows.
    page.root.connect_map(|root| {
        mark_first_heading(root.upcast_ref());
        let root = root.clone();
        glib::idle_add_local_once(move || mark_first_heading(root.upcast_ref()));
    });
    let stack = ui.borrow().stack.clone();
    stack.add_named(&page.root, Some(sid));
    ui.borrow_mut().pages.insert(sid, page.root);
}

/// Give a page's first heading the `page-first` class (no space above it, so
/// the gap to the top edge matches the gap below it), unless something else
/// comes before it.
fn mark_first_heading(root: &gtk::Widget) {
    const CONTENT: &[&str] = &["graph-card", "group-list", "settings-card", "settings-option", "banner", "table-card", "toolbar"];
    // The first heading, unless content comes before it.
    fn target(w: &gtk::Widget) -> Option<Option<gtk::Widget>> {
        if !w.is_visible() {
            return None;
        }
        if w.has_css_class("group-title") || w.has_css_class("group-head") {
            return Some(Some(w.clone()));
        }
        if CONTENT.iter().any(|c| w.has_css_class(c)) {
            return Some(None);
        }
        let mut c = w.first_child();
        while let Some(x) = c {
            if let Some(found) = target(&x) {
                return Some(found);
            }
            c = x.next_sibling();
        }
        None
    }
    // Only touch classes that change, so a check that finds nothing new costs no restyle.
    fn apply(w: &gtk::Widget, first: Option<&gtk::Widget>) {
        let is = first == Some(w);
        if is != w.has_css_class("page-first") {
            if is {
                w.add_css_class("page-first");
            } else {
                w.remove_css_class("page-first");
            }
        }
        let mut c = w.first_child();
        while let Some(x) = c {
            apply(&x, first);
            c = x.next_sibling();
        }
    }
    let first = target(root).flatten();
    apply(root, first.as_ref());
}

pub fn navigate(id: &str) {
    let Some(ui) = ui() else { return };
    // Settings is a dialog over the window, not a page.
    if id == "settings" {
        settings_dialog::open();
        if !ui.borrow().current.is_empty() {
            return;
        }
    }
    let id = if id == "settings" { "overview" } else { id };
    let resolved = {
        let u = ui.borrow();
        u.sections.iter().find(|s| s.id == id).or_else(|| u.sections.first()).map(|s| s.id)
    };
    let Some(id) = resolved else { return };
    ensure_built(id);
    let mut u = ui.borrow_mut();
    if let Some(prev) = u.nav_items.get(u.current) {
        prev.remove_css_class("active");
    }
    if let Some(b) = u.nav_items.get(id) {
        b.add_css_class("active");
    }
    u.stack.set_visible_child_name(id);
    u.current = id;
    if let Some(s) = u.sections.iter().find(|s| s.id == id) {
        chrome::set_title(&u.window, "Activity", s.title);
        u.header.set_subtitle(s.title);
        while let Some(c) = u.edit.first_child() {
            u.edit.remove(&c);
        }
        let files = (s.files)();
        if !files.is_empty() {
            u.edit.append(&widgets::config_button(&files));
        }
    }
}

/// Rebuild a section page from scratch (after a change that alters its layout).
pub fn rebuild(id: &'static str) {
    let Some(ui) = ui() else { return };
    let old = ui.borrow_mut().pages.remove(id);
    if let Some(old) = old {
        ui.borrow().stack.remove(&old);
    }
    let current = ui.borrow().current;
    ensure_built(id);
    if current == id {
        ui.borrow().stack.set_visible_child_name(id);
    }
}

/// Show a short message at the bottom of the window. A newer message replaces
/// the one showing and restarts its timer.
pub fn toast(message: &str) {
    let Some(ui) = ui() else {
        eprintln!("nyx-activity: {message}");
        return;
    };
    let mut u = ui.borrow_mut();
    if let Some(t) = u.toast_timer.take() {
        t.remove();
    }
    u.toast.set_text(message);
    let bx = u.toast.parent();
    if let Some(bx) = &bx {
        bx.set_visible(true);
    }
    let weak = Rc::downgrade(&ui);
    u.toast_timer = Some(glib::timeout_add_local_once(std::time::Duration::from_millis(3500), move || {
        if let Some(ui) = weak.upgrade() {
            let mut u = ui.borrow_mut();
            u.toast_timer = None;
            if let Some(bx) = u.toast.parent() {
                bx.set_visible(false);
            }
        }
    }));
}

pub fn current() -> &'static str {
    ui().map(|u| u.borrow().current).unwrap_or("")
}

thread_local! {
    static PAUSE_BUTTONS: RefCell<Vec<gtk::Button>> = const { RefCell::new(Vec::new()) };
}

/// The live/paused pill in the header.
fn pause_button() -> gtk::Button {
    let b = gtk::Button::new();
    b.add_css_class("chip");
    b.add_css_class("live-pill");
    b.set_tooltip_text(Some("Pause or resume updates (Ctrl+P)"));
    b.connect_clicked(|_| toggle_pause());
    PAUSE_BUTTONS.with(|p| p.borrow_mut().push(b.clone()));
    refresh_pause();
    b
}

pub fn refresh_pause() {
    let paused = live::paused();
    let secs = prefs::get().interval_ms as f64 / 1000.0;
    let label = if paused { "Paused".to_string() } else { format!("Live · {secs} s") };
    PAUSE_BUTTONS.with(|p| {
        for b in p.borrow().iter() {
            b.set_label(&label);
            if paused {
                b.add_css_class("paused");
            } else {
                b.remove_css_class("paused");
            }
        }
    });
}

pub fn toggle_pause() {
    live::set_paused(!live::paused());
    refresh_pause();
}

pub fn window() -> Option<gtk::ApplicationWindow> {
    ui().map(|u| u.borrow().window.clone())
}
