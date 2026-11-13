//! The process table. One stable object per pid lives in a `gio::ListStore`;
//! every tick we update their data in Rust, work out order, tree depth and
//! visibility ourselves, and let GTK's sort/filter models follow. Selection and
//! scroll position survive updates because the objects never change identity.

use super::top::{self, kind_name};
use crate::actions::{self, Signal};
use crate::sampler::Snapshot;
use crate::sampler::groups::{Group, Kind};
use crate::sampler::procs::Proc;
use crate::widgets::{self, Page};
use crate::{apps, fmt, graph, live, prefs, window};
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, gio, glib, pango};
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

// ---------- Row data ----------

#[derive(Debug, Clone, Default)]
pub struct Row {
    pub p: Proc,
    pub icon: String,
    pub title: String,
    pub address: String,
    // Computed per tick for the current view.
    pub depth: u32,
    pub has_children: bool,
    pub expanded: bool,
    pub visible: bool,
    pub order: u32,
    /// Summed over the subtree, shown while a group is collapsed.
    pub agg: Option<Agg>,
    /// Set on an app's or service's row in grouped All; its `p` describes the group.
    pub group: Option<GroupRow>,
}

/// An app's or service's processes, under one row.
#[derive(Debug, Clone, Default)]
pub struct GroupRow {
    /// The cgroup path, which identifies the group from one tick to the next.
    pub path: String,
    pub unit: String,
    pub kind: Kind,
    pub pids: Vec<i32>,
    pub leader: i32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Agg {
    pub cpu: f64,
    pub mem: u64,
    pub disk: f64,
    pub gpu: f64,
    pub threads: u32,
    pub count: u32,
}

impl Row {
    fn cpu(&self) -> f64 {
        self.agg.map_or(self.p.cpu, |a| a.cpu)
    }
    fn mem(&self) -> u64 {
        self.agg.map_or(self.p.rss, |a| a.mem)
    }
    fn disk(&self) -> f64 {
        self.agg.map_or(self.p.read_bps + self.p.write_bps, |a| a.disk)
    }
    fn gpu(&self) -> f64 {
        self.agg.map_or(self.p.gpu, |a| a.gpu)
    }
    fn threads(&self) -> u32 {
        self.agg.map_or(self.p.threads, |a| a.threads)
    }
    /// The process this row stands for: a group's leader, or its own.
    fn pid(&self) -> i32 {
        self.group.as_ref().map_or(self.p.pid, |g| g.leader)
    }
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ProcObject {
        pub row: RefCell<Row>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ProcObject {
        const NAME: &'static str = "TasksProcObject";
        type Type = super::ProcObject;
    }

    impl ObjectImpl for ProcObject {}
}

glib::wrapper! {
    pub struct ProcObject(ObjectSubclass<imp::ProcObject>);
}

impl ProcObject {
    fn new(row: Row) -> Self {
        let o: Self = glib::Object::new();
        *o.imp().row.borrow_mut() = row;
        o
    }
    pub fn row(&self) -> std::cell::Ref<'_, Row> {
        self.imp().row.borrow()
    }
    fn row_mut(&self) -> std::cell::RefMut<'_, Row> {
        self.imp().row.borrow_mut()
    }
}

// ---------- Columns ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Col {
    Name,
    Pid,
    User,
    Cpu,
    Mem,
    Disk,
    Gpu,
    Threads,
    State,
}

const COLUMNS: &[(Col, &str, &str, i32)] = &[
    (Col::Name, "name", "Name", 360),
    (Col::Pid, "pid", "PID", 78),
    (Col::User, "user", "User", 84),
    // Wide enough for the totals in their titles ("CPU 23%").
    (Col::Cpu, "cpu", "CPU", 80),
    (Col::Mem, "mem", "Memory", 108),
    (Col::Disk, "disk", "Disk", 100),
    (Col::Gpu, "gpu", "GPU", 76),
    (Col::Threads, "threads", "Threads", 72),
    (Col::State, "state", "State", 88),
];

fn state_name(c: char) -> &'static str {
    match c {
        'R' => "Running",
        'S' => "Sleeping",
        'D' => "Waiting on disk",
        'T' => "Stopped",
        't' => "Traced",
        'Z' => "Zombie",
        'I' => "Idle",
        'X' => "Dead",
        _ => "Unknown",
    }
}

fn compare(col: Col, a: &Row, b: &Row) -> Ordering {
    match col {
        Col::Name => a.p.name.to_lowercase().cmp(&b.p.name.to_lowercase()).then(a.p.pid.cmp(&b.p.pid)),
        Col::Pid => a.pid().cmp(&b.pid()),
        Col::User => a.p.user.cmp(&b.p.user).then(a.p.pid.cmp(&b.p.pid)),
        Col::Cpu => a.cpu().total_cmp(&b.cpu()),
        Col::Mem => a.mem().cmp(&b.mem()),
        Col::Disk => a.disk().total_cmp(&b.disk()),
        Col::Gpu => a.gpu().total_cmp(&b.gpu()),
        Col::Threads => a.threads().cmp(&b.threads()),
        Col::State => a.p.state.cmp(&b.p.state),
    }
}

// ---------- Page state ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Apps,
    All,
    Tree,
    Mine,
}

impl View {
    fn id(self) -> &'static str {
        match self {
            View::Apps => "apps",
            View::All => "all",
            View::Tree => "tree",
            View::Mine => "mine",
        }
    }
    fn from_id(s: &str) -> View {
        match s {
            "apps" => View::Apps,
            "tree" => View::Tree,
            "mine" => View::Mine,
            _ => View::All,
        }
    }
}

struct State {
    store: gio::ListStore,
    objects: HashMap<i32, ProcObject>,
    filter: gtk::CustomFilter,
    sorter: gtk::CustomSorter,
    selection: gtk::MultiSelection,
    view_widget: gtk::ColumnView,
    scroll: gtk::ScrolledWindow,
    columns: Vec<(Col, gtk::ColumnViewColumn)>,
    view: View,
    query: String,
    /// Apps view: groups the user opened. Tree view: branches the user closed.
    opened: HashSet<i32>,
    closed: HashSet<i32>,
    /// All: each app's or service's processes under one row.
    grouped: bool,
    group_chip: gtk::Button,
    /// Group rows' ids (negative, so never a pid), kept by cgroup path.
    group_ids: HashMap<String, i32>,
    next_group_id: i32,
    /// Groups the user opened, by cgroup path.
    opened_groups: HashSet<String>,
    bound: HashMap<usize, (ProcObject, Col, gtk::Widget)>,
    icons: HashMap<(i32, String), String>,
    search: gtk::SearchEntry,
    chips: Vec<(View, gtk::Button)>,
    pending_reveal: Option<i32>,
    details: Option<Details>,
    my_uid: u32,
    /// The table card, or the "nothing matches" message.
    table: gtk::Stack,
    empty: gtk::Label,
    /// Holds the table and the details panel, stacked or side by side.
    split: gtk::Box,
}

thread_local! {
    static STATE: RefCell<Option<Rc<RefCell<State>>>> = const { RefCell::new(None) };
}

fn state() -> Option<Rc<RefCell<State>>> {
    STATE.with(|s| s.borrow().clone())
}

// ---------- Cells ----------

fn num_label() -> gtk::Label {
    let l = gtk::Label::new(None);
    l.add_css_class("cell-num");
    l.set_xalign(1.0);
    l
}

fn setup_cell(col: Col) -> gtk::Widget {
    match col {
        Col::Name => {
            let b = widgets::hbox(6);
            let indent = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            let expander = gtk::Button::from_icon_name("pan-end-symbolic");
            expander.add_css_class("tree-expander");
            expander.set_valign(gtk::Align::Center);
            expander.set_focusable(false);
            let icon = gtk::Image::new();
            icon.set_pixel_size(16);
            let name = widgets::label("", "");
            name.set_ellipsize(pango::EllipsizeMode::End);
            name.set_hexpand(true);
            let tag = widgets::tag("", "");
            b.append(&indent);
            b.append(&expander);
            b.append(&icon);
            b.append(&name);
            b.append(&tag);
            b.upcast()
        }
        Col::User | Col::State => {
            let l = widgets::label("", "cell-dim");
            l.set_ellipsize(pango::EllipsizeMode::End);
            l.upcast()
        }
        _ => num_label().upcast(),
    }
}

thread_local! {
    /// Total RAM, for scaling the memory column's shading.
    static MEM_TOTAL: Cell<u64> = const { Cell::new(0) };
}

const HEAT: [&str; 5] = ["heat-1", "heat-2", "heat-3", "heat-4", "heat-5"];

/// How busy a value is, 0 (idle) to 5, for shading its cell.
fn heat_level(col: Col, v: f64, mem_total: u64) -> usize {
    let steps: [f64; 5] = match col {
        Col::Cpu | Col::Gpu => [1.0, 5.0, 15.0, 30.0, 60.0],
        // Share of RAM, in percent.
        Col::Mem => [1.0, 3.0, 8.0, 15.0, 30.0],
        Col::Disk => [64e3, 1e6, 10e6, 50e6, 200e6],
        _ => return 0,
    };
    let v = if col == Col::Mem { if mem_total > 0 { v / mem_total as f64 * 100.0 } else { 0.0 } } else { v };
    steps.iter().filter(|s| v >= **s).count()
}

fn set_heat(l: &gtk::Label, level: usize, zero: bool) {
    for (i, class) in HEAT.iter().enumerate() {
        if i + 1 == level {
            l.add_css_class(class);
        } else {
            l.remove_css_class(class);
        }
    }
    if zero {
        l.add_css_class("cell-dim");
    } else {
        l.remove_css_class("cell-dim");
    }
}

fn render(col: Col, row: &Row, w: &gtk::Widget, view: View, tree: bool) {
    match col {
        Col::Name => {
            let Some(indent) = w.first_child() else { return };
            let Some(expander) = indent.next_sibling() else { return };
            let Some(icon) = expander.next_sibling().and_downcast::<gtk::Image>() else { return };
            let Some(name) = icon.next_sibling().and_downcast::<gtk::Label>() else { return };
            let Some(tag) = name.next_sibling().and_downcast::<gtk::Label>() else { return };
            indent.set_size_request((row.depth * 18) as i32, -1);
            expander.set_visible(tree && row.has_children);
            expander.set_opacity(if tree && row.has_children { 1.0 } else { 0.0 });
            if let Some(b) = expander.downcast_ref::<gtk::Button>() {
                b.set_icon_name(if row.expanded { "pan-down-symbolic" } else { "pan-end-symbolic" });
            }
            if row.icon.is_empty() {
                icon.clear();
            } else {
                icon.set_icon_name(Some(&row.icon));
            }
            let text = if view == View::Apps && row.depth == 0 && !row.title.is_empty() {
                row.title.clone()
            } else {
                row.p.name.clone()
            };
            if name.text() != text {
                name.set_text(&text);
            }
            let tip = match &row.group {
                Some(g) => format!("{} {}", kind_name(g.kind), g.unit),
                None if row.p.cmdline.is_empty() => format!("[{}]", row.p.name),
                None => row.p.cmdline.clone(),
            };
            name.set_tooltip_text(Some(&tip));
            match (row.agg, row.p.state) {
                (Some(a), _) if a.count > 1 => {
                    tag.set_text(&a.count.to_string());
                    tag.set_tooltip_text(Some(&format!("{} processes", a.count)));
                    tag.remove_css_class("danger");
                    tag.set_visible(true);
                }
                (_, 'T') => {
                    tag.set_text("Paused");
                    tag.set_tooltip_text(None);
                    tag.add_css_class("danger");
                    tag.set_visible(true);
                }
                (_, 'Z') => {
                    tag.set_text("Zombie");
                    tag.set_tooltip_text(None);
                    tag.add_css_class("danger");
                    tag.set_visible(true);
                }
                _ => tag.set_visible(false),
            }
        }
        _ => {
            let Some(l) = w.downcast_ref::<gtk::Label>() else { return };
            let mem_total = MEM_TOTAL.with(|m| m.get());
            let heat = |v: f64| heat_level(col, v, mem_total);
            let (text, level, zero) = match col {
                Col::Pid | Col::State if row.group.is_some() => (String::new(), 0, false),
                Col::Pid => (row.p.pid.to_string(), 0, false),
                Col::User => (row.p.user.clone(), 0, false),
                Col::Cpu => {
                    let v = row.cpu();
                    (if v < 0.05 { "0".into() } else { format!("{v:.1}") }, heat(v), v < 0.05)
                }
                Col::Mem => (fmt::bytes(row.mem() as f64), heat(row.mem() as f64), row.mem() == 0),
                Col::Disk => {
                    let v = row.disk();
                    (if v < 1.0 { "0".into() } else { fmt::rate(v) }, heat(v), v < 1.0)
                }
                Col::Gpu => {
                    let v = row.gpu();
                    if v >= 0.05 {
                        (format!("{v:.1}"), heat(v), false)
                    } else if row.p.nvidia {
                        ("dGPU".into(), 0, false)
                    } else {
                        ("0".into(), 0, true)
                    }
                }
                Col::Threads => (row.threads().to_string(), 0, false),
                Col::State => (state_name(row.p.state).to_string(), 0, false),
                Col::Name => unreachable!(),
            };
            if l.text() != text {
                l.set_text(&text);
            }
            if !matches!(col, Col::User | Col::State) {
                set_heat(l, level, zero);
            }
        }
    }
}

/// All, with each app's or service's processes under one row.
fn grouping(s: &State) -> bool {
    s.view == View::All && s.grouped
}

/// Rows have branches to open and close.
fn is_tree(s: &State) -> bool {
    matches!(s.view, View::Apps | View::Tree) || grouping(s)
}

fn key(w: &gtk::Widget) -> usize {
    w.as_ptr() as usize
}

fn make_column(col: Col, title: &str, width: i32) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
        let w = setup_cell(col);
        if col == Col::Name
            && let Some(expander) = w.first_child().and_then(|i| i.next_sibling()).and_downcast::<gtk::Button>()
        {
            let item = item.downgrade();
            expander.connect_clicked(move |_| {
                if let Some(obj) = item.upgrade().and_then(|i| i.item()).and_downcast::<ProcObject>() {
                    toggle_expanded(obj.row().p.pid);
                }
            });
        }
        item.set_child(Some(&w));
    });
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
        let (Some(obj), Some(w)) = (item.item().and_downcast::<ProcObject>(), item.child()) else { return };
        let Some(st) = state() else { return };
        let (view, tree) = (st.borrow().view, is_tree(&st.borrow()));
        render(col, &obj.row(), &w, view, tree);
        st.borrow_mut().bound.insert(key(&w), (obj, col, w));
    });
    factory.connect_unbind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
        if let (Some(w), Some(st)) = (item.child(), state()) {
            st.borrow_mut().bound.remove(&key(&w));
        }
    });
    let c = gtk::ColumnViewColumn::new(Some(title), Some(factory));
    // A sorter that never reorders; it only gives the header its sort indicator.
    // The real order is worked out in `update`.
    c.set_sorter(Some(&gtk::CustomSorter::new(|_, _| gtk::Ordering::Equal)));
    c.set_resizable(true);
    if width > 0 {
        c.set_fixed_width(width);
    } else {
        c.set_expand(true);
    }
    c
}

// ---------- Update ----------

fn sort_state(st: &State) -> (Col, bool) {
    let Some(cvs) = st.view_widget.sorter().and_downcast::<gtk::ColumnViewSorter>() else { return (Col::Cpu, true) };
    let Some(primary) = cvs.primary_sort_column() else { return (Col::Cpu, true) };
    let col = st.columns.iter().find(|(_, c)| c == &primary).map(|(c, _)| *c).unwrap_or(Col::Cpu);
    (col, cvs.primary_sort_order() == gtk::SortType::Descending)
}

fn matches(row: &Row, q: &str) -> bool {
    q.is_empty()
        || row.p.name.to_lowercase().contains(q)
        || row.p.cmdline.to_lowercase().contains(q)
        || row.p.pid.to_string() == q
        || row.p.user.to_lowercase() == q
        || row.title.to_lowercase().contains(q)
}

fn icon_for(p: &Proc, class: Option<&str>) -> String {
    let found = class.and_then(apps::lookup).or_else(|| apps::lookup(&p.name));
    match found {
        Some(a) if !a.icon.is_empty() => a.icon,
        // Background processes get no icon; the column stays aligned.
        _ => String::new(),
    }
}

/// Take in a snapshot: sync objects, then recompute the view.
///
/// GTK re-binds rows synchronously while the store, filter or sorter change,
/// and binding reads the page state, so those calls happen with no borrow held.
fn update(snap: &Snapshot) {
    let Some(st) = state() else { return };
    let Some(procs) = &snap.procs else { return };
    let windows: HashMap<i32, &crate::sampler::windows::Window> = snap.windows.iter().map(|w| (w.pid, w)).collect();
    let window_pids: HashSet<i32> = windows.keys().copied().collect();
    let (store, removed, added) = {
        let mut s = st.borrow_mut();
        let headers = if grouping(&s) { group_rows(&mut s, snap, procs) } else { Vec::new() };
        let alive: HashSet<i32> = procs.iter().map(|p| p.pid).chain(headers.iter().map(|r| r.p.pid)).collect();
        let dead: Vec<i32> = s.objects.keys().filter(|p| !alive.contains(p)).copied().collect();
        let removed: Vec<ProcObject> = dead.iter().filter_map(|pid| s.objects.remove(pid)).collect();
        let mut added = Vec::new();
        // A process to show opens the group it's in.
        if let Some(pid) = s.pending_reveal
            && let Some(g) = headers.iter().filter_map(|r| r.group.as_ref()).find(|g| g.pids.contains(&pid))
        {
            s.opened_groups.insert(g.path.clone());
        }
        for row in headers {
            if let Some(obj) = s.objects.get(&row.p.pid) {
                let mut r = obj.row_mut();
                r.p = row.p;
                r.icon = row.icon;
                r.address = row.address;
                r.group = row.group;
            } else {
                let id = row.p.pid;
                let obj = ProcObject::new(row);
                added.push(obj.clone());
                s.objects.insert(id, obj);
            }
        }
        for p in procs {
            let win = windows.get(&p.pid);
            let class = win.map(|w| w.class.clone());
            let icon_key = (p.pid, class.clone().unwrap_or_default());
            let icon = match s.icons.get(&icon_key) {
                Some(i) => i.clone(),
                None => {
                    let i = icon_for(p, class.as_deref());
                    s.icons.insert(icon_key, i.clone());
                    i
                }
            };
            let title = win.map(|w| w.title.clone()).unwrap_or_default();
            let address = win.map(|w| w.address.clone()).unwrap_or_default();
            if let Some(obj) = s.objects.get(&p.pid) {
                let mut r = obj.row_mut();
                r.p = p.clone();
                r.icon = icon;
                r.title = title;
                r.address = address;
            } else {
                let obj = ProcObject::new(Row { p: p.clone(), icon, title, address, ..Default::default() });
                added.push(obj.clone());
                s.objects.insert(p.pid, obj);
            }
        }
        s.icons.retain(|(pid, _), _| alive.contains(pid));
        layout(&mut s, &window_pids);
        (s.store.clone(), removed, added)
    };
    for obj in removed {
        if let Some(pos) = store.find(&obj) {
            store.remove(pos);
        }
    }
    if !added.is_empty() {
        store.extend_from_slice(&added);
    }
    MEM_TOTAL.with(|m| m.set(snap.mem.total));
    apply_view(&st);
    let pending = st.borrow().pending_reveal;
    if let Some(pid) = pending {
        let (exists, visible) = st.borrow().objects.get(&pid).map_or((false, false), |o| (true, o.row().visible));
        if visible || !exists {
            st.borrow_mut().pending_reveal = None;
        }
        if visible {
            select_pid(pid);
        }
    }
    {
        let s = st.borrow();
        update_titles(&s, snap);
        let none = s.selection.n_items() == 0;
        let q = s.query.trim();
        if none && !q.is_empty() {
            s.empty.set_text(&format!("No processes match “{q}”."));
        }
        s.table.set_visible_child_name(if none && !q.is_empty() { "empty" } else { "table" });
        let narrow = s.split.parent().is_some_and(|p| p.has_css_class("narrow"));
        let wide = s.split.width() >= DOCK_WIDTH && !narrow;
        set_docked(&s, wide);
    }
    refresh_details(snap);
}

/// Re-run the filter and sort, then refresh the cells on screen.
///
/// ListView keeps its top row anchored, so after a re-sort it would scroll to
/// wherever that row moved. We hold the scroll position still instead.
fn apply_view(st: &Rc<RefCell<State>>) {
    let (filter, sorter, cv, adj) = {
        let s = st.borrow();
        (s.filter.clone(), s.sorter.clone(), s.view_widget.clone(), s.scroll.vadjustment())
    };
    let before = adj.value();
    filter.changed(gtk::FilterChange::Different);
    sorter.changed(gtk::SorterChange::Different);
    if before < 1.0 {
        if cv.model().is_some_and(|m| m.n_items() > 0) {
            cv.scroll_to(0, None, gtk::ListScrollFlags::NONE, None);
        }
    } else {
        adj.set_value(before);
        glib::idle_add_local_once(move || adj.set_value(before));
    }
    let (view, tree, bound): (View, bool, Vec<(ProcObject, Col, gtk::Widget)>) = {
        let s = st.borrow();
        (s.view, is_tree(&s), s.bound.values().cloned().collect())
    };
    for (obj, col, w) in bound {
        render(col, &obj.row(), &w, view, tree);
    }
}

/// Per pid: visible, order, depth, has children, expanded, collapsed totals.
type Placement = (bool, u32, u32, bool, bool, Option<Agg>);

/// A row for each app or service with more than one process (grouped All).
fn group_rows(s: &mut State, snap: &Snapshot, procs: &[Proc]) -> Vec<Row> {
    let by_pid: HashMap<i32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let mut rows = Vec::new();
    for g in snap.groups.iter().filter(|g| g.procs > 1) {
        let id = match s.group_ids.get(&g.path) {
            Some(id) => *id,
            None => {
                let id = s.next_group_id;
                s.next_group_id -= 1;
                s.group_ids.insert(g.path.clone(), id);
                id
            }
        };
        rows.push(group_row(id, g, snap, by_pid.get(&g.leader).copied()));
    }
    let live: HashSet<&str> = snap.groups.iter().map(|g| g.path.as_str()).collect();
    s.group_ids.retain(|path, _| live.contains(path.as_str()));
    s.opened_groups.retain(|path| live.contains(path.as_str()));
    rows
}

fn group_row(id: i32, g: &Group, snap: &Snapshot, leader: Option<&Proc>) -> Row {
    let (name, icon) = top::group_identity(g, snap);
    let address = match g.kind {
        Kind::App => snap.windows.iter().find(|w| g.pids.contains(&w.pid)).map(|w| w.address.clone()).unwrap_or_default(),
        _ => String::new(),
    };
    let p = Proc {
        pid: id,
        name,
        cmdline: leader.map(|l| l.cmdline.clone()).unwrap_or_default(),
        uid: leader.map_or(0, |l| l.uid),
        user: leader.map(|l| l.user.clone()).unwrap_or_default(),
        start: leader.map_or(0.0, |l| l.start),
        cgroup: g.path.clone(),
        ..Default::default()
    };
    let group = GroupRow { path: g.path.clone(), unit: g.unit.clone(), kind: g.kind, pids: g.pids.clone(), leader: g.leader };
    Row { p, icon, address, group: Some(group), ..Default::default() }
}

/// Processes added up, as a collapsed branch shows them.
fn sum<'a>(rows: impl Iterator<Item = &'a Row>) -> Agg {
    rows.fold(Agg::default(), |mut a, r| {
        a.cpu += r.p.cpu;
        a.mem += r.p.rss;
        a.disk += r.p.read_bps + r.p.write_bps;
        a.gpu = (a.gpu + r.p.gpu).min(100.0);
        a.threads += r.p.threads;
        a.count += 1;
        a
    })
}

/// Each group row's processes still running, and their totals on the group row.
fn add_group_totals(rows: &mut HashMap<i32, Row>) -> HashMap<i32, Vec<i32>> {
    let members: HashMap<i32, Vec<i32>> = rows
        .iter()
        .filter_map(|(id, r)| {
            let g = r.group.as_ref()?;
            Some((*id, g.pids.iter().copied().filter(|p| rows.get(p).is_some_and(|m| m.group.is_none())).collect()))
        })
        .collect();
    for (id, pids) in &members {
        let a = sum(pids.iter().map(|p| &rows[p]));
        if let Some(r) = rows.get_mut(id) {
            r.agg = Some(a);
        }
    }
    members
}

/// Grouped All: group rows and lone processes sorted together, each group's
/// processes under it while it's open. A filter shows the groups with a match,
/// opened to the matching processes.
fn place_grouped(
    rows: &HashMap<i32, Row>,
    mut members: HashMap<i32, Vec<i32>>,
    opened: &HashSet<String>,
    q: &str,
    show_kernel: bool,
    cmp: impl Fn(&i32, &i32) -> Ordering,
) -> HashMap<i32, Placement> {
    let in_group: HashSet<i32> = members.values().flatten().copied().collect();
    let mut top: Vec<i32> = rows
        .values()
        .filter(|r| r.group.is_some() || (!in_group.contains(&r.p.pid) && (show_kernel || !r.p.kernel)))
        .map(|r| r.p.pid)
        .collect();
    top.sort_by(&cmp);
    let mut out = HashMap::new();
    let mut order = 0u32;
    for id in top {
        let row = &rows[&id];
        let (Some(group), Some(kids)) = (&row.group, members.get_mut(&id)) else {
            out.insert(id, (matches(row, q), order, 0, false, false, None));
            order += 1;
            continue;
        };
        kids.sort_by(&cmp);
        let hits: Vec<bool> = kids.iter().map(|k| matches(&rows[k], q)).collect();
        let any = hits.iter().any(|h| *h);
        let (shown, expanded) = if q.is_empty() { (true, opened.contains(&group.path)) } else { (any || matches(row, q), any) };
        out.insert(id, (shown, order, 0, !kids.is_empty(), expanded, row.agg));
        order += 1;
        for (k, hit) in kids.iter().zip(hits) {
            out.insert(*k, (shown && expanded && hit, order, 1, false, false, None));
            order += 1;
        }
    }
    out
}

/// Work out visibility, order, depth and aggregates for the current view.
fn layout(s: &mut State, window_pids: &HashSet<i32>) {
    let (col, desc) = sort_state(s);
    let q = s.query.to_lowercase();
    let q = q.trim();
    let show_kernel = prefs::get().kernel_threads;
    let mut rows: HashMap<i32, Row> = s.objects.iter().map(|(pid, o)| (*pid, o.row().clone())).collect();
    let members = if grouping(s) { add_group_totals(&mut rows) } else { HashMap::new() };
    let rows = rows;
    let cmp = |a: &i32, b: &i32| {
        let o = compare(col, &rows[a], &rows[b]);
        if desc { o.reverse() } else { o }
    };
    let mut out: HashMap<i32, Placement> = HashMap::new();

    let flat = |filter: &dyn Fn(&Row) -> bool, out: &mut HashMap<i32, Placement>| {
        let mut list: Vec<i32> = rows.values().filter(|r| filter(r)).map(|r| r.p.pid).collect();
        list.sort_by(cmp);
        for (i, pid) in list.iter().enumerate() {
            out.insert(*pid, (true, i as u32, 0, false, false, None));
        }
    };

    let tree_view = matches!(s.view, View::Apps | View::Tree);
    if grouping(s) {
        out = place_grouped(&rows, members, &s.opened_groups, q, show_kernel, cmp);
    } else if !tree_view {
        let my_uid = s.my_uid;
        let mine = s.view == View::Mine;
        let apps = s.view == View::Apps;
        flat(
            &|r: &Row| {
                (show_kernel || !r.p.kernel || mine)
                    && (!mine || (r.p.uid == my_uid && !r.p.kernel))
                    && (!apps || window_pids.contains(&r.p.pid))
                    && matches(r, q)
            },
            &mut out,
        );
    } else {
        let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
        for r in rows.values() {
            if (!show_kernel && r.p.kernel && s.view == View::Tree) || r.p.pid == r.p.ppid {
                continue;
            }
            children.entry(r.p.ppid).or_default().push(r.p.pid);
        }
        for list in children.values_mut() {
            list.sort_by(cmp);
        }
        // Subtree sums.
        fn agg(pid: i32, rows: &HashMap<i32, Row>, children: &HashMap<i32, Vec<i32>>, memo: &mut HashMap<i32, Agg>) -> Agg {
            if let Some(a) = memo.get(&pid) {
                return *a;
            }
            let r = &rows[&pid];
            let mut a = Agg {
                cpu: r.p.cpu,
                mem: r.p.rss,
                disk: r.p.read_bps + r.p.write_bps,
                gpu: r.p.gpu,
                threads: r.p.threads,
                count: 1,
            };
            for c in children.get(&pid).into_iter().flatten() {
                let ca = agg(*c, rows, children, memo);
                a.cpu += ca.cpu;
                a.mem += ca.mem;
                a.disk += ca.disk;
                a.gpu = (a.gpu + ca.gpu).min(100.0);
                a.threads += ca.threads;
                a.count += ca.count;
            }
            memo.insert(pid, a);
            a
        }
        let mut memo = HashMap::new();
        let mut roots: Vec<i32> = if s.view == View::Apps {
            // Outermost window owners only (a terminal's child windows fold into it).
            window_pids
                .iter()
                .filter(|pid| rows.contains_key(pid))
                .filter(|pid| {
                    let mut cur = rows[pid].p.ppid;
                    let mut hops = 0;
                    while let Some(r) = rows.get(&cur) {
                        if window_pids.contains(&cur) {
                            return false;
                        }
                        cur = r.p.ppid;
                        hops += 1;
                        if hops > 64 {
                            break;
                        }
                    }
                    true
                })
                .copied()
                .collect()
        } else {
            rows.values()
                .filter(|r| !rows.contains_key(&r.p.ppid) || r.p.ppid == r.p.pid)
                .filter(|r| show_kernel || !r.p.kernel)
                .map(|r| r.p.pid)
                .collect()
        };
        // Sort roots by their (aggregated, for apps) value.
        let apps = s.view == View::Apps;
        roots.sort_by(|a, b| {
            let o = if apps && matches!(col, Col::Cpu | Col::Mem | Col::Disk | Col::Gpu | Col::Threads) {
                let (x, y) = (agg(*a, &rows, &children, &mut memo), agg(*b, &rows, &children, &mut memo));
                match col {
                    Col::Cpu => x.cpu.total_cmp(&y.cpu),
                    Col::Mem => x.mem.cmp(&y.mem),
                    Col::Disk => x.disk.total_cmp(&y.disk),
                    Col::Gpu => x.gpu.total_cmp(&y.gpu),
                    _ => x.threads.cmp(&y.threads),
                }
            } else {
                compare(col, &rows[a], &rows[b])
            };
            if desc { o.reverse() } else { o }
        });
        // Filtering keeps the tree: matches show with the path down to them opened.
        let keep: Option<HashSet<i32>> = (!q.is_empty()).then(|| {
            let parents: HashMap<i32, i32> = rows.values().map(|r| (r.p.pid, r.p.ppid)).collect();
            let hits: HashSet<i32> = rows.values().filter(|r| matches(r, q)).map(|r| r.p.pid).collect();
            with_ancestors(&hits, &parents)
        });
        let mut order = 0u32;
        let mut stack: Vec<(i32, u32, bool)> = roots.iter().rev().map(|p| (*p, 0, true)).collect();
        while let Some((pid, depth, shown)) = stack.pop() {
            let kids = children.get(&pid).cloned().unwrap_or_default();
            let has_children = !kids.is_empty();
            let (shown, expanded) = match &keep {
                Some(keep) => (shown && keep.contains(&pid), has_children && kids.iter().any(|k| keep.contains(k))),
                None => {
                    (shown, has_children && if s.view == View::Apps { s.opened.contains(&pid) } else { !s.closed.contains(&pid) })
                }
            };
            let a = if has_children && !expanded { Some(agg(pid, &rows, &children, &mut memo)) } else { None };
            out.insert(pid, (shown, order, depth, has_children, expanded, a));
            order += 1;
            for k in kids.iter().rev() {
                stack.push((*k, depth + 1, shown && expanded));
            }
        }
    }
    for (pid, obj) in &s.objects {
        let mut r = obj.row_mut();
        match out.get(pid) {
            Some(&(visible, order, depth, has_children, expanded, agg)) => {
                r.visible = visible;
                r.order = order;
                r.depth = depth;
                r.has_children = has_children;
                r.expanded = expanded;
                r.agg = agg;
            }
            None => {
                r.visible = false;
                r.order = u32::MAX;
                r.depth = 0;
                r.has_children = false;
                r.agg = None;
            }
        }
    }
}

/// The given pids plus every ancestor of each (following `parents`, pid → ppid).
fn with_ancestors(pids: &HashSet<i32>, parents: &HashMap<i32, i32>) -> HashSet<i32> {
    let mut out = pids.clone();
    for pid in pids {
        let mut cur = *pid;
        // Bounded, in case of a loop in stale data.
        for _ in 0..64 {
            match parents.get(&cur) {
                Some(&pp) if pp != cur && out.insert(pp) => cur = pp,
                _ => break,
            }
        }
    }
    out
}

/// Recompute the view right away (after a view, filter or expand change).
pub fn refilter() {
    let Some(st) = state() else { return };
    if let Some(snap) = live::latest() {
        update(&snap);
    } else {
        apply_view(&st);
    }
}

fn toggle_expanded(pid: i32) {
    let Some(st) = state() else { return };
    {
        let mut s = st.borrow_mut();
        let path = s.objects.get(&pid).and_then(|o| o.row().group.as_ref().map(|g| g.path.clone()));
        if let Some(path) = path {
            if !s.opened_groups.remove(&path) {
                s.opened_groups.insert(path);
            }
        } else {
            let set = if s.view == View::Apps { &mut s.opened } else { &mut s.closed };
            if !set.remove(&pid) {
                set.insert(pid);
            }
        }
    }
    refilter();
}

/// The row above `pid` in the current view: its group row in grouped All, else its parent.
fn parent_row(s: &State, pid: i32, ppid: i32) -> i32 {
    if !grouping(s) {
        return ppid;
    }
    s.objects.iter().find(|(_, o)| o.row().group.as_ref().is_some_and(|g| g.pids.contains(&pid))).map_or(ppid, |(id, _)| *id)
}

fn toggle_grouping() {
    let Some(st) = state() else { return };
    let on = {
        let mut s = st.borrow_mut();
        s.grouped = !s.grouped;
        if s.grouped {
            s.group_chip.add_css_class("selected");
        } else {
            s.group_chip.remove_css_class("selected");
        }
        s.grouped
    };
    prefs::update(|p| p.group_processes = on);
    refilter();
}

fn set_view(v: View) {
    let Some(st) = state() else { return };
    {
        let mut s = st.borrow_mut();
        s.view = v;
        s.group_chip.set_visible(v == View::All);
        for (view, b) in &s.chips {
            if *view == v {
                b.add_css_class("selected");
            } else {
                b.remove_css_class("selected");
            }
        }
    }
    prefs::update(|p| p.process_view = v.id().into());
    refilter();
}

fn selected(st: &State) -> Vec<ProcObject> {
    let sel = st.selection.selection();
    let mut out = Vec::new();
    if let Some((iter, first)) = gtk::BitsetIter::init_first(&sel) {
        for pos in std::iter::once(first).chain(iter) {
            if let Some(o) = st.selection.item(pos).and_downcast::<ProcObject>() {
                out.push(o);
            }
        }
    }
    out
}

fn select_pid(pid: i32) {
    let Some(st) = state() else { return };
    let s = st.borrow();
    let n = s.selection.n_items();
    for i in 0..n {
        if let Some(o) = s.selection.item(i).and_downcast::<ProcObject>()
            && o.row().p.pid == pid
        {
            s.selection.select_item(i, true);
            let cv = s.view_widget.clone();
            drop(s);
            cv.scroll_to(i, None, gtk::ListScrollFlags::FOCUS, None);
            return;
        }
    }
}

/// Show one process in the table (from the Overview).
pub fn reveal(pid: i32) {
    window::navigate("processes");
    let Some(st) = state() else { return };
    {
        let mut s = st.borrow_mut();
        s.query.clear();
        s.search.set_text("");
        if s.view == View::Apps {
            drop(s);
            set_view(View::All);
        }
    }
    st.borrow_mut().pending_reveal = Some(pid);
    refilter();
}

pub fn focus_search() {
    if let Some(st) = state() {
        let e = st.borrow().search.clone();
        e.grab_focus();
    }
}

/// The selected processes, a group row standing for each of its own.
fn selected_procs(st: &State) -> Vec<ProcObject> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for o in selected(st) {
        let pids = match &o.row().group {
            Some(g) => g.pids.clone(),
            None => vec![o.row().p.pid],
        };
        for pid in pids {
            if seen.insert(pid)
                && let Some(m) = st.objects.get(&pid)
            {
                out.push(m.clone());
            }
        }
    }
    out
}

/// The selected processes, minus kernel threads (signals can't reach them).
fn targets(st: &State) -> Vec<(i32, String)> {
    selected_procs(st).iter().filter(|o| !o.row().p.kernel).map(|o| (o.row().p.pid, o.row().p.name.clone())).collect()
}

fn end_selected(sig: Signal) {
    let Some(st) = state() else { return };
    let t = targets(&st.borrow());
    if !t.is_empty() {
        actions::signal(&t, sig);
    }
}

// ---------- Details ----------

struct Details {
    revealer: gtk::Revealer,
    icon: gtk::Image,
    title: gtk::Label,
    subtitle: gtk::Label,
    /// Everything below the head; hidden while the panel is collapsed.
    more: gtk::Box,
    values: Vec<gtk::Label>,
    /// The readings and the actions; fewer per row while docked beside the table.
    flows: [gtk::FlowBox; 2],
    cmdline: gtk::Label,
    graphs: gtk::Box,
    pid: Rc<Cell<i32>>,
    focus: gtk::Button,
    close: gtk::Button,
    pause: gtk::Button,
    priority: gtk::MenuButton,
    nice_checks: Vec<(i32, gtk::Image)>,
    kill: gtk::Button,
    end: gtk::Button,
}

/// The selected processes that signals may go to (kernel threads left out).
fn panel_targets() -> Vec<(i32, String)> {
    state().map(|s| targets(&s.borrow())).unwrap_or_default()
}

fn priority_popover(pid: &Rc<Cell<i32>>) -> (gtk::Popover, Vec<(i32, gtk::Image)>) {
    let pop = gtk::Popover::new();
    let plist = widgets::vbox(2);
    let mut checks = Vec::new();
    for (label, nice) in [
        ("Highest (-15)", -15),
        ("High (-5)", -5),
        ("Normal (0)", 0),
        ("Low (5)", 5),
        ("Lowest (10)", 10),
        ("Background (19)", 19),
    ] {
        let b = gtk::Button::new();
        b.add_css_class("flat");
        let row = widgets::hbox(8);
        let check = gtk::Image::from_icon_name("object-select-symbolic");
        check.set_opacity(0.0);
        let l = widgets::label(label, "");
        l.set_hexpand(true);
        row.append(&l);
        row.append(&check);
        b.set_child(Some(&row));
        let pop2 = pop.clone();
        b.connect_clicked(move |_| {
            pop2.popdown();
            let t = panel_targets();
            if !t.is_empty() {
                actions::renice(&t, nice);
            }
        });
        plist.append(&b);
        checks.push((nice, check));
    }
    let custom = widgets::hbox(6);
    custom.add_css_class("priority-custom");
    custom.append(&widgets::label("Custom", "dim"));
    let spin = gtk::SpinButton::with_range(-20.0, 19.0, 1.0);
    spin.set_hexpand(true);
    custom.append(&spin);
    let apply = gtk::Button::with_label("Set");
    let (pop2, spin2) = (pop.clone(), spin.clone());
    apply.connect_clicked(move |_| {
        pop2.popdown();
        let t = panel_targets();
        if !t.is_empty() {
            actions::renice(&t, spin2.value_as_int());
        }
    });
    custom.append(&apply);
    plist.append(&custom);
    pop.set_child(Some(&plist));
    // Start the spinner at the process's current value.
    let p = pid.clone();
    pop.connect_show(move |_| {
        if let Some(o) = object(p.get()) {
            spin.set_value(o.row().p.nice as f64);
        }
    });
    (pop, checks)
}

fn details_panel() -> Details {
    let revealer = gtk::Revealer::new();
    revealer.set_transition_type(gtk::RevealerTransitionType::SlideUp);
    revealer.set_transition_duration(if prefs::get().reduce_motion { 0 } else { 160 });
    // Its title expands; docked beside the table, a hidden panel would still take half the spare width.
    revealer.set_hexpand(false);
    let panel = widgets::vbox(10);
    panel.add_css_class("details-panel");
    revealer.set_child(Some(&panel));
    let pid = Rc::new(Cell::new(0));

    let head = widgets::hbox(10);
    let icon = gtk::Image::new();
    icon.set_pixel_size(28);
    let text = widgets::vbox(0);
    text.set_hexpand(true);
    let title = widgets::label("", "details-title");
    title.set_ellipsize(pango::EllipsizeMode::End);
    let subtitle = widgets::label("", "dim");
    subtitle.add_css_class("mono");
    subtitle.set_ellipsize(pango::EllipsizeMode::End);
    // "parent N" is a link to the parent process.
    subtitle.connect_activate_link(|_, uri| {
        if let Some(pid) = uri.strip_prefix("pid:").and_then(|p| p.parse().ok()) {
            glib::idle_add_local_once(move || {
                reveal(pid);
            });
        }
        glib::Propagation::Stop
    });
    text.append(&title);
    text.append(&subtitle);
    head.append(&icon);
    head.append(&text);
    let collapse = gtk::Button::from_icon_name("pan-down-symbolic");
    collapse.add_css_class("flat");
    collapse.set_valign(gtk::Align::Start);
    head.append(&collapse);
    let hide = gtk::Button::from_icon_name("window-close-symbolic");
    hide.add_css_class("flat");
    hide.set_tooltip_text(Some("Hide details"));
    hide.set_valign(gtk::Align::Start);
    hide.connect_clicked(|_| {
        if let Some(st) = state() {
            st.borrow().selection.unselect_all();
        }
    });
    head.append(&hide);
    panel.append(&head);

    let more = widgets::vbox(10);
    panel.append(&more);
    let (flow, values) =
        widgets::kv_flow(&["CPU", "Memory", "Disk read", "Disk write", "Threads", "Priority", "Open files", "Started"]);
    flow.set_max_children_per_line(8);
    more.append(&flow);
    let graphs = widgets::hbox(10);
    graphs.set_homogeneous(true);
    more.append(&graphs);
    let cmdline = widgets::label("", "code-block");
    cmdline.set_wrap(true);
    cmdline.set_wrap_mode(pango::WrapMode::WordChar);
    cmdline.set_selectable(true);
    cmdline.set_lines(3);
    cmdline.set_ellipsize(pango::EllipsizeMode::End);
    more.append(&cmdline);

    let set_compact = {
        let (more, collapse) = (more.clone(), collapse.clone());
        move |compact: bool| {
            more.set_visible(!compact);
            collapse.set_icon_name(if compact { "pan-up-symbolic" } else { "pan-down-symbolic" });
            collapse.set_tooltip_text(Some(if compact { "Show more" } else { "Show less" }));
        }
    };
    set_compact(prefs::get().details_compact);
    collapse.connect_clicked(move |_| {
        prefs::update(|p| p.details_compact = !p.details_compact);
        set_compact(prefs::get().details_compact);
        if let Some(snap) = live::latest() {
            refresh_details(&snap);
        }
    });

    // Actions. Those that signal go to every selected process.
    let buttons = gtk::FlowBox::new();
    buttons.set_selection_mode(gtk::SelectionMode::None);
    buttons.set_row_spacing(6);
    buttons.set_column_spacing(6);
    buttons.set_max_children_per_line(8);
    let p = pid.clone();
    let focus = gtk::Button::with_label("Switch to");
    focus.connect_clicked(move |_| {
        if let Some(o) = object(p.get()) {
            actions::focus_window(&o.row().address);
        }
    });
    let p = pid.clone();
    let close = gtk::Button::with_label("Close window");
    close.connect_clicked(move |_| {
        if let Some(o) = object(p.get()) {
            actions::close_window(&o.row().address);
        }
    });
    let pause = gtk::Button::with_label("Pause");
    pause.set_tooltip_text(Some("Freeze the process (SIGSTOP) until you resume it"));
    pause.connect_clicked(|_| pause_selected());
    let priority = gtk::MenuButton::new();
    priority.set_label("Priority");
    let (pop, nice_checks) = priority_popover(&pid);
    priority.set_popover(Some(&pop));
    let p = pid.clone();
    let location = gtk::Button::with_label("Open location");
    location.connect_clicked(move |_| actions::open_location(object(p.get()).map_or(p.get(), |o| o.row().pid())));
    let p = pid.clone();
    let copy = gtk::Button::with_label("Copy command");
    copy.connect_clicked(move |_| {
        if let Some(o) = object(p.get()) {
            actions::copy(&o.row().p.cmdline);
        }
    });
    let kill = kill_button("Kill", "Click again to kill", Signal::Kill);
    kill.set_tooltip_text(Some("Stop it immediately (SIGKILL). Unsaved work is lost."));
    let end = kill_button("End task", "Click again to end", Signal::Term);
    end.set_tooltip_text(Some("Ask it to quit (SIGTERM)"));
    for w in [
        focus.upcast_ref::<gtk::Widget>(),
        close.upcast_ref(),
        pause.upcast_ref(),
        priority.upcast_ref(),
        location.upcast_ref(),
        copy.upcast_ref(),
        kill.upcast_ref(),
        end.upcast_ref(),
    ] {
        buttons.append(w);
    }
    let mut child = buttons.first_child();
    while let Some(c) = child {
        c.set_focusable(false);
        child = c.next_sibling();
    }
    more.append(&buttons);

    Details {
        revealer,
        icon,
        title,
        subtitle,
        more,
        values,
        flows: [flow, buttons],
        cmdline,
        graphs,
        pid,
        focus,
        close,
        pause,
        priority,
        nice_checks,
        kill,
        end,
    }
}

/// Pause the selection, or resume it when every selected process is already paused.
fn pause_selected() {
    let Some(st) = state() else { return };
    let (t, all_stopped) = {
        let s = st.borrow();
        let sel: Vec<ProcObject> = selected_procs(&s).into_iter().filter(|o| !o.row().p.kernel).collect();
        let all = !sel.is_empty() && sel.iter().all(|o| o.row().p.state == 'T');
        (targets(&s), all)
    };
    if !t.is_empty() {
        actions::signal(&t, if all_stopped { Signal::Cont } else { Signal::Stop });
    }
}

/// End/Kill in the details panel: two clicks while "Ask before ending tasks" is on.
fn kill_button(label: &str, armed: &str, sig: Signal) -> gtk::Button {
    widgets::two_click_if(label, armed, || prefs::get().confirm_kill, move || end_selected(sig))
}

fn object(pid: i32) -> Option<ProcObject> {
    state().and_then(|s| s.borrow().objects.get(&pid).cloned())
}

/// Set a button's label unless it's waiting for its confirming click.
fn relabel(b: &gtk::Button, text: &str) {
    if !b.has_css_class("armed") && b.label().as_deref() != Some(text) {
        b.set_label(text);
    }
}

fn refresh_details(snap: &Snapshot) {
    let Some(st) = state() else { return };
    let s = st.borrow();
    let Some(d) = &s.details else { return };
    let sel = selected(&s);
    let Some(obj) = sel.first() else {
        d.revealer.set_reveal_child(false);
        return;
    };
    let r = obj.row().clone();
    d.revealer.set_reveal_child(true);
    // A group's own processes are in the table; its graphs would be only its leader's.
    d.graphs.set_visible(r.group.is_none());
    if d.pid.get() != r.p.pid {
        d.pid.set(r.p.pid);
        while let Some(c) = d.graphs.first_child() {
            d.graphs.remove(&c);
        }
    }
    if r.group.is_none() && d.graphs.first_child().is_none() {
        live::track(r.p.pid);
        let series = |key: String, tone| vec![graph::Series::new(key, "", tone)];
        let cpu = graph::sparkline(
            series(format!("proc.{}.cpu", r.p.pid), graph::Tone::Accent),
            graph::Scale::Auto { floor: 5.0 },
            40,
            fmt::pct,
        );
        let mem = graph::sparkline(
            series(format!("proc.{}.mem", r.p.pid), graph::Tone::Second),
            graph::Scale::Auto { floor: 1e6 },
            40,
            fmt::bytes,
        );
        for (title, g) in [("CPU", cpu), ("Memory", mem)] {
            let b = widgets::vbox(2);
            b.add_css_class("core-cell");
            b.append(&widgets::label(title, "core-name"));
            b.append(&g);
            d.graphs.append(&b);
        }
    }
    d.icon.set_icon_name(Some(if r.icon.is_empty() { "application-x-executable-symbolic" } else { &r.icon }));
    let name = if r.title.is_empty() { r.p.name.clone() } else { format!("{} — {}", r.p.name, r.title) };
    d.title.set_text(&name);
    let more = if sel.len() > 1 { format!(" · {} selected", sel.len()) } else { String::new() };
    let compact = !d.more.is_visible();
    let count = r.agg.map_or(1, |a| a.count);
    let markup = if let Some(g) = &r.group {
        let head = if compact {
            format!("{count} processes · {} · {}", fmt::pct(r.cpu()), fmt::bytes(r.mem() as f64))
        } else {
            format!("{} {} · {count} processes", kind_name(g.kind), g.unit)
        };
        glib::markup_escape_text(&format!("{head}{more}")).to_string()
    } else if compact {
        // Collapsed: the one line carries the live numbers.
        format!("PID {} · {} · {}{more}", r.p.pid, fmt::pct(r.cpu()), fmt::bytes(r.mem() as f64))
    } else {
        let parent = if s.objects.contains_key(&r.p.ppid) {
            format!("<a href=\"pid:{0}\" title=\"Show the parent process\">parent {0}</a>", r.p.ppid)
        } else {
            format!("parent {}", r.p.ppid)
        };
        format!(
            "PID {} · {parent} · {} · {}{}",
            r.p.pid,
            glib::markup_escape_text(&r.p.user),
            state_name(r.p.state),
            glib::markup_escape_text(&more)
        )
    };
    if d.subtitle.label() != markup {
        d.subtitle.set_markup(&markup);
    }
    let boot = glib::DateTime::now_local().ok().map(|n| n.to_unix() as f64 - snap.cpu.uptime);
    let started = boot
        .and_then(|b| glib::DateTime::from_unix_local((b + r.p.start) as i64).ok())
        .and_then(|t| t.format("%H:%M · %b %e").ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "–".into());
    // A group shows its processes added up; a priority only when they share one.
    let procs: Vec<Proc> = match &r.group {
        Some(g) => g.pids.iter().filter_map(|p| s.objects.get(p)).map(|o| o.row().p.clone()).collect(),
        None => vec![r.p.clone()],
    };
    let nices: HashSet<i32> = procs.iter().map(|p| p.nice).collect();
    let fds: u32 = procs.iter().map(|p| p.fds).sum();
    let vals = [
        fmt::pct(procs.iter().map(|p| p.cpu).sum()),
        fmt::bytes(procs.iter().map(|p| p.rss).sum::<u64>() as f64),
        fmt::rate(procs.iter().map(|p| p.read_bps).sum()),
        fmt::rate(procs.iter().map(|p| p.write_bps).sum()),
        procs.iter().map(|p| p.threads).sum::<u32>().to_string(),
        if nices.len() == 1 { nices.iter().next().map_or_else(String::new, i32::to_string) } else { "–".into() },
        if fds > 0 { fds.to_string() } else { "–".into() },
        started,
    ];
    for (l, v) in d.values.iter().zip(vals) {
        l.set_text(&v);
    }
    let cmd = if r.p.cmdline.is_empty() { format!("[{}] (kernel thread)", r.p.name) } else { r.p.cmdline.clone() };
    if d.cmdline.text() != cmd {
        d.cmdline.set_text(&cmd);
    }
    let has_window = !r.address.is_empty() && sel.len() == 1;
    d.focus.set_visible(has_window);
    d.close.set_visible(has_window);

    // Signals go to every selected process that isn't a kernel thread.
    let members = selected_procs(&s);
    let live_sel: Vec<&ProcObject> = members.iter().filter(|o| !o.row().p.kernel).collect();
    let n = live_sel.len();
    let all_stopped = n > 0 && live_sel.iter().all(|o| o.row().p.state == 'T');
    let verb = if all_stopped { "Resume" } else { "Pause" };
    if n > 1 {
        relabel(&d.pause, &format!("{verb} {n}"));
        relabel(&d.kill, &format!("Kill {n}"));
        relabel(&d.end, &format!("End {n} tasks"));
    } else {
        relabel(&d.pause, verb);
        relabel(&d.kill, "Kill");
        relabel(&d.end, "End task");
    }
    for b in [&d.pause, &d.kill, &d.end] {
        b.set_sensitive(n > 0);
    }
    d.priority.set_sensitive(n > 0);
    // Tick the current priority when the selection shares one.
    let nice = live_sel.first().map(|o| o.row().p.nice);
    let shared = nice.filter(|v| live_sel.iter().all(|o| o.row().p.nice == *v));
    for (value, check) in &d.nice_checks {
        check.set_opacity(if shared == Some(*value) { 1.0 } else { 0.0 });
    }
}

// ---------- Context menu ----------

/// The row widget under a point in the table, as the object it shows.
fn object_at(cv: &gtk::ColumnView, x: f64, y: f64) -> Option<ProcObject> {
    let st = state()?;
    let mut w = cv.pick(x, y, gtk::PickFlags::DEFAULT);
    while let Some(widget) = w {
        // Cells are bound by their content widget; climb until one matches.
        if let Some((obj, _, _)) = st.borrow().bound.get(&key(&widget)) {
            return Some(obj.clone());
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            if let Some((obj, _, _)) = st.borrow().bound.get(&key(&c)) {
                return Some(obj.clone());
            }
            child = c.next_sibling();
        }
        if widget == *cv.upcast_ref::<gtk::Widget>() {
            break;
        }
        w = widget.parent();
    }
    None
}

fn position_of(obj: &ProcObject) -> Option<u32> {
    let st = state()?;
    let s = st.borrow();
    (0..s.selection.n_items()).find(|i| s.selection.item(*i).and_downcast::<ProcObject>().as_ref() == Some(obj))
}

fn context_menu(cv: &gtk::ColumnView) {
    let group = gio::SimpleActionGroup::new();
    let add = |name: &str, f: fn()| {
        let a = gio::SimpleAction::new(name, None);
        a.connect_activate(move |_, _| f());
        group.add_action(&a);
        a
    };
    let first = || state().and_then(|s| selected(&s.borrow()).first().cloned());
    let switch = add("switch", || {
        if let Some(o) = state().and_then(|s| selected(&s.borrow()).first().cloned()) {
            actions::focus_window(&o.row().address);
        }
    });
    let close = add("close", || {
        if let Some(o) = state().and_then(|s| selected(&s.borrow()).first().cloned()) {
            actions::close_window(&o.row().address);
        }
    });
    let pause = add("pause", pause_selected);
    let end = add("end", || end_selected(Signal::Term));
    let kill = add("kill", || end_selected(Signal::Kill));
    add("tree", || {
        if let Some(o) = state().and_then(|s| selected(&s.borrow()).first().cloned()) {
            show_in_tree(o.row().pid());
        }
    });
    add("location", || {
        if let Some(o) = state().and_then(|s| selected(&s.borrow()).first().cloned()) {
            actions::open_location(o.row().pid());
        }
    });
    add("copy", || {
        if let Some(o) = state().and_then(|s| selected(&s.borrow()).first().cloned()) {
            actions::copy(&o.row().p.cmdline);
        }
    });
    let priority = gio::SimpleAction::new("priority", Some(glib::VariantTy::INT32));
    priority.connect_activate(|_, v| {
        let Some(nice) = v.and_then(|v| v.get::<i32>()) else { return };
        let t = panel_targets();
        if !t.is_empty() {
            actions::renice(&t, nice);
        }
    });
    group.add_action(&priority);
    cv.insert_action_group("proc", Some(&group));

    let menu = gio::Menu::new();
    let window = gio::Menu::new();
    window.append(Some("Switch to"), Some("proc.switch"));
    window.append(Some("Close window"), Some("proc.close"));
    menu.append_section(None, &window);
    let control = gio::Menu::new();
    control.append(Some("Pause / resume"), Some("proc.pause"));
    let prio = gio::Menu::new();
    for (label, nice) in [
        ("Highest (-15)", -15),
        ("High (-5)", -5),
        ("Normal (0)", 0),
        ("Low (5)", 5),
        ("Lowest (10)", 10),
        ("Background (19)", 19),
    ] {
        let item = gio::MenuItem::new(Some(label), None);
        item.set_action_and_target_value(Some("proc.priority"), Some(&nice.to_variant()));
        prio.append_item(&item);
    }
    control.append_submenu(Some("Priority"), &prio);
    menu.append_section(None, &control);
    let info = gio::Menu::new();
    info.append(Some("Show in Tree"), Some("proc.tree"));
    info.append(Some("Open location"), Some("proc.location"));
    info.append(Some("Copy command"), Some("proc.copy"));
    menu.append_section(None, &info);
    let stop = gio::Menu::new();
    stop.append(Some("End task"), Some("proc.end"));
    stop.append(Some("Kill"), Some("proc.kill"));
    menu.append_section(None, &stop);

    let pop = gtk::PopoverMenu::from_model(Some(&menu));
    pop.set_parent(cv);
    pop.set_has_arrow(false);
    pop.set_halign(gtk::Align::Start);
    let p2 = pop.clone();
    cv.connect_destroy(move |_| p2.unparent());

    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_SECONDARY);
    let cv2 = cv.clone();
    click.connect_pressed(move |g, _, x, y| {
        let Some(obj) = object_at(&cv2, x, y) else { return };
        g.set_state(gtk::EventSequenceState::Claimed);
        if let (Some(pos), Some(st)) = (position_of(&obj), state()) {
            let sel = st.borrow().selection.clone();
            if !sel.is_selected(pos) {
                sel.select_item(pos, true);
            }
        }
        // Only what applies to this selection is offered.
        let (one, has_window, any_user) = {
            let Some(st) = state() else { return };
            let s = st.borrow();
            let sel = selected(&s);
            let one = sel.len() == 1;
            let has_window = one && first().is_some_and(|o| !o.row().address.is_empty());
            (one, has_window, sel.iter().any(|o| !o.row().p.kernel))
        };
        switch.set_enabled(has_window);
        close.set_enabled(has_window);
        for a in [&pause, &end, &kill, &priority] {
            a.set_enabled(any_user);
        }
        for name in ["tree", "location", "copy"] {
            if let Some(a) = group.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                a.set_enabled(one);
            }
        }
        pop.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        pop.popup();
    });
    cv.add_controller(click);
}

/// Switch to the Tree view with this process selected.
fn show_in_tree(pid: i32) {
    let Some(st) = state() else { return };
    let search = st.borrow().search.clone();
    st.borrow_mut().query.clear();
    search.set_text("");
    // Open the branches down to it.
    let parents: HashMap<i32, i32> = st.borrow().objects.iter().map(|(p, o)| (*p, o.row().p.ppid)).collect();
    let path = with_ancestors(&HashSet::from([pid]), &parents);
    st.borrow_mut().closed.retain(|p| !path.contains(p));
    st.borrow_mut().pending_reveal = Some(pid);
    set_view(View::Tree);
}

// ---------- Saved column layout ----------

thread_local! {
    static SAVE_COLUMNS: RefCell<Option<glib::SourceId>> = const { RefCell::new(None) };
}

/// Save column widths and order a moment after the last change (dragging fires a lot).
fn save_columns_soon() {
    SAVE_COLUMNS.with(|s| {
        if let Some(id) = s.borrow_mut().take() {
            id.remove();
        }
        *s.borrow_mut() = Some(glib::timeout_add_local_once(std::time::Duration::from_millis(400), || {
            SAVE_COLUMNS.with(|s| *s.borrow_mut() = None);
            let Some(st) = state() else { return };
            let cv = st.borrow().view_widget.clone();
            let cols = cv.columns();
            let mut order = Vec::new();
            let mut widths = HashMap::new();
            for i in 0..cols.n_items() {
                let Some(c) = cols.item(i).and_downcast::<gtk::ColumnViewColumn>() else { continue };
                let Some(id) = c.id() else { continue };
                order.push(id.to_string());
                if c.fixed_width() > 0 && !c.expands() {
                    widths.insert(id.to_string(), c.fixed_width());
                }
            }
            prefs::update(|p| {
                p.column_order = order;
                p.column_widths = widths;
            });
        }));
    });
}

fn save_sort(st: &State) {
    let (col, desc) = sort_state(st);
    let id = COLUMNS.iter().find(|(c, ..)| *c == col).map(|(_, id, ..)| id.to_string()).unwrap_or_default();
    let p = prefs::get();
    if p.sort_column != id || p.sort_desc != desc {
        prefs::update(|p| {
            p.sort_column = id;
            p.sort_desc = desc;
        });
    }
}

/// "CPU 23%" and so on: the column titles carry the machine's totals.
fn update_titles(st: &State, snap: &Snapshot) {
    let gpu = snap.gpus.iter().filter_map(|g| g.util).fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))));
    for (col, c) in &st.columns {
        let base = COLUMNS.iter().find(|(x, ..)| x == col).map(|(_, _, t, _)| *t).unwrap_or("");
        let total = match col {
            Col::Cpu => Some(fmt::pct(snap.cpu.usage)),
            Col::Mem if snap.mem.total > 0 => Some(fmt::pct(snap.mem.used as f64 / snap.mem.total as f64 * 100.0)),
            Col::Disk => Some(fmt::rate(snap.disk_read() + snap.disk_write())),
            Col::Gpu => gpu.map(fmt::pct),
            _ => None,
        };
        let title = match total {
            Some(t) => format!("{base} {t}"),
            None => base.to_string(),
        };
        if c.title().as_deref() != Some(title.as_str()) {
            c.set_title(Some(&title));
        }
    }
}

// ---------- Build ----------

/// Wide enough for the details panel to sit beside the table.
const DOCK_WIDTH: i32 = 1100;
const DOCKED_PANEL: i32 = 380;

fn set_docked(st: &State, docked: bool) {
    let Some(d) = &st.details else { return };
    if st.split.orientation() == gtk::Orientation::Horizontal && docked {
        return;
    }
    if st.split.orientation() == gtk::Orientation::Vertical && !docked {
        return;
    }
    st.split.set_orientation(if docked { gtk::Orientation::Horizontal } else { gtk::Orientation::Vertical });
    d.revealer.set_transition_type(if docked {
        gtk::RevealerTransitionType::SlideLeft
    } else {
        gtk::RevealerTransitionType::SlideUp
    });
    // The table scrolls sideways, so it doesn't hold its width against the panel: docked,
    // the panel has to ask for no more than its column, or it takes the table's room.
    let per_line = if docked { 2 } else { 8 };
    for flow in &d.flows {
        flow.set_max_children_per_line(per_line);
    }
    // They still fill the panel; this only stops them asking for more.
    let chars = if docked { 12 } else { -1 };
    for l in [&d.title, &d.subtitle, &d.cmdline] {
        l.set_max_width_chars(chars);
    }
    if let Some(panel) = d.revealer.child() {
        if docked {
            panel.add_css_class("docked");
            panel.set_size_request(DOCKED_PANEL, -1);
        } else {
            panel.remove_css_class("docked");
            panel.set_size_request(-1, -1);
        }
    }
}

pub fn build(page: &Page) {
    let p = prefs::get();
    let view = View::from_id(&p.process_view);

    // Toolbar: views, filter, columns.
    let toolbar = widgets::hbox(8);
    toolbar.add_css_class("toolbar");
    toolbar.add_css_class("table-toolbar");
    let chips_box = widgets::hbox(6);
    let mut chips = Vec::new();
    for (v, label, tip) in [
        (View::Apps, "Apps", "Programs with windows, with their helper processes"),
        (View::All, "All", "Every process"),
        (View::Tree, "Tree", "Processes under the ones that started them"),
        (View::Mine, "Mine", "Only your own processes"),
    ] {
        let b = gtk::Button::with_label(label);
        b.add_css_class("chip");
        b.set_tooltip_text(Some(tip));
        if v == view {
            b.add_css_class("selected");
        }
        b.connect_clicked(move |_| set_view(v));
        chips_box.append(&b);
        chips.push((v, b));
    }
    toolbar.append(&chips_box);
    let group_chip = gtk::Button::with_label("Group by app");
    group_chip.add_css_class("chip");
    group_chip.set_tooltip_text(Some("Put each app's or service's processes under one row"));
    if p.group_processes {
        group_chip.add_css_class("selected");
    }
    group_chip.set_visible(view == View::All);
    group_chip.connect_clicked(|_| toggle_grouping());
    toolbar.append(&group_chip);
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Filter by name, command, PID or user"));
    search.set_hexpand(true);
    toolbar.append(&search);

    let columns_menu = gtk::MenuButton::new();
    columns_menu.set_icon_name("view-list-symbolic");
    columns_menu.set_tooltip_text(Some("Columns"));
    toolbar.append(&columns_menu);
    page.body.append(&toolbar);

    // Model: store -> filter -> sort -> selection.
    let store = gio::ListStore::new::<ProcObject>();
    let filter = gtk::CustomFilter::new(|o| o.downcast_ref::<ProcObject>().is_some_and(|o| o.row().visible));
    let filtered = gtk::FilterListModel::new(Some(store.clone()), Some(filter.clone()));
    let sorter = gtk::CustomSorter::new(|a, b| {
        let (Some(a), Some(b)) = (a.downcast_ref::<ProcObject>(), b.downcast_ref::<ProcObject>()) else {
            return gtk::Ordering::Equal;
        };
        a.row().order.cmp(&b.row().order).into()
    });
    let sorted = gtk::SortListModel::new(Some(filtered), Some(sorter.clone()));
    let selection = gtk::MultiSelection::new(Some(sorted));
    let cv = gtk::ColumnView::new(Some(selection.clone()));
    cv.set_show_column_separators(false);
    cv.set_reorderable(true);
    cv.set_hexpand(true);
    cv.set_vexpand(true);

    // Columns in the saved order (any new ones at the end), at their saved widths.
    let hidden = p.hidden_columns.clone();
    let mut ordered: Vec<&(Col, &str, &str, i32)> = COLUMNS.iter().collect();
    ordered.sort_by_key(|(_, id, ..)| p.column_order.iter().position(|o| o == id).unwrap_or(usize::MAX));
    let mut columns = Vec::new();
    let colbox = widgets::vbox(2);
    for (col, id, title, width) in COLUMNS {
        let c = make_column(*col, title, p.column_widths.get(*id).copied().filter(|_| *width > 0).unwrap_or(*width));
        c.set_id(Some(id));
        c.set_visible(!hidden.iter().any(|h| h == id));
        c.connect_fixed_width_notify(|_| save_columns_soon());
        if *col != Col::Name {
            let check = gtk::CheckButton::with_label(title);
            check.set_active(c.is_visible());
            let c2 = c.clone();
            let id = id.to_string();
            check.connect_toggled(move |b| {
                c2.set_visible(b.is_active());
                let on = b.is_active();
                let id = id.clone();
                prefs::update(move |p| {
                    p.hidden_columns.retain(|h| *h != id);
                    if !on {
                        p.hidden_columns.push(id);
                    }
                });
            });
            colbox.append(&check);
        }
        columns.push((*col, c));
    }
    for (col, ..) in ordered {
        if let Some((_, c)) = columns.iter().find(|(x, _)| x == col) {
            cv.append_column(c);
        }
    }
    cv.columns().connect_items_changed(|_, _, _, _| save_columns_soon());
    let pop = gtk::Popover::new();
    pop.set_child(Some(&colbox));
    columns_menu.set_popover(Some(&pop));
    let sort_col = COLUMNS.iter().find(|(_, id, ..)| *id == p.sort_column).map_or(Col::Cpu, |(c, ..)| *c);
    if let Some(c) = columns.iter().find(|(c, _)| *c == sort_col).map(|(_, c)| c.clone()) {
        let order = if p.sort_desc || p.sort_column.is_empty() { gtk::SortType::Descending } else { gtk::SortType::Ascending };
        cv.sort_by_column(Some(&c), order);
    }
    if let Some(cvs) = cv.sorter() {
        cvs.connect_changed(|_, _| {
            if let Some(st) = state() {
                save_sort(&st.borrow());
            }
            refilter();
        });
    }

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::External)
        .child(&cv)
        .vexpand(true)
        .build();
    let card = widgets::vbox(0);
    card.add_css_class("table-card");
    card.set_overflow(gtk::Overflow::Hidden);
    card.append(&scroll);
    card.set_vexpand(true);
    let empty = widgets::label("", "empty-state");
    empty.set_valign(gtk::Align::Start);
    empty.set_wrap(true);
    let table = gtk::Stack::new();
    table.set_hexpand(true);
    table.set_vexpand(true);
    table.add_named(&card, Some("table"));
    table.add_named(&empty, Some("empty"));

    // Table and details: stacked, or side by side when there's room.
    let split = gtk::Box::new(gtk::Orientation::Vertical, 0);
    split.set_vexpand(true);
    split.append(&table);
    let details = details_panel();
    split.append(&details.revealer);
    page.body.append(&split);

    let my_uid = unsafe { libc::getuid() };
    let st = State {
        store,
        objects: HashMap::new(),
        filter,
        sorter,
        selection: selection.clone(),
        view_widget: cv.clone(),
        scroll: scroll.clone(),
        columns,
        view,
        query: String::new(),
        opened: HashSet::new(),
        closed: HashSet::new(),
        grouped: p.group_processes,
        group_chip,
        group_ids: HashMap::new(),
        next_group_id: -1,
        opened_groups: HashSet::new(),
        bound: HashMap::new(),
        icons: HashMap::new(),
        search: search.clone(),
        chips,
        pending_reveal: None,
        details: Some(details),
        my_uid,
        table,
        empty,
        split,
    };
    STATE.with(|s| *s.borrow_mut() = Some(Rc::new(RefCell::new(st))));

    search.connect_search_changed(|e| {
        if let Some(st) = state() {
            st.borrow_mut().query = e.text().to_string();
        }
        refilter();
    });
    // Esc in the filter clears it and goes back to the table.
    let cv2 = cv.clone();
    search.connect_stop_search(move |e| {
        e.set_text("");
        cv2.grab_focus();
    });
    selection.connect_selection_changed(|_, _, _| {
        if let Some(snap) = live::latest() {
            refresh_details(&snap);
        }
    });
    cv.connect_activate(|cv, pos| {
        let Some(o) = cv.model().and_then(|m| m.item(pos)).and_downcast::<ProcObject>() else { return };
        let (pid, has_children, address) = {
            let r = o.row();
            (r.p.pid, r.has_children, r.address.clone())
        };
        if has_children {
            toggle_expanded(pid);
        } else if !address.is_empty() {
            actions::focus_window(&address);
        }
    });

    // Delete ends, Shift+Delete kills. With confirmation on, press twice.
    let armed: Rc<Cell<Option<(Signal, std::time::Instant)>>> = Rc::new(Cell::new(None));
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(move |_, key, _, mods| {
        match key {
            gdk::Key::slash => {
                focus_search();
                return glib::Propagation::Stop;
            }
            gdk::Key::Left | gdk::Key::Right if mods.is_empty() => return tree_key(key == gdk::Key::Right),
            gdk::Key::Delete | gdk::Key::KP_Delete => {}
            _ => return glib::Propagation::Proceed,
        }
        let sig = if mods.contains(gdk::ModifierType::SHIFT_MASK) { Signal::Kill } else { Signal::Term };
        let Some(st) = state() else { return glib::Propagation::Stop };
        let t = targets(&st.borrow());
        if t.is_empty() {
            return glib::Propagation::Stop;
        }
        let confirm = prefs::get().confirm_kill;
        let ready = matches!(armed.get(), Some((s, at)) if s == sig && at.elapsed().as_secs_f64() < 3.0);
        if !confirm || ready {
            armed.set(None);
            end_selected(sig);
        } else {
            armed.set(Some((sig, std::time::Instant::now())));
            let what = if t.len() == 1 { t[0].1.clone() } else { format!("{} processes", t.len()) };
            let verb = if sig == Signal::Kill { "kill" } else { "end" };
            window::toast(&format!("Press again to {verb} {what}"));
        }
        glib::Propagation::Stop
    });
    // Ahead of the list's own arrow handling.
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    cv.add_controller(keys);
    context_menu(&cv);

    live::on_tick(&cv, update);
}

/// Left/Right in Apps and Tree: close or open the selected branch; Left on a
/// closed or childless row moves to its parent.
fn tree_key(open: bool) -> glib::Propagation {
    let Some(st) = state() else { return glib::Propagation::Proceed };
    let (tree, sel) = {
        let s = st.borrow();
        (is_tree(&s), selected(&s))
    };
    if !tree || sel.len() != 1 {
        return glib::Propagation::Proceed;
    }
    let (pid, ppid, depth, has_children, expanded) = {
        let r = sel[0].row();
        (r.p.pid, r.p.ppid, r.depth, r.has_children, r.expanded)
    };
    let ppid = parent_row(&st.borrow(), pid, ppid);
    if open {
        if has_children && !expanded {
            toggle_expanded(pid);
        }
    } else if has_children && expanded {
        toggle_expanded(pid);
    } else if depth > 0 {
        select_pid(ppid);
    }
    glib::Propagation::Stop
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_ancestors() {
        // 1 ← 10 ← 100, 1 ← 20; 7 points at itself.
        let parents = HashMap::from([(10, 1), (100, 10), (20, 1), (1, 0), (7, 7)]);
        let got = with_ancestors(&HashSet::from([100, 7]), &parents);
        assert_eq!(got, HashSet::from([100, 10, 1, 0, 7]));
    }

    fn proc_row(pid: i32, name: &str, cpu: f64, kernel: bool) -> Row {
        Row { p: Proc { pid, name: name.into(), cpu, rss: 1000, kernel, ..Default::default() }, ..Default::default() }
    }

    /// Firefox (-1) holds 10 and 11; 20 runs alone; 30 is a kernel thread.
    fn sample() -> HashMap<i32, Row> {
        let group = GroupRow { path: "/app-firefox.scope".into(), pids: vec![10, 11], leader: 10, ..Default::default() };
        let header =
            Row { p: Proc { pid: -1, name: "Firefox".into(), ..Default::default() }, group: Some(group), ..Default::default() };
        [
            header,
            proc_row(10, "firefox", 5.0, false),
            proc_row(11, "Web Content", 20.0, false),
            proc_row(20, "foot", 1.0, false),
            proc_row(30, "kworker", 0.0, true),
        ]
        .into_iter()
        .map(|r| (r.p.pid, r))
        .collect()
    }

    fn by_cpu(rows: &HashMap<i32, Row>) -> impl Fn(&i32, &i32) -> Ordering + '_ {
        |a, b| compare(Col::Cpu, &rows[a], &rows[b]).reverse()
    }

    #[test]
    fn groups_add_up_their_processes() {
        let mut rows = sample();
        let members = add_group_totals(&mut rows);
        assert_eq!(members[&-1], vec![10, 11]);
        let a = rows[&-1].agg.expect("group totals");
        assert_eq!((a.count, a.cpu, a.mem), (2, 25.0, 2000));
    }

    #[test]
    fn groups_start_closed() {
        let mut rows = sample();
        let members = add_group_totals(&mut rows);
        let out = place_grouped(&rows, members, &HashSet::new(), "", false, by_cpu(&rows));
        // The group (25%) sorts above foot (1%); its processes and the kernel thread are hidden.
        let (shown, order, depth, has_children, expanded, agg) = out[&-1];
        assert_eq!((shown, order, depth, has_children, expanded), (true, 0, 0, true, false));
        assert_eq!(agg.map(|a| a.count), Some(2));
        assert!(!out[&10].0 && !out[&11].0);
        assert_eq!(out[&20].0, true);
        assert!(!out.contains_key(&30));
    }

    #[test]
    fn opened_groups_list_their_processes() {
        let mut rows = sample();
        let members = add_group_totals(&mut rows);
        let opened = HashSet::from(["/app-firefox.scope".to_string()]);
        let out = place_grouped(&rows, members, &opened, "", false, by_cpu(&rows));
        assert_eq!((out[&11].0, out[&11].1, out[&11].2), (true, 1, 1));
        assert_eq!((out[&10].0, out[&10].1), (true, 2));
        assert_eq!(out[&20].1, 3);
    }

    #[test]
    fn a_filter_opens_groups_to_their_matches() {
        let mut rows = sample();
        let members = add_group_totals(&mut rows);
        let out = place_grouped(&rows, members, &HashSet::new(), "web", false, by_cpu(&rows));
        assert!(out[&-1].0 && out[&-1].4);
        assert!(out[&11].0 && !out[&10].0);
        assert!(!out[&20].0);
    }

    #[test]
    fn grades_heat() {
        assert_eq!(heat_level(Col::Cpu, 0.5, 0), 0);
        assert_eq!(heat_level(Col::Cpu, 20.0, 0), 3);
        assert_eq!(heat_level(Col::Cpu, 99.0, 0), 5);
        // 1 GiB of 16 GiB is 6.25%: two steps.
        assert_eq!(heat_level(Col::Mem, (1u64 << 30) as f64, 16 << 30), 2);
        assert_eq!(heat_level(Col::Mem, 1e9, 0), 0);
        assert_eq!(heat_level(Col::Pid, 1e9, 0), 0);
    }
}
