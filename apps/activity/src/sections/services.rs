use crate::sampler::groups::Kind;
use crate::startup::{self, Timer, Unit};
use crate::widgets::{self, Page};
use crate::{charts, cmd, fmt, live, parts, prefs, window};
use gtk::prelude::*;
use gtk::{gio, glib, pango};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// One row: a service, or a timer with the unit it starts.
#[derive(Clone, PartialEq)]
struct Entry {
    unit: Unit,
    timer: Option<Timer>,
}

struct State {
    user: Cell<bool>,
    timers: Cell<bool>,
    query: RefCell<String>,
    entries: RefCell<HashMap<String, Entry>>,
    /// Rows by unit name, kept between refreshes so the list doesn't jump.
    rows: RefCell<HashMap<String, gtk::ListBoxRow>>,
    /// Each row's state line ("Running · 54 MB"), refreshed in place.
    state_labels: RefCell<HashMap<String, gtk::Label>>,
    loading: Cell<bool>,
    list: gtk::ListBox,
    stack: gtk::Stack,
    count: gtk::Label,
    /// The unit shown beside the list.
    chosen: RefCell<String>,
    detail: Detail,
    narrow: Cell<bool>,
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

/// When a failed unit failed, from the alerts' last check.
fn failed_at(unit: &str) -> Option<i64> {
    crate::alerts::failed().into_iter().find(|f| f.unit.name == unit).and_then(|f| f.since)
}

/// "11 Nov 04:13", or "04:13" today.
fn short_when(unix: i64) -> String {
    let (Ok(t), Ok(now)) = (glib::DateTime::from_unix_local(unix), glib::DateTime::now_local()) else { return String::new() };
    let fmt_ = if (t.year(), t.day_of_year()) == (now.year(), now.day_of_year()) { "%H:%M" } else { "%-d %b %H:%M" };
    t.format(fmt_).map(|s| s.to_string()).unwrap_or_default()
}

/// What a row says about its state, in words.
fn state_line(e: &Entry, mem: Option<u64>) -> (String, &'static str) {
    let u = &e.unit;
    if u.failed() {
        let when = failed_at(&u.name).map(|t| format!(" {}", short_when(t))).unwrap_or_default();
        return (format!("Failed{when}"), "failed");
    }
    if let Some(t) = &e.timer {
        let now = glib::real_time() as f64 / 1e6;
        return match t.next {
            Some(n) => (format!("Next {}", fmt::relative(n, now)), "running"),
            None => ("Not scheduled".into(), ""),
        };
    }
    if u.running() {
        let m = mem.filter(|m| *m > 0).map(|m| format!(" · {}", fmt::bytes(m as f64))).unwrap_or_default();
        let what = if u.sub == "running" || u.sub.is_empty() { "Running".to_string() } else { capital(&u.sub) };
        (format!("{what}{m}"), "running")
    } else {
        ("Stopped".into(), "")
    }
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// Failed first, then running, then stopped; by name within each.
fn rank(e: &Entry) -> (u8, String) {
    let u = &e.unit;
    (
        if u.failed() {
            0
        } else if u.running() {
            1
        } else {
            2
        },
        title(e).to_lowercase(),
    )
}

/// A service by what it does: its description, or its name when it has none.
fn title(e: &Entry) -> String {
    let d = e.timer.as_ref().map_or(e.unit.description.as_str(), |t| t.unit.description.as_str());
    if d.is_empty() || d == e.unit.name { pretty_name(&e.unit.name) } else { d.to_string() }
}

fn refresh_states(st: &State) {
    let mem = unit_memory(st.user.get());
    let entries = st.entries.borrow();
    for (name, label) in st.state_labels.borrow().iter() {
        let Some(e) = entries.get(name) else { continue };
        let (text, _) = state_line(e, mem.get(name).copied());
        if label.text() != text {
            label.set_text(&text);
        }
    }
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

/// Bring the list in line with `entries`: update changed rows in place, add new
/// ones, drop gone ones, and keep them failed, running, stopped.
fn populate(st: &Rc<State>, mut entries: Vec<Entry>) {
    entries.sort_by_key(rank);
    {
        let mut rows = st.rows.borrow_mut();
        let old = st.entries.borrow();
        let names: std::collections::HashSet<&str> = entries.iter().map(|e| e.unit.name.as_str()).collect();
        rows.retain(|name, row| {
            let keep = names.contains(name.as_str());
            if !keep {
                st.list.remove(row);
                st.state_labels.borrow_mut().remove(name);
            }
            keep
        });
        let mut moved = false;
        for (i, e) in entries.iter().enumerate() {
            match rows.get(&e.unit.name) {
                Some(row) => {
                    if old.get(&e.unit.name) != Some(e) {
                        row.set_child(Some(&row_content(st, e)));
                    }
                    if row.index() != i as i32 {
                        moved = true;
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
        // A unit that changed state moves to its place.
        if moved {
            for e in entries.iter() {
                if let Some(row) = rows.get(&e.unit.name) {
                    st.list.remove(row);
                    st.list.append(row);
                }
            }
        }
    }
    let running = entries.iter().filter(|e| e.unit.running()).count();
    let failed = entries.iter().filter(|e| e.unit.failed()).count();
    let stopped = entries.len().saturating_sub(running + failed);
    *st.entries.borrow_mut() = entries.iter().cloned().map(|e| (e.unit.name.clone(), e)).collect();
    st.list.invalidate_filter();
    let what = if st.timers.get() { "waiting" } else { "running" };
    st.count.set_text(&format!("{running} {what} · {failed} failed · {stopped} stopped"));
    refresh_states(st);
    let shown = entries.iter().filter(|e| visible(st, e)).count();
    st.stack.set_visible_child_name(if shown == 0 { "empty" } else { "list" });
    // The first time, the one that failed (or the first) is chosen.
    let chosen = st.chosen.borrow().clone();
    match st.entries.borrow().get(&chosen) {
        Some(e) => {
            let e = e.clone();
            st.detail.refresh(&e);
        }
        None => {
            if let Some(e) = entries.iter().find(|e| visible(st, e)) {
                if let Some(row) = st.rows.borrow().get(&e.unit.name) {
                    st.list.select_row(Some(row));
                }
                choose(st, e);
            }
        }
    }
}

fn visible(st: &State, e: &Entry) -> bool {
    let q = st.query.borrow();
    let u = &e.unit;
    q.is_empty()
        || u.name.to_lowercase().contains(q.as_str())
        || u.description.to_lowercase().contains(q.as_str())
        || e.timer.as_ref().is_some_and(|t| t.activates.to_lowercase().contains(q.as_str()))
}

fn choose(st: &Rc<State>, e: &Entry) {
    *st.chosen.borrow_mut() = e.unit.name.clone();
    st.detail.show(st, e);
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

/// A row: its state dot, what it does with its unit name under it, and its state in words.
fn row_content(st: &Rc<State>, e: &Entry) -> gtk::Box {
    let u = &e.unit;
    let row = widgets::hbox(10);
    row.add_css_class("service-row");
    let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    dot.add_css_class("state-dot");
    let (text, class) = state_line(e, None);
    if !class.is_empty() {
        dot.add_css_class(class);
    }
    dot.set_valign(gtk::Align::Center);
    row.append(&dot);
    let names = widgets::vbox(1);
    names.set_hexpand(true);
    let t = widgets::label(&title(e), "service-title");
    t.set_xalign(0.0);
    t.set_ellipsize(pango::EllipsizeMode::End);
    let n = widgets::label(u.name.trim_end_matches(".service").trim_end_matches(".timer"), "service-unit");
    n.set_xalign(0.0);
    n.set_ellipsize(pango::EllipsizeMode::Middle);
    names.append(&t);
    names.append(&n);
    row.append(&names);
    let state = widgets::label(&text, "service-state");
    state.add_css_class("tnum");
    if !class.is_empty() {
        state.add_css_class(class);
    }
    state.set_xalign(1.0);
    row.append(&state);
    st.state_labels.borrow_mut().insert(u.name.clone(), state);
    row.set_tooltip_text(Some(&u.name));
    row
}

// ---------- Beside the list ----------

struct Detail {
    root: gtk::Box,
    head: parts::Head,
    state: charts::Tile,
    ran: charts::Tile,
    memory: charts::Tile,
    actions: gtk::Box,
    log: LogPanel,
    /// Which unit's facts were last asked for, so a late answer for another is dropped.
    asked: Rc<RefCell<String>>,
}

fn detail() -> Detail {
    let root = widgets::vbox(12);
    root.add_css_class("service-detail");
    root.set_hexpand(true);
    let head = parts::head("", "");
    root.append(&head.root);
    let state = charts::tile("State");
    let ran = charts::tile("Ran for");
    let memory = charts::tile("Memory");
    root.append(&parts::readings(&[&state, &ran, &memory]));
    let actions = widgets::hbox(8);
    actions.add_css_class("service-actions");
    root.append(&actions);
    let log = log_panel();
    root.append(&log.root);
    Detail { root, head, state, ran, memory, actions, log, asked: Rc::default() }
}

/// systemd's monotonic µs as seconds, when set.
fn mono(p: &HashMap<String, String>, k: &str) -> Option<f64> {
    p.get(k).and_then(|v| v.parse::<f64>().ok()).filter(|v| *v > 0.0).map(|v| v / 1e6)
}

impl Detail {
    fn show(&self, st: &Rc<State>, e: &Entry) {
        let user = st.user.get();
        let log_unit = e.timer.as_ref().map_or(e.unit.name.clone(), |t| t.activates.clone());
        self.log.open(&log_unit, user);
        while let Some(c) = self.actions.first_child() {
            self.actions.remove(&c);
        }
        let u = &e.unit;
        let act = |label: &str, icon: &str, verb: &'static str, class: &str| {
            let b = if icon.is_empty() { gtk::Button::with_label(label) } else { widgets::labeled_button(icon, label) };
            if !class.is_empty() {
                b.add_css_class(class);
            }
            let (st, name) = (st.clone(), u.name.clone());
            b.connect_clicked(move |_| systemctl(&st, verb, &name));
            self.actions.append(&b);
        };
        if u.running() {
            act("Restart", "view-refresh-symbolic", "restart", "suggested-action");
            act("Stop", "media-playback-stop-symbolic", "stop", "");
        } else {
            act("Start", "media-playback-start-symbolic", "start", "suggested-action");
        }
        match u.file_state.as_str() {
            "enabled" => act("Don't start it", "", "disable", "flat"),
            "disabled" => act("Start it at login", "", "enable", "flat"),
            _ => {}
        }
        let spacer = widgets::hbox(0);
        spacer.set_hexpand(true);
        self.actions.append(&spacer);
        let more = widgets::labeled_button("document-properties-symbolic", "Details");
        more.add_css_class("flat");
        let (st2, e2) = (st.clone(), e.clone());
        more.connect_clicked(move |_| page_details(&st2, &e2));
        self.actions.append(&more);
        self.refresh(e);
    }

    fn refresh(&self, e: &Entry) {
        let u = &e.unit;
        self.head.title.set_text(&title(e));
        let starts = match u.file_state.as_str() {
            "enabled" => "starts at login",
            "disabled" => "doesn't start by itself",
            "static" => "starts when something needs it",
            "generated" => "made by the system at boot",
            "transient" => "started for this session",
            "" => "",
            other => other,
        };
        let mut line = vec![u.name.clone()];
        if !starts.is_empty() {
            line.push(starts.to_string());
        }
        if let Some(t) = &e.timer {
            line.push(format!("starts {}", pretty_name(&t.activates)));
        }
        self.head.line.set_text(&line.join(" · "));
        let (text, class) = state_line(e, None);
        let word = text.split(' ').next().unwrap_or("").to_string();
        self.state.value.set_text(&word);
        for c in ["failed", "running"] {
            if c == class {
                self.state.value.add_css_class(c);
            } else {
                self.state.value.remove_css_class(c);
            }
        }
        let name = u.name.clone();
        *self.asked.borrow_mut() = name.clone();
        let (asked, state, ran, memory) =
            (self.asked.clone(), self.state.sub.clone(), self.ran.clone_parts(), self.memory.clone_parts());
        let running = u.running();
        let failed = u.failed();
        let scope_user = self.log.user.get();
        cmd::background(
            move || startup::unit_props(&name, scope_user),
            move |p| {
                if *asked.borrow() != p.get("Id").cloned().unwrap_or_else(|| asked.borrow().clone()) {
                    return;
                }
                let since = p.get("StateChangeTimestamp").map(String::as_str).unwrap_or("");
                state.set_text(
                    &since.split(' ').skip(1).take(2).collect::<Vec<_>>().join(" ").chars().take(16).collect::<String>(),
                );
                let now = glib::monotonic_time() as f64 / 1e6;
                let (rv, rs) = ran;
                if running {
                    rs.set_text("so far");
                    rv.set_text(
                        &mono(&p, "ActiveEnterTimestampMonotonic").map(|t| fmt::duration(now - t)).unwrap_or_else(|| "–".into()),
                    );
                } else {
                    let span = mono(&p, "ExecMainStartTimestampMonotonic")
                        .zip(mono(&p, "ExecMainExitTimestampMonotonic"))
                        .map(|(a, b)| b - a);
                    rs.set_text(if failed { "before it failed" } else { "last time" });
                    rv.set_text(
                        &span
                            .filter(|s| *s >= 0.0)
                            .map(|s| if s < 1.0 { "under 1 s".into() } else { fmt::duration(s) })
                            .unwrap_or_else(|| "–".into()),
                    );
                }
                let (mv, ms) = memory;
                match p.get("MemoryCurrent").and_then(|m| m.parse::<f64>().ok()).filter(|m| *m < 1e18) {
                    Some(m) => {
                        mv.set_text(&fmt::bytes(m));
                        ms.set_text("");
                    }
                    None => {
                        mv.set_text("–");
                        ms.set_text("not running");
                    }
                }
            },
        );
    }
}

trait TileParts {
    fn clone_parts(&self) -> (gtk::Label, gtk::Label);
}

impl TileParts for charts::Tile {
    fn clone_parts(&self) -> (gtk::Label, gtk::Label) {
        (self.value.clone(), self.sub.clone())
    }
}

// ---------- The log beside it ----------

struct LogPanel {
    root: gtk::Box,
    view: gtk::TextView,
    unit: Rc<RefCell<String>>,
    user: Rc<Cell<bool>>,
    /// 0 all, 1 warnings and worse, 2 errors.
    level: Rc<Cell<u8>>,
    follow: gtk::Switch,
    follower: Rc<RefCell<Option<gio::Subprocess>>>,
}

/// A journal line: when, how bad (syslog priority), what.
fn journal_line(json: &str) -> Option<(String, u8, String)> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let msg = match &v["MESSAGE"] {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(bytes) => {
            String::from_utf8_lossy(&bytes.iter().filter_map(|b| b.as_u64().map(|b| b as u8)).collect::<Vec<_>>()).to_string()
        }
        _ => return None,
    };
    let prio = v["PRIORITY"].as_str().and_then(|p| p.parse().ok()).unwrap_or(6);
    let us: i64 = v["__REALTIME_TIMESTAMP"].as_str().and_then(|t| t.parse().ok()).unwrap_or(0);
    let when = glib::DateTime::from_unix_local(us / 1_000_000)
        .ok()
        .and_then(|t| t.format("%H:%M:%S").ok())
        .map(|s| s.to_string())
        .unwrap_or_default();
    Some((when, prio, msg))
}

fn log_panel() -> LogPanel {
    let root = widgets::vbox(8);
    root.set_vexpand(true);
    let bar = widgets::hbox(10);
    bar.add_css_class("service-log-bar");
    bar.append(&widgets::label("Log", "graph-card-title"));
    let level = Rc::new(Cell::new(0u8));
    let view = gtk::TextView::new();
    let unit: Rc<RefCell<String>> = Rc::default();
    let user = Rc::new(Cell::new(true));
    let follow = gtk::Switch::new();
    let follower: Rc<RefCell<Option<gio::Subprocess>>> = Rc::default();
    let panel_parts = (view.clone(), unit.clone(), user.clone(), level.clone(), follow.clone(), follower.clone());
    let seg = widgets::segmented(&widgets::opts(&[("0", "All"), ("1", "Warnings"), ("2", "Errors")]), "0", move |v| {
        let (view, unit, user, level, follow, follower) = panel_parts.clone();
        level.set(v.parse().unwrap_or(0));
        read_log(&view, &unit.borrow(), user.get(), level.get(), &follow, &follower);
    });
    seg.add_css_class("small");
    bar.append(&seg);
    let spacer = widgets::hbox(0);
    spacer.set_hexpand(true);
    bar.append(&spacer);
    bar.append(&widgets::label("Follow", "dim"));
    follow.set_active(true);
    follow.set_valign(gtk::Align::Center);
    follow.set_tooltip_text(Some("Show new lines as they're written"));
    bar.append(&follow);
    root.append(&bar);
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_monospace(true);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.add_css_class("code-block");
    view.add_css_class("service-log");
    let buf = view.buffer();
    let p = crate::theme::palette();
    buf.create_tag(Some("time"), &[("foreground", &p.dim_text.as_str())]);
    buf.create_tag(Some("warn"), &[("foreground", &charts::swatch("amber").to_hex().as_str())]);
    buf.create_tag(Some("err"), &[("foreground", &p.danger.as_str())]);
    let scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&view).build();
    scroll.add_css_class("service-log-scroll");
    root.append(&scroll);
    {
        let (view, unit, user, level, follower) = (view.clone(), unit.clone(), user.clone(), level.clone(), follower.clone());
        follow.connect_active_notify(move |f| {
            if f.is_active() {
                start_follow(&view, &unit.borrow(), user.get(), level.get(), &follower);
            } else if let Some(p) = follower.borrow_mut().take() {
                p.force_exit();
            }
        });
    }
    let fl = follower.clone();
    root.connect_unmap(move |_| {
        if let Some(p) = fl.borrow_mut().take() {
            p.force_exit();
        }
    });
    LogPanel { root, view, unit, user, level, follow, follower }
}

impl LogPanel {
    fn open(&self, unit: &str, user: bool) {
        *self.unit.borrow_mut() = unit.to_string();
        self.user.set(user);
        read_log(&self.view, unit, user, self.level.get(), &self.follow, &self.follower);
    }
}

fn append_line(buf: &gtk::TextBuffer, (when, prio, msg): &(String, u8, String), first: bool) {
    let mut end = buf.end_iter();
    if !first {
        buf.insert(&mut end, "\n");
    }
    buf.insert_with_tags_by_name(&mut end, &format!("{when}  "), &["time"]);
    match prio {
        0..=3 => buf.insert_with_tags_by_name(&mut end, msg, &["err"]),
        4 => buf.insert_with_tags_by_name(&mut end, msg, &["warn"]),
        _ => buf.insert(&mut end, msg),
    }
}

fn prio_arg(level: u8) -> Option<&'static str> {
    match level {
        1 => Some("warning"),
        2 => Some("err"),
        _ => None,
    }
}

/// The last 200 lines, then new ones as they come while Follow is on.
fn read_log(
    view: &gtk::TextView,
    unit: &str,
    user: bool,
    level: u8,
    follow: &gtk::Switch,
    follower: &Rc<RefCell<Option<gio::Subprocess>>>,
) {
    if let Some(p) = follower.borrow_mut().take() {
        p.force_exit();
    }
    let buf = view.buffer();
    buf.set_text("");
    if unit.is_empty() {
        return;
    }
    let scope = if user { "--user" } else { "--system" };
    let mut args = vec!["journalctl", scope, "-u", unit, "-n", "200", "--no-pager", "-o", "json"];
    if let Some(p) = prio_arg(level) {
        args.extend(["-p", p]);
    }
    let (view, unit, follow, follower) = (view.clone(), unit.to_string(), follow.clone(), follower.clone());
    cmd::run_async(&args, move |r| {
        let buf = view.buffer();
        let lines: Vec<(String, u8, String)> = r.map(|t| t.lines().filter_map(journal_line).collect()).unwrap_or_default();
        if lines.is_empty() {
            buf.set_text("No log entries.");
        }
        for (i, l) in lines.iter().enumerate() {
            append_line(&buf, l, i == 0);
        }
        let mut end = buf.end_iter();
        view.scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0);
        if follow.is_active() {
            start_follow(&view, &unit, user, level, &follower);
        }
    });
}

fn start_follow(view: &gtk::TextView, unit: &str, user: bool, level: u8, follower: &Rc<RefCell<Option<gio::Subprocess>>>) {
    if let Some(p) = follower.borrow_mut().take() {
        p.force_exit();
    }
    if unit.is_empty() {
        return;
    }
    let scope = if user { "--user" } else { "--system" };
    let mut args = vec!["journalctl", scope, "-u", unit, "-f", "-n", "0", "-o", "json", "--no-pager"];
    if let Some(p) = prio_arg(level) {
        args.extend(["-p", p]);
    }
    let os: Vec<&std::ffi::OsStr> = args.iter().map(std::ffi::OsStr::new).collect();
    let Ok(proc) = gio::Subprocess::newv(&os, gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE) else {
        return;
    };
    let Some(out) = proc.stdout_pipe() else { return };
    *follower.borrow_mut() = Some(proc.clone());
    let stream = gio::DataInputStream::new(&out);
    let (view, follower) = (view.clone(), follower.clone());
    glib::spawn_future_local(async move {
        while let Ok(Some(line)) = stream.read_line_utf8_future(glib::Priority::DEFAULT).await {
            if follower.borrow().as_ref() != Some(&proc) {
                break;
            }
            let Some(l) = journal_line(&line) else { continue };
            let buf = view.buffer();
            let first = buf.char_count() == 0 || buf.text(&buf.start_iter(), &buf.end_iter(), false) == "No log entries.";
            if first {
                buf.set_text("");
            }
            append_line(&buf, &l, first);
            let mut end = buf.end_iter();
            view.scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0);
        }
    });
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
    page.fill();
    let p = prefs::get();
    // ----- Yours, the system's, or timers; a filter; how many -----
    let toolbar = widgets::hbox(8);
    toolbar.add_css_class("proc-toolbar");
    let current = if p.services_timers {
        "timers"
    } else if p.services_scope == "system" {
        "system"
    } else {
        "user"
    };
    let search = gtk::SearchEntry::new();
    search.add_css_class("proc-filter");
    search.set_placeholder_text(Some("Filter services"));
    search.set_max_width_chars(28);
    if let Some(icon) = search.first_child().and_downcast::<gtk::Image>() {
        icon.set_icon_name(Some("activity-filter-symbolic"));
    }
    let count = widgets::label("", "proc-totals");
    count.add_css_class("tnum");
    count.set_ellipsize(pango::EllipsizeMode::End);
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.add_css_class("flat");
    refresh.set_tooltip_text(Some("Refresh (it also refreshes by itself every few seconds)"));

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
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
    let side = widgets::vbox(0);
    side.add_css_class("service-side");
    side.append(&stack);

    let st = Rc::new(State {
        user: Cell::new(p.services_scope != "system"),
        timers: Cell::new(p.services_timers),
        query: RefCell::new(String::new()),
        entries: RefCell::new(HashMap::new()),
        rows: RefCell::new(HashMap::new()),
        state_labels: RefCell::new(HashMap::new()),
        loading: Cell::new(false),
        list: list.clone(),
        stack: stack.clone(),
        count: count.clone(),
        chosen: RefCell::new(String::new()),
        detail: detail(),
        narrow: Cell::new(false),
    });

    // Switching scope or kind starts the list over.
    let reset = |st: &Rc<State>| {
        for row in st.rows.borrow_mut().drain().map(|(_, r)| r) {
            st.list.remove(&row);
        }
        st.state_labels.borrow_mut().clear();
        st.entries.borrow_mut().clear();
        st.chosen.borrow_mut().clear();
        st.stack.set_visible_child_name("loading");
        load(st, false);
    };
    let s = st.clone();
    let switch =
        widgets::segmented(&widgets::opts(&[("user", "Yours"), ("system", "System"), ("timers", "Timers")]), current, move |v| {
            let (user, timers) = match v.as_str() {
                "system" => (false, false),
                "timers" => (true, true),
                _ => (true, false),
            };
            s.user.set(user);
            s.timers.set(timers);
            prefs::update(|p| {
                p.services_scope = if user { "user" } else { "system" }.into();
                p.services_timers = timers;
            });
            reset(&s);
        });
    toolbar.append(&switch);
    toolbar.append(&search);
    let spacer = widgets::hbox(0);
    spacer.set_hexpand(true);
    toolbar.append(&spacer);
    toolbar.append(&count);
    toolbar.append(&refresh);
    page.body.append(&toolbar);

    let cols = widgets::hbox(0);
    cols.set_vexpand(true);
    cols.append(&side);
    cols.append(&st.detail.root);
    page.body.append(&cols);

    let s2 = st.clone();
    list.set_filter_func(move |row| s2.entries.borrow().get(row.widget_name().as_str()).is_some_and(|e| visible(&s2, e)));
    let s2 = st.clone();
    list.connect_row_selected(move |_, row| {
        let Some(row) = row else { return };
        let e = s2.entries.borrow().get(row.widget_name().as_str()).cloned();
        if let Some(e) = e
            && *s2.chosen.borrow() != e.unit.name
        {
            choose(&s2, &e);
        }
    });
    // In a narrow window the list is alone; a row opens the service on its own.
    let s2 = st.clone();
    list.connect_row_activated(move |_, row| {
        if !s2.narrow.get() {
            return;
        }
        let e = s2.entries.borrow().get(row.widget_name().as_str()).cloned();
        if let Some(e) = e {
            let log_unit = e.timer.as_ref().map_or(e.unit.name.clone(), |t| t.activates.clone());
            show_logs(&log_unit, s2.user.get());
        }
    });
    let fit = {
        let (st, side) = (st.clone(), side.clone());
        move |b: &gtk::Box| {
            let narrow = b.has_css_class("narrow");
            st.narrow.set(narrow);
            st.detail.root.set_visible(!narrow);
            side.set_hexpand(narrow);
        }
    };
    fit(&page.body);
    page.body.connect_css_classes_notify(fit);

    let s2 = st.clone();
    search.connect_search_changed(move |e| {
        *s2.query.borrow_mut() = e.text().trim().to_lowercase();
        s2.list.invalidate_filter();
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
    let s2 = st.clone();
    live::on_tick(&list, move |_| refresh_states(&s2));
    load(&st, false);
}
