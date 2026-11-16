use crate::startup::{self, HyprEntry, Managed, Unit, XdgEntry};
use crate::widgets::{self, Page};
use crate::{apps, cmd, fmt, parts, paths, window};
use gtk::pango;
use gtk::prelude::*;
use std::collections::HashMap;

struct Data {
    managed: startup::State,
    included: bool,
    user: Vec<HyprEntry>,
    omarchy: Vec<HyprEntry>,
    xdg: Vec<XdgEntry>,
    units: Vec<Unit>,
    blame: HashMap<String, f64>,
    boot: Vec<(String, f64)>,
    session: Vec<(String, f64)>,
}

fn load() -> Data {
    Data {
        managed: startup::load(),
        included: startup::included(),
        user: startup::user_entries(),
        omarchy: startup::omarchy_entries(),
        xdg: startup::xdg_entries(),
        units: startup::units(true),
        blame: startup::blame(true),
        boot: startup::boot_time(false),
        session: startup::boot_time(true),
    }
}

fn reload() {
    window::rebuild("startup");
}

fn app_name(command: &str) -> (String, String) {
    // "sleep 2 && omarchy-hook post-boot" is about the last command, not sleep.
    let last = command.rsplit("&&").next().and_then(|c| c.rsplit(';').next()).map(str::trim).unwrap_or(command);
    let prog = apps::exec_program(last);
    match apps::lookup(&prog) {
        Some(a) => (a.name, a.icon),
        None => (prog, "utilities-terminal-symbolic".into()),
    }
}

pub fn build(page: &Page) {
    let holder = widgets::vbox(0);
    page.body.append(&holder);
    let loading = widgets::loading("Reading startup items…");
    holder.append(&loading);
    let page = Page { root: page.root.clone(), body: holder.clone() };
    cmd::background(load, move |d| {
        holder.remove(&loading);
        fill(&page, d);
    });
}

/// Your last start as one labelled bar: firmware, boot loader, kernel, system services.
fn last_start(d: &Data) -> gtk::Box {
    const COLOURS: &[&str] = &["violet", "blue", "pink", "teal", "amber", "green"];
    let c = parts::card("system-reboot-symbolic", "Your last start");
    let total: f64 = d.boot.iter().map(|(_, s)| s).sum();
    let session: f64 = d.session.iter().map(|(_, s)| s).sum();
    if d.boot.is_empty() {
        let l = widgets::label("systemd didn't record this start's timing.", "dim");
        l.set_xalign(0.0);
        c.root.append(&l);
        return c.root;
    }
    let (bar, split) = super::dashboard::split_bar(COLOURS);
    bar.set_content_height(14);
    *split.borrow_mut() = (d.boot.iter().map(|(_, s)| *s).collect(), total.max(0.001));
    c.root.append(&bar);
    let names: Vec<String> = d.boot.iter().map(|(l, _)| boot_phase(l)).collect();
    let keys: Vec<(&str, &'static str)> = names.iter().zip(COLOURS.iter().cycle()).map(|(n, c)| (n.as_str(), *c)).collect();
    let (row, labels) = super::dashboard::key_row(&keys);
    for (l, ((name, _), (_, secs))) in labels.iter().zip(keys.iter().zip(&d.boot)) {
        l.set_markup(&format!("{name} <b>{}</b>", short_time(*secs)));
    }
    c.root.append(&parts::wrap_keys(&row));
    let line = if session > 0.0 {
        format!("{} from power on to the sign-in screen, then {} for your session.", short_time(total), short_time(session))
    } else {
        format!("{} from power on to the sign-in screen.", short_time(total))
    };
    let l = widgets::label(&line, "page-card-line");
    l.set_xalign(0.0);
    l.set_wrap(true);
    c.root.append(&l);
    c.root
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    App,
    Hyprland,
    Service,
}

/// One thing that starts at sign-in: what it is, how long it took, and its controls.
struct Item {
    kind: Kind,
    name: String,
    icon: String,
    /// What it is, under its name.
    what: String,
    tip: String,
    secs: Option<f64>,
    failed: bool,
    controls: Vec<gtk::Widget>,
}

fn items(d: &Data) -> Vec<Item> {
    let units: HashMap<&str, &Unit> = d.units.iter().map(|u| (u.name.as_str(), u)).collect();
    let mut out = Vec::new();
    for (i, m) in d.managed.hyprland.iter().enumerate() {
        let (_, icon) = app_name(&m.command);
        let sw = gtk::Switch::new();
        sw.set_active(m.enabled);
        sw.set_tooltip_text(Some("Start at login"));
        sw.connect_active_notify(move |s| {
            let on = s.is_active();
            let mut st = startup::load();
            if let Some(item) = st.hyprland.get_mut(i) {
                item.enabled = on;
            }
            if let Err(e) = startup::save(&st) {
                window::toast(&format!("Couldn't save: {e:#}"));
            }
        });
        let remove = widgets::two_click("Remove", "Click again to remove", move || {
            let mut st = startup::load();
            if i < st.hyprland.len() {
                st.hyprland.remove(i);
            }
            match startup::save(&st) {
                Ok(()) => reload(),
                Err(e) => window::toast(&format!("Couldn't save: {e:#}")),
            }
        });
        out.push(Item {
            kind: Kind::Hyprland,
            name: m.name.clone(),
            icon,
            what: "Hyprland · added here".into(),
            tip: m.command.clone(),
            secs: None,
            failed: false,
            controls: vec![remove.upcast(), sw.upcast()],
        });
    }
    for (e, whose) in d.user.iter().map(|e| (e, "yours")).chain(d.omarchy.iter().map(|e| (e, "Omarchy's"))) {
        let (name, icon) = app_name(&e.command);
        let sw = gtk::Switch::new();
        sw.set_active(true);
        sw.set_sensitive(false);
        sw.set_tooltip_text(Some(&format!("Started by {whose} autostart.lua; change it there (Open config)")));
        out.push(Item {
            kind: Kind::Hyprland,
            name,
            icon,
            what: format!("Hyprland · {whose}"),
            tip: e.command.clone(),
            secs: None,
            failed: false,
            controls: vec![sw.upcast()],
        });
    }
    for e in d.xdg.iter().filter(|e| !e.other_desktop) {
        let unit = startup::xdg_unit(&e.id);
        out.push(Item {
            kind: Kind::App,
            name: e.name.clone(),
            icon: e.icon.clone(),
            what: if e.system { "App".into() } else { "App · yours".into() },
            tip: if e.comment.is_empty() { e.exec.clone() } else { format!("{}\n{}", e.comment, e.exec) },
            secs: d.blame.get(&unit).copied(),
            failed: units.get(unit.as_str()).is_some_and(|u| u.failed()),
            controls: xdg_controls(e),
        });
    }
    for u in d.units.iter().filter(|u| matches!(u.file_state.as_str(), "enabled" | "disabled") && !u.name.starts_with("app-")) {
        let sw = gtk::Switch::new();
        sw.set_active(u.file_state == "enabled");
        sw.set_tooltip_text(Some("Enable at login"));
        let name = u.name.clone();
        sw.connect_active_notify(move |s| {
            let verb = if s.is_active() { "enable" } else { "disable" };
            let n = name.clone();
            cmd::run_async(&["systemctl", "--user", verb, &name], move |r| match r {
                Ok(_) => window::toast(&format!("{n}: {verb}d")),
                Err(e) => window::toast(&format!("Couldn't {verb} {n}: {e}")),
            });
        });
        out.push(Item {
            kind: Kind::Service,
            name: startup::unescape(u.name.trim_end_matches(".service")),
            icon: String::new(),
            what: if u.description.is_empty() { "Service".into() } else { format!("Service · {}", u.description) },
            tip: u.name.clone(),
            secs: d.blame.get(&u.name).copied(),
            failed: u.failed(),
            controls: vec![sw.upcast()],
        });
    }
    // Slowest first; the ones that weren't timed after, by name.
    out.sort_by(|a, b| {
        b.secs.unwrap_or(-1.0).total_cmp(&a.secs.unwrap_or(-1.0)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

fn item_row(it: &Item) -> gtk::Box {
    let row = widgets::hbox(12);
    row.add_css_class("startup-row");
    let face = super::processes::face("app-row", 28);
    super::processes::set_face(face.upcast_ref(), &it.icon, &it.name, crate::theme::palette().light);
    row.append(&face);
    let text = widgets::vbox(1);
    text.set_hexpand(true);
    text.set_valign(gtk::Align::Center);
    let t = widgets::label(&it.name, "startup-name");
    t.set_xalign(0.0);
    t.set_ellipsize(pango::EllipsizeMode::End);
    let w = widgets::label(&it.what, "startup-what");
    w.set_xalign(0.0);
    w.set_ellipsize(pango::EllipsizeMode::End);
    text.append(&t);
    text.append(&w);
    row.append(&text);
    if !it.tip.is_empty() {
        row.set_tooltip_text(Some(&it.tip));
    }
    if it.failed {
        row.append(&widgets::tag("Failed", "danger"));
    }
    if let Some(s) = it.secs.filter(|s| *s >= 0.05) {
        let l = widgets::label(&short_time(s), "startup-time");
        l.add_css_class("tnum");
        if s >= 60.0 {
            l.add_css_class("slow");
        }
        l.set_tooltip_text(Some("How long it took to start at your last login"));
        row.append(&l);
    }
    for c in &it.controls {
        c.set_valign(gtk::Align::Center);
        row.append(c);
    }
    row
}

fn fill(page: &Page, d: Data) {
    let g = page.cards("");
    g.add(&last_start(&d));

    // ----- What to show, and adding to it -----
    let items = items(&d);
    let count = |k: Option<Kind>| items.iter().filter(|i| k.is_none_or(|k| i.kind == k)).count();
    let tool = widgets::hbox(6);
    tool.add_css_class("startup-tool");
    let list = widgets::vbox(0);
    list.add_css_class("graph-card");
    list.add_css_class("startup-list");
    let rows: Vec<(Kind, gtk::Box)> = items.iter().map(|it| (it.kind, item_row(it))).collect();
    for (_, r) in &rows {
        list.append(r);
    }
    let rows = std::rc::Rc::new(rows);
    let mut first: Option<gtk::ToggleButton> = None;
    for (k, label) in
        [(None, "All"), (Some(Kind::App), "Apps"), (Some(Kind::Hyprland), "Hyprland"), (Some(Kind::Service), "Services")]
    {
        let n = count(k);
        if k.is_some() && n == 0 {
            continue;
        }
        let b = gtk::ToggleButton::with_label(&format!("{label} {n}"));
        b.add_css_class("chip");
        b.add_css_class("small");
        match &first {
            Some(f) => b.set_group(Some(f)),
            None => {
                b.set_active(true);
                b.add_css_class("selected");
                first = Some(b.clone());
            }
        }
        let rows = rows.clone();
        b.connect_toggled(move |b| {
            if !b.is_active() {
                b.remove_css_class("selected");
                return;
            }
            b.add_css_class("selected");
            for (kind, r) in rows.iter() {
                r.set_visible(k.is_none_or(|k| k == *kind));
            }
        });
        tool.append(&b);
    }
    let spacer = widgets::hbox(0);
    spacer.set_hexpand(true);
    tool.append(&spacer);
    let add = widgets::labeled_button("list-add-symbolic", "Add…");
    add.set_tooltip_text(Some("Pick an app, or type any command to run at login"));
    add.connect_clicked(|_| add_dialog());
    tool.append(&add);
    g.add(&tool);

    // ----- What held it up -----
    if !d.managed.hyprland.is_empty() && !d.included {
        let b = widgets::banner(
            "Hyprland doesn't load <tt>activity.lua</tt> yet, so what you add here won't start. \
             Activity can add one line, <tt>require(\"hypr.activity\")</tt>, to the end of <tt>hyprland.lua</tt> (after a backup).",
            true,
        );
        let fix = gtk::Button::with_label("Add the line");
        fix.add_css_class("suggested-action");
        fix.set_valign(gtk::Align::Center);
        fix.connect_clicked(|_| match startup::add_include() {
            Ok(backup) => {
                window::toast(&format!("Added. Backup: {}", paths::pretty(&backup)));
                reload();
            }
            Err(e) => window::toast(&format!("Couldn't edit hyprland.lua: {e:#}")),
        });
        b.append(&fix);
        g.add(&b);
    }
    let session: f64 = d.session.iter().map(|(_, s)| s).sum();
    if let Some((unit, secs)) = d.blame.iter().max_by(|a, b| a.1.total_cmp(b.1)).filter(|(_, s)| **s >= 3.0) {
        let note = parts::note();
        let name = pretty_unit(unit);
        if session > 0.0 && *secs > session {
            note.set(
                "system-reboot-symbolic",
                parts::Tone::Plain,
                &format!("{name} took {} to start at your last login", short_time(*secs)),
                "It kept going after your session was ready, so it didn't hold you up.",
            );
        } else {
            note.set(
                "system-reboot-symbolic",
                parts::Tone::Warn,
                &format!("{name} held up your last start by {}", short_time(*secs)),
                "Your session waits for it before it's ready.",
            );
        }
        let unit = unit.clone();
        note.on_action("See log", move || super::services::open_logs(&unit, true));
        g.add(&note.root);
    }
    if items.is_empty() {
        g.add(&widgets::row("Nothing starts at login", "Add an app or a command to start it when you sign in.", None));
    } else {
        g.add(&list);
    }
}

/// An app's controls: reset a system entry you changed, or remove one you added, then its switch.
fn xdg_controls(e: &XdgEntry) -> Vec<gtk::Widget> {
    let mut controls: Vec<gtk::Widget> = Vec::new();
    if e.system && e.user_file.is_some() {
        let reset = gtk::Button::from_icon_name("edit-undo-symbolic");
        reset.add_css_class("reset-button");
        reset.add_css_class("flat");
        reset.set_tooltip_text(Some("Restore the system default (remove your override)"));
        let e2 = e.clone();
        reset.connect_clicked(move |_| match startup::remove_user_xdg(&e2) {
            Ok(()) => reload(),
            Err(err) => window::toast(&format!("Couldn't remove the override: {err}")),
        });
        controls.push(reset.upcast());
    } else if !e.system {
        let e2 = e.clone();
        let remove = widgets::two_click("Remove", "Click again to remove", move || match startup::remove_user_xdg(&e2) {
            Ok(()) => reload(),
            Err(err) => window::toast(&format!("Couldn't remove it: {err}")),
        });
        controls.push(remove.upcast());
    }
    let sw = gtk::Switch::new();
    sw.set_active(e.enabled);
    sw.set_tooltip_text(Some("Start at login"));
    let e2 = e.clone();
    sw.connect_active_notify(move |s| {
        if let Err(err) = startup::set_xdg_enabled(&e2, s.is_active()) {
            window::toast(&format!("Couldn't change it: {err:#}"));
        } else {
            window::toast(if s.is_active() { "It will start at your next login" } else { "It won't start at your next login" });
        }
    });
    controls.push(sw.upcast());
    controls
}

fn boot_phase(label: &str) -> String {
    match label {
        "firmware" => "Firmware".into(),
        "loader" => "Boot loader".into(),
        "kernel" => "Kernel".into(),
        "initrd" => "Initial ramdisk".into(),
        "userspace" => "System services".into(),
        other => other.to_string(),
    }
}

pub fn short_time(secs: f64) -> String {
    if secs < 1.0 {
        format!("{} ms", (secs * 1000.0).round())
    } else if secs < 60.0 {
        format!("{secs:.2} s")
    } else {
        fmt::duration(secs)
    }
}

/// "app-dropbox@autostart.service" -> "dropbox"; "foo.service" -> "foo".
pub fn pretty_unit(unit: &str) -> String {
    let u = unit.trim_end_matches(".service");
    let u = u.strip_prefix("app-").and_then(|s| s.strip_suffix("@autostart")).unwrap_or(u);
    startup::unescape(u)
}

fn add_dialog() {
    let (dialog, card) = widgets::dialog("Add a startup item", 560);
    let intro = widgets::label("Pick an app to start at login, or type a command.", "dim");
    intro.set_wrap(true);
    card.append(&intro);

    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search apps"));
    card.append(&search);
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.add_css_class("table-card");
    let all = apps::all();
    let shown: Vec<apps::App> = all.iter().filter(|a| !a.hidden && !a.name.is_empty()).cloned().collect();
    for a in &shown {
        let r = widgets::hbox(10);
        r.set_margin_top(4);
        r.set_margin_bottom(4);
        r.set_margin_start(8);
        let img = gtk::Image::from_icon_name(if a.icon.is_empty() { "application-x-executable-symbolic" } else { &a.icon });
        img.set_pixel_size(20);
        r.append(&img);
        let n = widgets::label(&a.name, "");
        n.set_hexpand(true);
        r.append(&n);
        list.append(&r);
    }
    let names: Vec<String> = shown.iter().map(|a| format!("{} {}", a.name, a.id).to_lowercase()).collect();
    let query = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
    let q = query.clone();
    list.set_filter_func(move |row| names.get(row.index() as usize).is_some_and(|n| n.contains(q.borrow().as_str())));
    let l2 = list.clone();
    search.connect_search_changed(move |e| {
        *query.borrow_mut() = e.text().trim().to_lowercase();
        l2.invalidate_filter();
    });
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .min_content_height(220)
        .max_content_height(300)
        .child(&list)
        .build();
    card.append(&scroll);

    card.append(&widgets::label("OR RUN A COMMAND", "group-title"));
    let name = gtk::Entry::new();
    name.set_placeholder_text(Some("Name, e.g. Clipboard history"));
    let command = gtk::Entry::new();
    command.set_placeholder_text(Some("Command, e.g. wl-paste --watch cliphist store"));
    command.add_css_class("mono");
    card.append(&name);
    card.append(&command);

    let buttons = widgets::hbox(8);
    buttons.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let add = gtk::Button::with_label("Add");
    add.add_css_class("suggested-action");
    buttons.append(&cancel);
    buttons.append(&add);
    card.append(&buttons);

    let d = dialog.clone();
    cancel.connect_clicked(move |_| d.close());
    let d = dialog.clone();
    add.connect_clicked(move |_| {
        let cmd_text = command.text().trim().to_string();
        if !cmd_text.is_empty() {
            let label = name.text().trim().to_string();
            let label = if label.is_empty() { apps::exec_program(&cmd_text) } else { label };
            let mut st = startup::load();
            st.hyprland.push(Managed { name: label, command: cmd_text, enabled: true });
            match startup::save(&st) {
                Ok(()) => {
                    d.close();
                    window::toast("Added. It starts at your next login.");
                    reload();
                }
                Err(e) => window::toast(&format!("Couldn't save: {e:#}")),
            }
            return;
        }
        let Some(row) = list.selected_row() else {
            window::toast("Pick an app or type a command");
            return;
        };
        let Some(app) = shown.get(row.index() as usize) else { return };
        match startup::add_xdg(app) {
            Ok(()) => {
                d.close();
                window::toast(&format!("{} will start at your next login", app.name));
                reload();
            }
            Err(e) => window::toast(&format!("Couldn't add it: {e:#}")),
        }
    });
    dialog.present();
}
