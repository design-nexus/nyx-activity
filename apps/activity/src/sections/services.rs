use crate::charts::{self, Bar, PAIR};
use crate::sampler::groups::Kind;
use crate::startup::{self, Timer, Unit};
use crate::widgets::{self, Page};
use crate::{cmd, fmt, live, prefs, window};
use gtk::prelude::*;
use gtk::{gio, glib, pango};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Show {
    All,
    Running,
    Failed,
}

impl Show {
    fn id(self) -> &'static str {
        match self {
            Show::All => "all",
            Show::Running => "running",
            Show::Failed => "failed",
        }
    }
}

type Chip<'a> = (&'a str, Rc<dyn Fn()>);

/// One row: a service, or a timer with the unit it starts.
#[derive(Clone, PartialEq)]
struct Entry {
    unit: Unit,
    timer: Option<Timer>,
}

struct State {
    user: Cell<bool>,
    timers: Cell<bool>,
    show: Cell<Show>,
    query: RefCell<String>,
    entries: RefCell<HashMap<String, Entry>>,
    /// Rows by unit name, kept between refreshes so the list doesn't jump.
    rows: RefCell<HashMap<String, gtk::ListBoxRow>>,
    /// Each row's memory figure, refreshed in place.
    mem_labels: RefCell<HashMap<String, gtk::Label>>,
    loading: Cell<bool>,
    list: gtk::ListBox,
    stack: gtk::Stack,
    banner: gtk::Box,
    count: gtk::Label,
    states: charts::StackBar,
    memory: charts::BarList,
    memory_sum: gtk::Label,
}

/// Memory each unit's cgroup holds, from the last sample (services only).
fn unit_memory(user: bool) -> HashMap<String, u64> {
    let Some(snap) = live::latest() else { return HashMap::new() };
    snap.groups
        .iter()
        .filter(|g| g.kind == Kind::Service && g.path.contains("/user@") == user)
        .map(|g| (g.unit.clone(), g.memory()))
        .collect()
}

/// The running / failed / stopped split and the hungriest services.
fn refresh_summary(st: &State) {
    let entries = st.entries.borrow();
    let running = entries.values().filter(|e| e.unit.running()).count();
    let failed = entries.values().filter(|e| e.unit.failed()).count();
    let stopped = entries.len().saturating_sub(running + failed);
    let parts = [running as f64, failed as f64, stopped as f64];
    st.states.set(&parts, (entries.len() as f64).max(1.0), &parts.map(|v| (v as u64).to_string()));

    let mem = unit_memory(st.user.get());
    for (name, label) in st.mem_labels.borrow().iter() {
        let text = mem.get(name).map(|m| fmt::bytes(*m as f64)).unwrap_or_default();
        if label.text() != text {
            label.set_text(&text);
        }
    }
    let mut top: Vec<(&String, &u64)> = mem.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1));
    let max = top.first().map_or(1, |t| *t.1).max(1) as f64;
    let bars: Vec<Bar> = top
        .iter()
        .take(4)
        .map(|(unit, m)| Bar {
            name: pretty_name(unit),
            frac: **m as f64 / max,
            value: fmt::bytes(**m as f64),
            tip: unit.to_string(),
            ..Default::default()
        })
        .collect();
    st.memory.set(&bars);
    let total: u64 = mem.values().sum();
    st.memory_sum.set_text(&if total > 0 { format!("{} in all", fmt::bytes(total as f64)) } else { String::new() });
}

fn load(st: &Rc<State>, quiet: bool) {
    if st.loading.replace(true) {
        return;
    }
    let (user, timers) = (st.user.get(), st.timers.get());
    if !quiet {
        st.count.set_text("Loading…");
    }
    let st = st.clone();
    cmd::background(
        move || {
            if timers {
                startup::timers(user).into_iter().map(|t| Entry { unit: t.unit.clone(), timer: Some(t) }).collect()
            } else {
                startup::units(user).into_iter().map(|unit| Entry { unit, timer: None }).collect::<Vec<_>>()
            }
        },
        move |entries| {
            st.loading.set(false);
            // The scope changed while this was loading: its own load follows.
            if st.user.get() != user || st.timers.get() != timers {
                load(&st, false);
                return;
            }
            populate(&st, entries);
        },
    );
}

fn state_tag(u: &Unit, timer: bool) -> gtk::Label {
    if u.failed() {
        widgets::tag("Failed", "danger")
    } else if u.running() {
        let text = if timer && u.sub == "waiting" {
            "Scheduled"
        } else if u.sub == "running" {
            "Running"
        } else {
            &u.sub
        };
        widgets::tag(text, "accent")
    } else {
        widgets::tag("Stopped", "")
    }
}

/// Bring the list in line with `entries`: update changed rows in place, add new
/// ones, drop gone ones. Rows that didn't change aren't touched.
fn populate(st: &Rc<State>, entries: Vec<Entry>) {
    let failed = entries.iter().filter(|e| e.unit.failed()).count();
    st.banner.set_visible(failed > 0);
    if let Some(l) = st.banner.last_child().and_downcast::<gtk::Label>() {
        let what = if st.timers.get() { "timer" } else { "service" };
        l.set_text(&format!("{failed} {what}{} failed. Open one to see more.", if failed == 1 { "" } else { "s" }));
    }
    {
        let mut rows = st.rows.borrow_mut();
        let old = st.entries.borrow();
        let names: std::collections::HashSet<&str> = entries.iter().map(|e| e.unit.name.as_str()).collect();
        rows.retain(|name, row| {
            let keep = names.contains(name.as_str());
            if !keep {
                st.list.remove(row);
                st.mem_labels.borrow_mut().remove(name);
            }
            keep
        });
        for (i, e) in entries.iter().enumerate() {
            match rows.get(&e.unit.name) {
                Some(row) => {
                    // Timers say "next in 3h", so they're redrawn every time.
                    if old.get(&e.unit.name) != Some(e) || e.timer.is_some() {
                        row.set_child(Some(&row_content(st, e)));
                    }
                }
                None => {
                    let row = gtk::ListBoxRow::new();
                    row.set_widget_name(&e.unit.name);
                    row.set_child(Some(&row_content(st, e)));
                    st.list.insert(&row, i as i32);
                    rows.insert(e.unit.name.clone(), row);
                }
            }
        }
    }
    *st.entries.borrow_mut() = entries.into_iter().map(|e| (e.unit.name.clone(), e)).collect();
    st.list.invalidate_filter();
    refresh_count(st);
    refresh_summary(st);
}

fn refresh_count(st: &State) {
    let entries = st.entries.borrow();
    let shown = entries.values().filter(|e| visible(st, e)).count();
    if st.loading.get() && entries.is_empty() {
        return;
    }
    let what = if st.timers.get() { "timers" } else { "services" };
    st.count.set_text(&if shown == entries.len() {
        format!("{shown} {what}")
    } else {
        format!("{shown} of {} {what} shown", entries.len())
    });
    st.stack.set_visible_child_name(if shown == 0 { "empty" } else { "list" });
}

fn visible(st: &State, e: &Entry) -> bool {
    let q = st.query.borrow();
    let u = &e.unit;
    let show = match st.show.get() {
        Show::All => true,
        Show::Running => u.running(),
        Show::Failed => u.failed(),
    };
    show && (q.is_empty()
        || u.name.to_lowercase().contains(q.as_str())
        || u.description.to_lowercase().contains(q.as_str())
        || e.timer.as_ref().is_some_and(|t| t.activates.to_lowercase().contains(q.as_str())))
}

fn systemctl(st: &Rc<State>, verb: &str, unit: &str) {
    run_systemctl(st.user.get(), verb, unit, {
        let st = st.clone();
        move || load(&st, true)
    });
}

fn run_systemctl(user: bool, verb: &str, unit: &str, after: impl FnOnce() + 'static) {
    let mut args: Vec<String> = if user {
        vec!["systemctl".into(), "--user".into()]
    } else {
        vec!["pkexec".into(), "systemctl".into(), "--system".into()]
    };
    args.push(verb.into());
    args.push(unit.into());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (verb, unit) = (verb.to_string(), unit.to_string());
    cmd::run_async(&refs, move |r| {
        match r {
            Ok(_) => window::toast(&format!("{}: {verb} done", pretty_name(&unit))),
            Err(e) => window::toast(&format!("Couldn't {verb} {unit}: {e}")),
        }
        after();
    });
}

/// "app-dropbox@autostart.service" -> "dropbox (autostart)"; "foo.service" -> "foo".
fn pretty_name(unit: &str) -> String {
    let u = unit.trim_end_matches(".service");
    match u.strip_prefix("app-").and_then(|s| s.strip_suffix("@autostart")) {
        Some(app) => format!("{} (autostart)", startup::unescape(app)),
        None => startup::unescape(u),
    }
}

/// The verbs that make sense for a unit in its current state.
fn verbs(u: &Unit) -> Vec<(&'static str, &'static str)> {
    let mut v = Vec::new();
    if u.running() {
        v.push(("Restart", "restart"));
        v.push(("Stop", "stop"));
    } else {
        v.push(("Start", "start"));
    }
    match u.file_state.as_str() {
        "enabled" => v.push(("Disable at boot", "disable")),
        "disabled" => v.push(("Enable at boot", "enable")),
        _ => {}
    }
    v
}

/// One line per unit: state dot, name, what it is, memory, start-at-boot, actions.
fn row_content(st: &Rc<State>, e: &Entry) -> gtk::Box {
    let u = &e.unit;
    let row = widgets::hbox(12);
    row.add_css_class("service-row");
    let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    dot.add_css_class("state-dot");
    if u.failed() {
        dot.add_css_class("failed");
    } else if u.running() {
        dot.add_css_class("running");
    }
    dot.set_valign(gtk::Align::Center);
    let state = state_tag(u, e.timer.is_some()).text();
    dot.set_tooltip_text(Some(&state));
    row.append(&dot);
    // Name and description share the row's middle equally, so they line up as columns.
    let cols = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    cols.set_homogeneous(true);
    cols.set_hexpand(true);
    let t = widgets::label(&pretty_name(&u.name), "service-name");
    t.set_ellipsize(pango::EllipsizeMode::End);
    t.set_width_chars(12);
    cols.append(&t);
    row.append(&cols);
    let desc = match &e.timer {
        Some(t) => {
            let now = glib::real_time() as f64 / 1e6;
            let next = t.next.map(|n| format!("next {}", fmt::relative(n, now))).unwrap_or_else(|| "not scheduled".into());
            let last = t.last.map(|l| format!("last {}", fmt::relative(l, now))).unwrap_or_else(|| "never run".into());
            format!("Starts {} · {next} · {last}", pretty_name(&t.activates))
        }
        None => u.description.clone(),
    };
    let d = widgets::label(&desc, "cell-dim");
    d.set_ellipsize(pango::EllipsizeMode::End);
    cols.append(&d);
    row.set_tooltip_text(Some(&format!(
        "{}\n{state}{}",
        u.name,
        if desc.is_empty() { String::new() } else { format!(" · {desc}") }
    )));
    let mem = widgets::label("", "cell-dim");
    mem.add_css_class("tnum");
    mem.set_width_chars(9);
    mem.set_xalign(1.0);
    mem.set_tooltip_text(Some("Memory it holds"));
    row.append(&mem);
    st.mem_labels.borrow_mut().insert(u.name.clone(), mem);
    let boot = widgets::label(if u.file_state.is_empty() { "–" } else { &u.file_state }, "cell-dim");
    boot.set_width_chars(10);
    boot.set_xalign(0.0);
    boot.set_tooltip_text(Some("Starts at boot when enabled"));
    if u.file_state == "enabled" {
        boot.remove_css_class("cell-dim");
    }
    row.append(&boot);

    let menu = gtk::MenuButton::new();
    menu.set_icon_name("view-more-symbolic");
    menu.set_tooltip_text(Some("Actions"));
    menu.set_valign(gtk::Align::Center);
    menu.add_css_class("flat");
    let pop = gtk::Popover::new();
    let actions = widgets::vbox(2);
    for (label, verb) in verbs(u) {
        let b = gtk::Button::with_label(label);
        b.add_css_class("flat");
        let (st2, name, pop2) = (st.clone(), u.name.clone(), pop.clone());
        b.connect_clicked(move |_| {
            pop2.popdown();
            systemctl(&st2, verb, &name);
        });
        actions.append(&b);
    }
    let details = gtk::Button::with_label("Details");
    details.add_css_class("flat");
    let (st2, e2, pop2) = (st.clone(), e.clone(), pop.clone());
    details.connect_clicked(move |_| {
        pop2.popdown();
        page_details(&st2, &e2);
    });
    actions.append(&details);
    let logs = gtk::Button::with_label("View logs");
    logs.add_css_class("flat");
    let log_unit = e.timer.as_ref().map_or(u.name.clone(), |t| t.activates.clone());
    let (user, pop2) = (st.user.get(), pop.clone());
    logs.connect_clicked(move |_| {
        pop2.popdown();
        show_logs(&log_unit, user);
    });
    actions.append(&logs);
    pop.set_child(Some(&actions));
    menu.set_popover(Some(&pop));
    row.append(&menu);
    row
}

/// Details for a row on the page; actions refresh the list when done.
fn page_details(st: &Rc<State>, e: &Entry) {
    let st2 = st.clone();
    show_details(st.user.get(), e, Rc::new(move || load(&st2, true)));
}

/// Open a service's details from elsewhere (the Ctrl+K palette).
pub fn open_unit(unit: Unit, user: bool) {
    window::navigate("services");
    show_details(user, &Entry { unit, timer: None }, Rc::new(|| {}));
}

/// A unit's details: what systemd knows about it right now, and what you can do.
/// `after` runs once an action on it has finished.
fn show_details(user: bool, e: &Entry, after: Rc<dyn Fn()>) {
    let u = &e.unit;
    let (dialog, card) = widgets::dialog(&pretty_name(&u.name), 640);
    if !u.description.is_empty() {
        let d = widgets::label(&u.description, "dim");
        d.set_wrap(true);
        card.append(&d);
    }
    let info = widgets::vbox(0);
    info.add_css_class("graph-card");
    let mut keys = vec!["State", "Main PID", "Memory", "CPU time", "Since", "Restart", "At boot"];
    if e.timer.is_some() {
        keys.extend(["Starts", "Next run", "Last run"]);
    }
    let (flow, v) = widgets::kv_flow(&keys);
    info.append(&flow);
    card.append(&info);
    let file = widgets::label("", "dim");
    file.add_css_class("mono");
    file.set_ellipsize(pango::EllipsizeMode::Middle);
    file.set_selectable(true);
    card.append(&file);
    let exec = widgets::label("", "code-block");
    exec.set_wrap(true);
    exec.set_wrap_mode(pango::WrapMode::WordChar);
    exec.set_selectable(true);
    exec.set_visible(false);
    card.append(&exec);
    v[6].set_text(if u.file_state.is_empty() { "–" } else { &u.file_state });
    if let Some(t) = &e.timer {
        let now = glib::real_time() as f64 / 1e6;
        v[7].set_text(&pretty_name(&t.activates));
        v[8].set_text(&t.next.map(|n| fmt::relative(n, now)).unwrap_or_else(|| "–".into()));
        v[9].set_text(&t.last.map(|l| fmt::relative(l, now)).unwrap_or_else(|| "never".into()));
    }

    let buttons = gtk::FlowBox::new();
    buttons.set_selection_mode(gtk::SelectionMode::None);
    buttons.set_column_spacing(6);
    buttons.set_row_spacing(6);
    buttons.set_max_children_per_line(8);
    for (label, verb) in verbs(u) {
        let b = gtk::Button::with_label(label);
        let (after, name, d) = (after.clone(), u.name.clone(), dialog.clone());
        b.connect_clicked(move |_| {
            let after = after.clone();
            run_systemctl(user, verb, &name, move || after());
            d.close();
        });
        buttons.append(&b);
    }
    let logs = gtk::Button::with_label("View logs");
    let log_unit = e.timer.as_ref().map_or(u.name.clone(), |t| t.activates.clone());
    logs.connect_clicked(move |_| show_logs(&log_unit, user));
    buttons.append(&logs);
    let show_proc = gtk::Button::with_label("Show process");
    show_proc.set_visible(false);
    buttons.append(&show_proc);
    let edit = gtk::Button::with_label(if user { "Edit unit" } else { "Open unit file" });
    edit.set_visible(false);
    buttons.append(&edit);
    let close = gtk::Button::with_label("Close");
    let d = dialog.clone();
    close.connect_clicked(move |_| d.close());
    buttons.append(&close);
    let mut child = buttons.first_child();
    while let Some(c) = child {
        c.set_focusable(false);
        child = c.next_sibling();
    }
    card.append(&buttons);
    dialog.present();

    let name = u.name.clone();
    let (d, name2) = (dialog.clone(), name.clone());
    cmd::background(
        move || startup::unit_props(&name, user),
        move |p| {
            let get = |k: &str| p.get(k).map(String::as_str).unwrap_or("");
            v[0].set_text(&format!("{} ({})", get("ActiveState"), get("SubState")));
            let pid: i32 = get("MainPID").parse().unwrap_or(0);
            v[1].set_text(&if pid > 0 { pid.to_string() } else { "–".into() });
            v[2].set_text(
                &get("MemoryCurrent").parse::<f64>().ok().filter(|m| *m < 1e18).map(fmt::bytes).unwrap_or_else(|| "–".into()),
            );
            v[3].set_text(
                &get("CPUUsageNSec")
                    .parse::<f64>()
                    .ok()
                    .filter(|n| *n < 1e18)
                    .map(|n| fmt::duration(n / 1e9))
                    .unwrap_or_else(|| "–".into()),
            );
            let since = get("ActiveEnterTimestamp");
            v[4].set_text(if since.is_empty() { "–" } else { since });
            v[4].set_tooltip_text(Some(since));
            v[5].set_text(if get("Restart").is_empty() { "–" } else { get("Restart") });
            let path = get("FragmentPath").to_string();
            file.set_text(&path);
            file.set_visible(!path.is_empty());
            // ExecStart reads "{ path=… ; argv[]=the command ; … }".
            let argv =
                get("ExecStart").split("argv[]=").nth(1).and_then(|r| r.split(" ;").next()).unwrap_or("").trim().to_string();
            exec.set_text(&argv);
            exec.set_visible(!argv.is_empty());
            if pid > 0 {
                show_proc.set_visible(true);
                let d = d.clone();
                show_proc.connect_clicked(move |_| {
                    d.close();
                    super::processes::reveal(pid);
                });
            }
            if !path.is_empty() {
                edit.set_visible(true);
                edit.connect_clicked(move |_| {
                    if user && cmd::present("xdg-terminal-exec") {
                        cmd::spawn(&["xdg-terminal-exec", "systemctl", "--user", "edit", "--full", &name2]);
                    } else {
                        cmd::open_in_editor(std::path::Path::new(&path));
                    }
                });
            }
        },
    );
}

/// Open Services with a unit's journal over it (from the Dashboard).
pub fn open_logs(unit: &str, user: bool) {
    window::navigate("services");
    show_logs(unit, user);
}

/// The journal for one unit: newest at the bottom, optionally following live.
fn show_logs(unit: &str, user: bool) {
    let (dialog, card) = widgets::dialog(&format!("Logs · {}", pretty_name(unit)), 860);
    dialog.set_default_height(600);

    let bar = widgets::hbox(8);
    bar.add_css_class("toolbar");
    let level =
        widgets::dropdown(&widgets::opts(&[("", "All messages"), ("warning", "Warnings and worse"), ("err", "Errors only")]), "");
    level.set_tooltip_text(Some("Priority"));
    bar.append(&level);
    let filter = gtk::SearchEntry::new();
    filter.set_placeholder_text(Some("Filter lines"));
    filter.set_hexpand(true);
    bar.append(&filter);
    let follow_box = widgets::hbox(6);
    follow_box.append(&widgets::label("Follow", "dim"));
    let follow = gtk::Switch::new();
    follow.set_valign(gtk::Align::Center);
    follow.set_tooltip_text(Some("Show new lines as they're written"));
    follow_box.append(&follow);
    bar.append(&follow_box);
    card.append(&bar);

    let view = gtk::TextView::new();
    view.set_editable(false);
    view.set_monospace(true);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.add_css_class("code-block");
    let scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&view).build();
    let stack = gtk::Stack::new();
    let spinner = widgets::loading("Reading the journal…");
    spinner.set_halign(gtk::Align::Center);
    spinner.set_valign(gtk::Align::Center);
    stack.add_named(&spinner, Some("loading"));
    stack.add_named(&scroll, Some("text"));
    card.append(&stack);

    let buttons = widgets::hbox(8);
    let copy = gtk::Button::with_label("Copy");
    let terminal = gtk::Button::with_label("Open in terminal");
    terminal.set_visible(cmd::present("xdg-terminal-exec"));
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    let close = gtk::Button::with_label("Close");
    buttons.append(&copy);
    buttons.append(&terminal);
    buttons.append(&spacer);
    buttons.append(&close);
    card.append(&buttons);

    /// Everything read so far, before the text filter.
    struct Log {
        lines: Vec<String>,
        follower: Option<gio::Subprocess>,
    }
    const MAX_LINES: usize = 5000;
    let log = Rc::new(RefCell::new(Log { lines: Vec::new(), follower: None }));
    let scope = if user { "--user" } else { "--system" };
    let unit = unit.to_string();

    let matches = |line: &str, q: &str| q.is_empty() || line.to_lowercase().contains(q);
    let render = {
        let (log, view, filter) = (log.clone(), view.clone(), filter.clone());
        move || {
            let q = filter.text().to_lowercase();
            let lines = &log.borrow().lines;
            let shown: Vec<&str> = lines.iter().map(String::as_str).filter(|l| matches(l, &q)).collect();
            let text = if lines.is_empty() {
                "No log entries.".to_string()
            } else if shown.is_empty() {
                "No lines match the filter.".to_string()
            } else {
                shown.join("\n")
            };
            view.buffer().set_text(&text);
            let mut end = view.buffer().end_iter();
            view.scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0);
        }
    };
    let render: Rc<dyn Fn()> = Rc::new(render);

    let priority = {
        let level = level.clone();
        move || -> Option<&'static str> {
            match level.selected() {
                1 => Some("warning"),
                2 => Some("err"),
                _ => None,
            }
        }
    };
    let priority: Rc<dyn Fn() -> Option<&'static str>> = Rc::new(priority);

    let stop_follow = {
        let log = log.clone();
        move || {
            if let Some(p) = log.borrow_mut().follower.take() {
                p.force_exit();
            }
        }
    };
    let stop_follow: Rc<dyn Fn()> = Rc::new(stop_follow);

    let start_follow = {
        let (log, unit, render, priority, view) = (log.clone(), unit.clone(), render.clone(), priority.clone(), view.clone());
        let filter = filter.clone();
        move || {
            let mut args = vec!["journalctl", scope, "-u", &unit, "-f", "-n", "0", "-o", "short-iso", "--no-pager"];
            let p = priority();
            if let Some(p) = &p {
                args.extend(["-p", p]);
            }
            let os: Vec<&std::ffi::OsStr> = args.iter().map(std::ffi::OsStr::new).collect();
            let Ok(proc) = gio::Subprocess::newv(&os, gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE)
            else {
                window::toast("Couldn't start journalctl");
                return;
            };
            let Some(out) = proc.stdout_pipe() else { return };
            log.borrow_mut().follower = Some(proc.clone());
            let stream = gio::DataInputStream::new(&out);
            let (log, render, view, filter) = (log.clone(), render.clone(), view.clone(), filter.clone());
            glib::spawn_future_local(async move {
                while let Ok(Some(line)) = stream.read_line_utf8_future(glib::Priority::DEFAULT).await {
                    let line = line.to_string();
                    let trimmed = {
                        let mut l = log.borrow_mut();
                        if l.follower.as_ref() != Some(&proc) {
                            break;
                        }
                        let was_empty = l.lines.is_empty();
                        l.lines.push(line.clone());
                        let over = l.lines.len().saturating_sub(MAX_LINES);
                        l.lines.drain(..over);
                        was_empty || over > 0
                    };
                    if trimmed {
                        render();
                    } else if matches(&line, &filter.text().to_lowercase()) {
                        let buf = view.buffer();
                        let mut end = buf.end_iter();
                        buf.insert(&mut end, &format!("\n{line}"));
                        let mut end = buf.end_iter();
                        view.scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0);
                    }
                }
            });
        }
    };
    let start_follow: Rc<dyn Fn()> = Rc::new(start_follow);

    // (Re)read the last 300 lines at the chosen priority, then follow if asked.
    let reload = {
        let (log, unit, render, priority, stack, follow) =
            (log.clone(), unit.clone(), render.clone(), priority.clone(), stack.clone(), follow.clone());
        let (stop_follow, start_follow) = (stop_follow.clone(), start_follow.clone());
        move || {
            stop_follow();
            stack.set_visible_child_name("loading");
            let mut args = vec!["journalctl", scope, "-u", &unit, "-n", "300", "--no-pager", "-o", "short-iso"];
            let p = priority();
            if let Some(p) = &p {
                args.extend(["-p", p]);
            }
            let (log, render, stack, follow, start_follow) =
                (log.clone(), render.clone(), stack.clone(), follow.clone(), start_follow.clone());
            cmd::run_async(&args, move |r| {
                log.borrow_mut().lines = match r {
                    Ok(t) => t.lines().filter(|l| !l.starts_with("-- No entries")).map(String::from).collect(),
                    Err(e) => vec![format!("Couldn't read the journal: {e}")],
                };
                stack.set_visible_child_name("text");
                render();
                if follow.is_active() {
                    start_follow();
                }
            });
        }
    };
    let reload: Rc<dyn Fn()> = Rc::new(reload);

    let r = reload.clone();
    level.connect_selected_notify(move |_| r());
    let r = render.clone();
    filter.connect_search_changed(move |_| r());
    let (start, stop) = (start_follow.clone(), stop_follow.clone());
    follow.connect_active_notify(move |s| if s.is_active() { start() } else { stop() });
    let (log2, filter2) = (log.clone(), filter.clone());
    copy.connect_clicked(move |_| {
        let q = filter2.text().to_lowercase();
        let text: Vec<String> = log2.borrow().lines.iter().filter(|l| matches(l, &q)).cloned().collect();
        crate::actions::copy(&text.join("\n"));
    });
    let unit2 = unit.clone();
    terminal.connect_clicked(move |_| {
        cmd::spawn(&["xdg-terminal-exec", "journalctl", scope, "-u", &unit2, "-f", "-n", "200"]);
    });
    let d = dialog.clone();
    close.connect_clicked(move |_| d.close());
    let stop = stop_follow.clone();
    dialog.connect_close_request(move |_| {
        stop();
        glib::Propagation::Proceed
    });
    dialog.present();
    reload();
}

pub fn build(page: &Page) {
    let p = prefs::get();
    // One row: what, whose, which, then the filter.
    let toolbar = widgets::hbox(8);
    toolbar.add_css_class("toolbar");
    toolbar.add_css_class("table-toolbar");
    let kind_box = widgets::hbox(4);
    let scope = widgets::dropdown(&widgets::opts(&[("user", "Yours"), ("system", "System")]), &p.services_scope);
    scope.add_css_class("compact");
    scope.set_tooltip_text(Some("Your user services, or the system's"));
    let status = widgets::dropdown(
        &widgets::opts(&[("all", "Any state"), ("running", "Running"), ("failed", "Failed")]),
        &p.services_show,
    );
    status.add_css_class("compact");
    status.set_tooltip_text(Some("Show only running or failed units"));
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Filter"));
    search.set_tooltip_text(Some("Filter by name or description"));
    search.set_hexpand(true);
    search.set_width_chars(6);
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.add_css_class("flat");
    refresh.set_tooltip_text(Some("Refresh (it also refreshes by itself every few seconds)"));
    toolbar.append(&kind_box);
    toolbar.append(&scope);
    toolbar.append(&status);
    toolbar.append(&search);
    toolbar.append(&refresh);
    page.body.append(&toolbar);

    // ----- Summary -----
    let states = charts::stack_bar(&["Running", "Failed", "Stopped"], None);
    // Its summary is the list's count ("12 of 106").
    let (states_card, count) = charts::card("State", &states.root);
    let memory = charts::bar_list(4, "No memory reading yet.", false);
    let (memory_card, memory_sum) = charts::card("Using the most memory", &memory.root);
    let summary = charts::row(&[states_card.upcast_ref(), memory_card.upcast_ref()], PAIR);
    summary.add_css_class("services-summary");
    page.body.append(&summary);

    let banner = widgets::banner("", true);
    banner.set_visible(false);
    banner.set_margin_start(20);
    banner.set_margin_end(16);
    banner.set_margin_bottom(12);
    page.body.append(&banner);

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("service-list");
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .vexpand(true)
        .child(&list)
        .build();
    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    let loading = widgets::loading("Reading services…");
    loading.set_halign(gtk::Align::Center);
    loading.set_valign(gtk::Align::Start);
    loading.add_css_class("empty-state");
    stack.add_named(&loading, Some("loading"));
    stack.add_named(&scroll, Some("list"));
    let empty = widgets::label("Nothing matches.", "empty-state");
    empty.set_valign(gtk::Align::Start);
    empty.set_xalign(0.5);
    stack.add_named(&empty, Some("empty"));
    page.body.append(&stack);

    let show = match p.services_show.as_str() {
        "running" => Show::Running,
        "failed" => Show::Failed,
        _ => Show::All,
    };
    let st = Rc::new(State {
        user: Cell::new(p.services_scope != "system"),
        timers: Cell::new(p.services_timers),
        show: Cell::new(show),
        query: RefCell::new(String::new()),
        entries: RefCell::new(HashMap::new()),
        rows: RefCell::new(HashMap::new()),
        mem_labels: RefCell::new(HashMap::new()),
        loading: Cell::new(false),
        list: list.clone(),
        stack: stack.clone(),
        banner,
        count,
        states,
        memory,
        memory_sum,
    });

    let s2 = st.clone();
    list.set_filter_func(move |row| s2.entries.borrow().get(row.widget_name().as_str()).is_some_and(|e| visible(&s2, e)));
    let s2 = st.clone();
    list.connect_row_activated(move |_, row| {
        let e = s2.entries.borrow().get(row.widget_name().as_str()).cloned();
        if let Some(e) = e {
            page_details(&s2, &e);
        }
    });

    /// A row of chips where one is selected; `selected` is the index chosen at start.
    fn chip_group(container: &gtk::Box, items: Vec<Chip>, selected: usize) {
        let buttons: Rc<RefCell<Vec<gtk::Button>>> = Rc::default();
        for (i, (label, act)) in items.into_iter().enumerate() {
            let b = gtk::Button::with_label(label);
            b.add_css_class("chip");
            if i == selected {
                b.add_css_class("selected");
            }
            let bs = buttons.clone();
            b.connect_clicked(move |me| {
                for x in bs.borrow().iter() {
                    x.remove_css_class("selected");
                }
                me.add_css_class("selected");
                act();
            });
            container.append(&b);
            buttons.borrow_mut().push(b);
        }
    }
    // Switching scope or kind starts the list over.
    let reset = |st: &Rc<State>| {
        for row in st.rows.borrow_mut().drain().map(|(_, r)| r) {
            st.list.remove(&row);
        }
        st.mem_labels.borrow_mut().clear();
        st.entries.borrow_mut().clear();
        st.stack.set_visible_child_name("loading");
        load(st, false);
    };
    let kind = |timers: bool| {
        let s = st.clone();
        Rc::new(move || {
            s.timers.set(timers);
            prefs::update(|p| p.services_timers = timers);
            reset(&s);
        }) as Rc<dyn Fn()>
    };
    chip_group(&kind_box, vec![("Services", kind(false)), ("Timers", kind(true))], st.timers.get() as usize);
    let s = st.clone();
    scope.connect_selected_notify(move |d| {
        let user = d.selected() == 0;
        if s.user.get() == user {
            return;
        }
        s.user.set(user);
        prefs::update(|p| p.services_scope = if user { "user" } else { "system" }.into());
        reset(&s);
    });
    let s = st.clone();
    status.connect_selected_notify(move |d| {
        let show = match d.selected() {
            1 => Show::Running,
            2 => Show::Failed,
            _ => Show::All,
        };
        s.show.set(show);
        prefs::update(|p| p.services_show = show.id().into());
        s.list.invalidate_filter();
        refresh_count(&s);
    });

    let s2 = st.clone();
    search.connect_search_changed(move |e| {
        *s2.query.borrow_mut() = e.text().trim().to_lowercase();
        s2.list.invalidate_filter();
        refresh_count(&s2);
    });
    let s2 = st.clone();
    refresh.connect_clicked(move |_| load(&s2, false));

    // Keep states fresh while the page is on screen.
    let weak = Rc::downgrade(&st);
    let page_root = page.root.clone();
    glib::timeout_add_seconds_local(5, move || {
        let Some(st) = weak.upgrade() else { return glib::ControlFlow::Break };
        let shown = page_root.is_mapped() && window::window().is_some_and(|w| !w.is_suspended());
        if shown && window::current() == "services" {
            load(&st, true);
        }
        glib::ControlFlow::Continue
    });
    load(&st, false);
}
