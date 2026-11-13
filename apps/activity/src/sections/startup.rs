use crate::charts::{self, Bar, PAIR};
use crate::startup::{self, HyprEntry, Managed, Unit, XdgEntry};
use crate::widgets::{self, Page};
use crate::{apps, cmd, fmt, paths, window};
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

/// A startup row: icon, then the name and its tags on one line, and under it the
/// description (or the command, when there's none). The command is always in the tooltip.
fn item_row(icon: &str, name: &str, desc: &str, command: &str, tags: &[gtk::Label], controls: &[gtk::Widget]) -> gtk::Box {
    let row = widgets::hbox(12);
    row.add_css_class("settings-option");
    row.add_css_class("startup-row");
    let img = gtk::Image::from_icon_name(if icon.is_empty() { "utilities-terminal-symbolic" } else { icon });
    img.set_pixel_size(if icon.ends_with("-symbolic") || icon.is_empty() { 18 } else { 24 });
    img.set_size_request(24, -1);
    img.set_valign(gtk::Align::Center);
    row.append(&img);
    let text = widgets::vbox(1);
    text.set_hexpand(true);
    text.set_valign(gtk::Align::Center);
    let head = widgets::hbox(8);
    let t = widgets::label(name, "settings-option-title");
    t.set_ellipsize(pango::EllipsizeMode::End);
    head.append(&t);
    for tag in tags {
        head.append(tag);
    }
    text.append(&head);
    let second = if desc.is_empty() { command } else { desc };
    if !second.is_empty() {
        let d = widgets::label(second, "settings-option-description");
        if desc.is_empty() {
            d.add_css_class("mono");
        }
        d.set_ellipsize(pango::EllipsizeMode::End);
        text.append(&d);
    }
    if !command.is_empty() {
        row.set_tooltip_text(Some(command));
    }
    row.append(&text);
    for c in controls {
        c.set_valign(gtk::Align::Center);
        row.append(c);
    }
    row
}

fn timing_tag(secs: Option<f64>) -> Option<gtk::Label> {
    let s = secs.filter(|s| *s >= 0.05)?;
    let kind = if s >= 1.0 { "danger" } else { "" };
    let l = widgets::tag(&short_time(s), kind);
    l.set_tooltip_text(Some("How long it took to start at login"));
    Some(l)
}

fn run_tag(unit: Option<&Unit>) -> Option<gtk::Label> {
    let u = unit?;
    if u.failed() {
        Some(widgets::tag("Failed", "danger"))
    } else if u.running() {
        Some(widgets::tag("Running", "accent"))
    } else {
        None
    }
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

/// A group of rows: a title, one line about it (more in its tooltip), then the rows.
fn group(page: &Page, title: &str, note: &str, more: &str) -> gtk::Box {
    let g = page.group(title);
    if !note.is_empty() {
        g.note(note);
    }
    if !more.is_empty() {
        g.wrapper.set_tooltip_text(Some(more));
    }
    g.list
}

/// A group that starts folded: its title line has a button to show the rows.
fn folded(page: &Page, title: &str, note: &str, count: usize) -> gtk::Box {
    let g = page.group(title);
    if !note.is_empty() {
        g.note(note);
    }
    let toggle = gtk::Button::with_label(&format!("Show {count}"));
    toggle.add_css_class("flat");
    toggle.add_css_class("group-action");
    g.header_end(&toggle);
    let revealer = gtk::Revealer::new();
    revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
    revealer.set_transition_duration(if crate::prefs::get().reduce_motion { 0 } else { 160 });
    let list = widgets::vbox(0);
    list.add_css_class("group-list");
    revealer.set_child(Some(&list));
    let key = format!("startup.{title}");
    let open = crate::prefs::get().expanded_groups.get(&key).copied().unwrap_or(false);
    revealer.set_reveal_child(open);
    toggle.set_label(&if open { "Hide".to_string() } else { format!("Show {count}") });
    g.list.add_css_class("bare");
    g.add(&revealer);
    toggle.connect_clicked(move |b| {
        let open = !revealer.reveals_child();
        revealer.set_reveal_child(open);
        crate::prefs::update(|p| {
            p.expanded_groups.insert(key.clone(), open);
        });
        b.set_label(&if open { "Hide".to_string() } else { format!("Show {count}") });
    });
    list
}

/// The boot, phase by phase, and what took longest at login.
fn summary(page: &Page, d: &Data) {
    let g = page.cards("At login");

    let names: Vec<String> = d.boot.iter().map(|(l, _)| boot_phase(l)).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let bar = charts::stack_bar(&refs, None);
    let total: f64 = d.boot.iter().map(|(_, s)| s).sum();
    bar.set(
        &d.boot.iter().map(|(_, s)| *s).collect::<Vec<_>>(),
        total.max(0.001),
        &d.boot.iter().map(|(_, s)| short_time(*s)).collect::<Vec<_>>(),
    );
    let body = widgets::vbox(14);
    body.append(&bar.root);
    let session: f64 = d.session.iter().map(|(_, s)| s).sum();
    let note = widgets::label(
        &if session > 0.0 { format!("Then your session's services took {}.", short_time(session)) } else { String::new() },
        "dim",
    );
    note.set_wrap(true);
    body.append(&note);
    let (boot_card, boot_sum) = charts::card("Last boot", &body);
    boot_sum.set_text(&if total > 0.0 { format!("{} to the desktop", short_time(total)) } else { "not measured".into() });
    if d.boot.is_empty() {
        bar.root.set_visible(false);
        note.set_text("systemd didn't record this boot's timing.");
    }

    let mut blame: Vec<(&String, &f64)> = d.blame.iter().collect();
    blame.sort_by(|a, b| b.1.total_cmp(a.1));
    let list = charts::bar_list(6, "Nothing was measured.", false);
    let max = blame.first().map_or(1.0, |b| *b.1).max(0.001);
    let bars: Vec<Bar> = blame
        .iter()
        .take(6)
        .map(|(unit, secs)| Bar {
            name: pretty_unit(unit),
            frac: **secs / max,
            value: short_time(**secs),
            tip: unit.to_string(),
            warn: **secs >= 5.0,
            ..Default::default()
        })
        .collect();
    list.set(&bars);
    let (slow_card, _) = charts::card("Slowest to start", &list.root);
    slow_card.set_tooltip_text(Some("What took longest at your last login (systemd's measurements)"));
    g.add(&charts::row(&[boot_card.upcast_ref(), slow_card.upcast_ref()], PAIR));

    let managed_on = d.managed.hyprland.iter().filter(|m| m.enabled).count();
    let hypr = charts::tile("With Hyprland");
    hypr.value.set_text(&(d.user.len() + d.omarchy.len() + managed_on).to_string());
    hypr.sub.set_text(&format!("{} Omarchy · {} yours", d.omarchy.len(), d.user.len() + managed_on));
    let here: Vec<&XdgEntry> = d.xdg.iter().filter(|e| !e.other_desktop).collect();
    let apps_on = here.iter().filter(|e| e.enabled).count();
    let apps_tile = charts::tile("Apps");
    apps_tile.value.set_text(&apps_on.to_string());
    apps_tile.sub.set_text(&format!("of {} that can start here", here.len()));
    let svc: Vec<&Unit> = d.units.iter().filter(|u| u.file_state == "enabled").collect();
    let svc_tile = charts::tile("Background services");
    svc_tile.value.set_text(&svc.len().to_string());
    svc_tile.sub.set_text(&format!("{} running now", svc.iter().filter(|u| u.running()).count()));
    let off = d.xdg.iter().filter(|e| !e.enabled && !e.other_desktop).count()
        + d.managed.hyprland.iter().filter(|m| !m.enabled).count()
        + d.units.iter().filter(|u| u.file_state == "disabled" && !u.name.starts_with("app-")).count();
    let off_tile = charts::tile("Turned off");
    off_tile.value.set_text(&off.to_string());
    off_tile.sub.set_text("won't start at login");
    g.add(&charts::row(
        &[hypr.root.upcast_ref(), apps_tile.root.upcast_ref(), svc_tile.root.upcast_ref(), off_tile.root.upcast_ref()],
        150,
    ));
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

fn fill(page: &Page, d: Data) {
    let units: HashMap<&str, &Unit> = d.units.iter().map(|u| (u.name.as_str(), u)).collect();
    summary(page, &d);

    // ----- Added in Activity -----
    let list =
        group(page, "Added here", "Started by Hyprland when you log in.", "Activity keeps these in ~/.config/hypr/activity.lua.");
    if !d.managed.hyprland.is_empty() && !d.included {
        let b = widgets::banner(
            "Hyprland doesn't load <tt>activity.lua</tt> yet, so these won't start. \
             Activity can add one line, <tt>require(\"hypr.activity\")</tt>, to the end of <tt>hyprland.lua</tt> (after a backup).",
            true,
        );
        let add = gtk::Button::with_label("Add the line");
        add.add_css_class("suggested-action");
        add.set_valign(gtk::Align::Center);
        add.connect_clicked(|_| match startup::add_include() {
            Ok(backup) => {
                window::toast(&format!("Added. Backup: {}", paths::pretty(&backup)));
                reload();
            }
            Err(e) => window::toast(&format!("Couldn't edit hyprland.lua: {e:#}")),
        });
        b.append(&add);
        list.append(&b);
    }
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
        list.append(&item_row(&icon, &m.name, "", &m.command, &[], &[remove.upcast(), sw.upcast()]));
    }
    let (add_row, _) =
        widgets::button_row("Add a startup item", "Pick an app, or type any command to run at login.", "Add…", |_| {
            add_dialog()
        });
    list.append(&add_row);

    // ----- XDG autostart -----
    let list = group(
        page,
        "Apps",
        "Programs that asked to start at login. Turning one off leaves the system file alone.",
        "From ~/.config/autostart and /etc/xdg/autostart.",
    );
    let (here, elsewhere): (Vec<&XdgEntry>, Vec<&XdgEntry>) = d.xdg.iter().partition(|e| !e.other_desktop);
    if here.is_empty() {
        list.append(&widgets::row("Nothing here", "No app has asked to start at login.", None));
    }
    for e in here {
        list.append(&xdg_row(e, &units, &d.blame));
    }
    if !elsewhere.is_empty() {
        let fold = folded(
            page,
            "Only for other desktops",
            "These won't start under Hyprland either way (OnlyShowIn/NotShowIn).",
            elsewhere.len(),
        );
        for e in elsewhere {
            let row = xdg_row(e, &units, &d.blame);
            row.set_opacity(0.6);
            fold.append(&row);
        }
    }

    // ----- systemd user services -----
    let list = group(
        page,
        "Background services",
        "Your systemd user services that start with the session.",
        "The Services page has every unit, with logs and details.",
    );
    let mut svc: Vec<&Unit> = d
        .units
        .iter()
        .filter(|u| matches!(u.file_state.as_str(), "enabled" | "disabled") && !u.name.starts_with("app-"))
        .collect();
    svc.sort_by(|a, b| (a.file_state != "enabled", &a.name).cmp(&(b.file_state != "enabled", &b.name)));
    if svc.is_empty() {
        list.append(&widgets::row("Nothing here", "No user services can be enabled or disabled.", None));
    }
    for u in svc {
        let mut tags = Vec::new();
        if let Some(t) = run_tag(Some(u)) {
            tags.push(t);
        }
        if let Some(t) = timing_tag(d.blame.get(&u.name).copied()) {
            tags.push(t);
        }
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
        let title = startup::unescape(u.name.trim_end_matches(".service"));
        let row = item_row("emblem-system-symbolic", &title, &u.description, "", &tags, &[sw.upcast()]);
        row.set_tooltip_text(Some(&u.name));
        list.append(&row);
    }

    // ----- Hyprland: yours, then Omarchy's (for reference) -----
    let list = folded(page, "Your Hyprland autostart", "From autostart.lua. Use Open config to change these.", d.user.len());
    if d.user.is_empty() {
        list.append(&widgets::row("Nothing here", "Your autostart.lua doesn't start anything.", None));
    }
    for e in &d.user {
        let (name, icon) = app_name(&e.command);
        list.append(&item_row(&icon, &name, "", &e.command, &[], &[]));
    }
    let list = folded(page, "Omarchy", "Omarchy's own startup: the bar, notifications, drive mounting.", d.omarchy.len());
    for e in &d.omarchy {
        let (name, icon) = app_name(&e.command);
        list.append(&item_row(&icon, &name, "", &e.command, &[], &[]));
    }
}

fn xdg_row(e: &XdgEntry, units: &HashMap<&str, &Unit>, blame: &HashMap<String, f64>) -> gtk::Box {
    let unit_name = startup::xdg_unit(&e.id);
    let unit = units.get(unit_name.as_str()).copied();
    let mut tags = Vec::new();
    if let Some(t) = run_tag(unit) {
        tags.push(t);
    }
    if let Some(t) = timing_tag(blame.get(&unit_name).copied()) {
        tags.push(t);
    }
    if !e.system {
        tags.push(widgets::tag("Yours", ""));
    }
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
    sw.set_sensitive(!e.other_desktop);
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
    item_row(&e.icon, &e.name, &e.comment, &e.exec, &tags, &controls)
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
