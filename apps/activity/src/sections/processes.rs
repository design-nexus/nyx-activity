//! The process table. One stable object per pid lives in a `gio::ListStore`;
//! every tick we update their data in Rust, work out order, tree depth and
//! visibility ourselves, and let GTK's sort/filter models follow. Selection and
//! scroll position survive updates because the objects never change identity.
//!
//! Apps shows one row per app or service under Apps, Background and System, each
//! opening to its own processes; All and Mine list processes, flat or under the
//! ones that started them. The selection's graphs, facts and actions are in an
//! inspector beside the table, or a bar under it in a narrow window.

use super::top::{self, kind_name};
use crate::actions::{self, Signal};
use crate::sampler::Snapshot;
use crate::sampler::groups::{Group, Kind};
use crate::sampler::procs::Proc;
use crate::widgets::{self, Page};
use crate::{fmt, graph, live, prefs, theme, window};
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, gio, glib, pango};
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

// ---------- Row data ----------

/// Where a row sits in Apps: apps with windows, your own background programs, the system's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Sec {
    #[default]
    Apps,
    Background,
    System,
}

const SECS: [Sec; 3] = [Sec::Apps, Sec::Background, Sec::System];

impl Sec {
    fn title(self) -> &'static str {
        match self {
            Sec::Apps => "Apps",
            Sec::Background => "Background",
            Sec::System => "System",
        }
    }

    /// Its heading's colour, the way the sidebar's groups take theirs.
    fn colour(self) -> &'static str {
        match self {
            Sec::Apps => nyx_ui::sidebar::ACCENT_SECTION,
            Sec::Background => "swatch-green",
            Sec::System => "swatch-violet",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Clone, Default)]
pub struct Row {
    pub p: Proc,
    pub icon: String,
    pub title: String,
    pub address: String,
    /// The app's own name, on a lone process that stands for its app in Apps.
    pub display: String,
    pub section: Sec,
    /// Open TCP connections.
    pub conns: u32,
    // Computed per tick for the current view.
    pub depth: u32,
    pub has_children: bool,
    pub expanded: bool,
    pub visible: bool,
    pub order: u32,
    /// Summed over the subtree, shown while a group is collapsed.
    pub agg: Option<Agg>,
    /// Set on an app's or service's row in Apps; its `p` describes the group.
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
    /// What its cgroup is charged for, counting shared pages once.
    pub memory: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Agg {
    pub cpu: f64,
    pub mem: u64,
    pub disk: f64,
    pub gpu: f64,
    pub threads: u32,
    pub conns: u32,
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
    fn conns(&self) -> u32 {
        self.agg.map_or(self.conns, |a| a.conns)
    }
    /// The process this row stands for: a group's leader, or its own.
    fn pid(&self) -> i32 {
        self.group.as_ref().map_or(self.p.pid, |g| g.leader)
    }
    /// The name the row shows: an app's at the top of Apps, the process's elsewhere.
    fn name(&self) -> &str {
        if self.display.is_empty() { &self.p.name } else { &self.display }
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
    Net,
    Gpu,
    Threads,
    State,
}

/// Id, title and width (0: takes the room that's left).
const COLUMNS: &[(Col, &str, &str, i32)] = &[
    (Col::Name, "name", "Name", 0),
    (Col::Pid, "pid", "PID", 80),
    (Col::User, "user", "User", 90),
    (Col::Cpu, "cpu", "CPU", 136),
    (Col::Mem, "mem", "Memory", 100),
    (Col::Disk, "disk", "Disk", 100),
    (Col::Net, "net", "Network", 100),
    (Col::Gpu, "gpu", "GPU", 76),
    (Col::Threads, "threads", "Threads", 76),
    (Col::State, "state", "State", 116),
];

/// What a narrow window keeps.
const NARROW_COLUMNS: [Col; 3] = [Col::Name, Col::Cpu, Col::Mem];

fn col_id(col: Col) -> &'static str {
    COLUMNS.iter().find(|(c, ..)| *c == col).map_or("", |(_, id, ..)| *id)
}

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
        Col::Name => a.name().to_lowercase().cmp(&b.name().to_lowercase()).then(a.p.pid.cmp(&b.p.pid)),
        Col::Pid => a.pid().cmp(&b.pid()),
        Col::User => a.p.user.cmp(&b.p.user).then(a.p.pid.cmp(&b.p.pid)),
        Col::Cpu => a.cpu().total_cmp(&b.cpu()),
        Col::Mem => a.mem().cmp(&b.mem()),
        Col::Disk => a.disk().total_cmp(&b.disk()),
        Col::Net => a.conns().cmp(&b.conns()),
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
    Mine,
}

impl View {
    fn id(self) -> &'static str {
        match self {
            View::Apps => "apps",
            View::All => "all",
            View::Mine => "mine",
        }
    }
    fn from_id(s: &str) -> View {
        match s {
            "apps" => View::Apps,
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
    sorted: gtk::SortListModel,
    section_sorter: gtk::CustomSorter,
    header_factory: gtk::SignalListItemFactory,
    selection: gtk::MultiSelection,
    view_widget: gtk::ColumnView,
    scroll: gtk::ScrolledWindow,
    columns: Vec<(Col, gtk::ColumnViewColumn)>,
    columns_menu: gtk::MenuButton,
    view: View,
    query: String,
    /// All and Mine: each process under the one that started it.
    tree: bool,
    tree_check: gtk::CheckButton,
    /// Branches the user closed in the tree.
    closed: HashSet<i32>,
    /// Group rows' ids (negative, so never a pid), kept by cgroup path.
    group_ids: HashMap<String, i32>,
    next_group_id: i32,
    /// Groups the user opened, by cgroup path.
    opened_groups: HashSet<String>,
    bound: HashMap<usize, (ProcObject, Col, gtk::Widget)>,
    headers: HashMap<usize, gtk::ListHeader>,
    /// Apps: the rows shown under each heading.
    counts: [usize; 3],
    /// Icon and app name by (pid, window class).
    identities: HashMap<(i32, String), (String, String)>,
    search: gtk::SearchEntry,
    segments: Vec<(View, gtk::ToggleButton)>,
    totals: gtk::Label,
    pending_reveal: Option<i32>,
    inspector: Option<Inspector>,
    foot: Option<Foot>,
    actions: gio::SimpleActionGroup,
    my_uid: u32,
    /// The table, or the "nothing matches" message.
    table: gtk::Stack,
    empty: gtk::Label,
    body: gtk::Box,
    narrow: bool,
    light: bool,
}

thread_local! {
    static STATE: RefCell<Option<Rc<RefCell<State>>>> = const { RefCell::new(None) };
}

fn state() -> Option<Rc<RefCell<State>>> {
    STATE.with(|s| s.borrow().clone())
}

/// How rows draw in the current view.
#[derive(Debug, Clone, Copy)]
struct Look {
    apps: bool,
    tree: bool,
    light: bool,
}

fn look(s: &State) -> Look {
    Look { apps: s.view == View::Apps, tree: s.view != View::Apps && s.tree, light: s.light }
}

/// Rows have branches to open and close.
fn is_tree(s: &State) -> bool {
    s.view == View::Apps || s.tree
}

// ---------- Faces: an app's icon, or its letter on a tile ----------

thread_local! {
    static HAS_ICON: RefCell<HashMap<String, bool>> = RefCell::new(HashMap::new());
}

/// Whether the icon theme (or the file a desktop entry names) has this icon.
fn icon_exists(icon: &str) -> bool {
    if icon.is_empty() {
        return false;
    }
    HAS_ICON.with(|h| {
        *h.borrow_mut().entry(icon.to_string()).or_insert_with(|| {
            if icon.starts_with('/') {
                std::path::Path::new(icon).is_file()
            } else {
                gdk::Display::default().is_some_and(|d| gtk::IconTheme::for_display(&d).has_icon(icon))
            }
        })
    })
}

fn face(class: &str, size: i32) -> gtk::Box {
    let root = widgets::hbox(0);
    root.add_css_class("proc-face");
    root.add_css_class(class);
    root.set_valign(gtk::Align::Center);
    let image = gtk::Image::new();
    image.set_pixel_size(size);
    let tile = widgets::hbox(0);
    tile.set_size_request(size, size);
    tile.set_hexpand(false);
    let letter = gtk::Label::new(None);
    letter.set_hexpand(true);
    letter.set_halign(gtk::Align::Center);
    tile.append(&letter);
    root.set_hexpand(false);
    root.append(&image);
    root.append(&tile);
    root
}

/// The app's own icon when there is one, else its first letter on an Umbra-style tile
/// in a colour picked by its name.
fn set_face(root: &gtk::Widget, icon: &str, name: &str, light: bool) {
    let Some(image) = root.first_child().and_downcast::<gtk::Image>() else { return };
    let Some(tile) = image.next_sibling() else { return };
    let Some(letter) = tile.first_child().and_downcast::<gtk::Label>() else { return };
    let own = icon_exists(icon);
    image.set_visible(own);
    tile.set_visible(!own);
    if own {
        if image.widget_name() != icon {
            if icon.starts_with('/') {
                image.set_from_file(Some(icon));
            } else {
                image.set_icon_name(Some(icon));
            }
            image.set_widget_name(icon);
        }
        return;
    }
    let first = name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_default();
    if letter.text() != first {
        letter.set_text(&first);
    }
    let swatch = super::dashboard::app_swatch(name);
    let key = format!("{swatch}{}", if light { "-light" } else { "" });
    if tile.widget_name() != key {
        let colour = format!("section-swatch-{swatch}");
        let mut classes = vec!["app-tile", colour.as_str()];
        if light {
            classes.push("light");
        }
        tile.set_css_classes(&classes);
        tile.set_widget_name(&key);
    }
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
            let b = widgets::hbox(0);
            b.add_css_class("proc-name-cell");
            let indent = widgets::hbox(0);
            let expander = gtk::Button::from_icon_name("pan-end-symbolic");
            expander.add_css_class("tree-expander");
            expander.set_valign(gtk::Align::Center);
            expander.set_focusable(false);
            let face = face("row", 26);
            // A process under its app: a line down from the app instead of a face.
            let guide = widgets::hbox(0);
            guide.add_css_class("proc-guide");
            guide.set_visible(false);
            // The name and, smaller, its count or PID: one label, so a narrow column cuts the PID first.
            let name = widgets::label("", "proc-name");
            name.set_hexpand(true);
            name.set_ellipsize(pango::EllipsizeMode::End);
            let tag = widgets::tag("", "");
            b.append(&indent);
            b.append(&expander);
            b.append(&face);
            b.append(&guide);
            b.append(&name);
            b.append(&tag);
            b.upcast()
        }
        Col::Cpu => {
            let b = widgets::hbox(10);
            b.set_halign(gtk::Align::End);
            let track = widgets::hbox(0);
            track.add_css_class("cpu-track");
            track.set_valign(gtk::Align::Center);
            let fill = widgets::hbox(0);
            fill.add_css_class("cpu-fill");
            fill.set_halign(gtk::Align::Start);
            track.append(&fill);
            let l = num_label();
            l.set_width_chars(6);
            b.append(&track);
            b.append(&l);
            b.upcast()
        }
        Col::User | Col::State => {
            let l = widgets::label("", "cell-dim");
            l.set_ellipsize(pango::EllipsizeMode::End);
            l.upcast()
        }
        _ => {
            let l = num_label();
            l.add_css_class("cell-dim");
            l.upcast()
        }
    }
}

fn children(w: &gtk::Widget) -> Vec<gtk::Widget> {
    let mut out = Vec::new();
    let mut c = w.first_child();
    while let Some(x) = c {
        c = x.next_sibling();
        out.push(x);
    }
    out
}

fn set_class(w: &impl IsA<gtk::Widget>, class: &str, on: bool) {
    if on != w.has_css_class(class) {
        if on {
            w.add_css_class(class);
        } else {
            w.remove_css_class(class);
        }
    }
}

fn set_text(l: &gtk::Label, text: &str) {
    if l.text() != text {
        l.set_text(text);
    }
}

fn plural(n: u32, one: &str, many: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {many}") }
}

/// Levels of the tree that indent; deeper ones line up with the last.
const MAX_INDENT: u32 = 8;

/// Width of the CPU bar, in pixels: full at 62.5%, as busy as a process gets for long.
const CPU_BAR: f64 = 52.0;

fn render(col: Col, row: &Row, w: &gtk::Widget, look: Look) {
    // A process under its app in Apps.
    let kid = look.apps && row.depth > 0;
    match col {
        Col::Name => {
            let parts = children(w);
            let [indent, expander, face, guide, name, tag] = parts.as_slice() else { return };
            let (Some(name), Some(tag)) = (name.downcast_ref::<gtk::Label>(), tag.downcast_ref::<gtk::Label>()) else {
                return;
            };
            set_class(w, "kid", kid);
            // Capped, so a deep branch doesn't push the numbers out of the window.
            indent.set_size_request(if look.tree { (row.depth.min(MAX_INDENT) * 16) as i32 } else { 0 }, -1);
            // Rows that can't open keep the space, so every name starts at the same place.
            expander.set_visible(look.apps || look.tree);
            expander.set_opacity(if row.has_children { 1.0 } else { 0.0 });
            expander.set_can_target(row.has_children);
            if let Some(b) = expander.downcast_ref::<gtk::Button>() {
                let icon = if row.expanded { "pan-down-symbolic" } else { "pan-end-symbolic" };
                if b.icon_name().as_deref() != Some(icon) {
                    b.set_icon_name(icon);
                }
            }
            face.set_visible(!kid);
            guide.set_visible(kid);
            let shown = if kid { row.p.name.as_str() } else { row.name() };
            if !kid {
                set_face(face, &row.icon, shown, look.light);
            }
            set_class(name, "strong", look.apps && !kid);
            let subtitle = match (&row.group, row.agg) {
                (Some(_), Some(a)) => plural(a.count, "process", "processes"),
                (Some(_), None) => String::new(),
                _ if look.apps && !kid => String::new(),
                _ => format!("PID {}", row.p.pid),
            };
            let mut markup = glib::markup_escape_text(shown).to_string();
            if !subtitle.is_empty() {
                markup.push_str(&format!(
                    "  <span size=\"smaller\" weight=\"normal\" alpha=\"58%\" font_features=\"tnum\">{subtitle}</span>"
                ));
            }
            if name.label() != markup {
                name.set_markup(&markup);
            }
            let tip = match &row.group {
                Some(g) => format!("{} {}", kind_name(g.kind), g.unit),
                None if row.p.cmdline.is_empty() => format!("[{}]", row.p.name),
                None => row.p.cmdline.clone(),
            };
            if name.tooltip_text().as_deref() != Some(tip.as_str()) {
                name.set_tooltip_text(Some(&tip));
            }
            match row.p.state {
                'T' if row.group.is_none() => {
                    set_text(tag, "Paused");
                    tag.add_css_class("danger");
                    tag.set_visible(true);
                }
                'Z' if row.group.is_none() => {
                    set_text(tag, "Zombie");
                    tag.add_css_class("danger");
                    tag.set_visible(true);
                }
                _ => tag.set_visible(false),
            }
        }
        Col::Cpu => {
            let Some(track) = w.first_child() else { return };
            let Some(l) = track.next_sibling().and_downcast::<gtk::Label>() else { return };
            let v = row.cpu();
            track.set_visible(!kid);
            if let Some(fill) = track.first_child() {
                let px = ((v * 1.6).min(100.0) / 100.0 * CPU_BAR).round() as i32;
                fill.set_visible(px > 0);
                fill.set_size_request(px.max(1), -1);
            }
            set_text(&l, &format!("{v:.1}%"));
            set_class(&l, "cell-dim", v < 0.05);
            set_class(&l, "small", kid);
        }
        _ => {
            let Some(l) = w.downcast_ref::<gtk::Label>() else { return };
            let text = match col {
                Col::Pid | Col::State if row.group.is_some() => String::new(),
                Col::Pid => row.p.pid.to_string(),
                Col::User => row.p.user.clone(),
                Col::Mem => fmt::bytes(row.mem() as f64),
                Col::Disk => {
                    let v = row.disk();
                    if v < 1.0 { "–".into() } else { fmt::rate(v) }
                }
                Col::Net => match row.conns() {
                    0 => "–".into(),
                    n => format!("{n} open"),
                },
                Col::Gpu => {
                    let v = row.gpu();
                    if v >= 0.05 {
                        format!("{v:.1}%")
                    } else if row.p.nvidia {
                        "dGPU".into()
                    } else {
                        "–".into()
                    }
                }
                Col::Threads => row.threads().to_string(),
                Col::State => state_name(row.p.state).to_string(),
                Col::Name | Col::Cpu => unreachable!(),
            };
            set_text(l, &text);
            set_class(l, "small", kid);
        }
    }
}

fn key(w: &impl IsA<glib::Object>) -> usize {
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
                    let pid = obj.row().p.pid;
                    toggle_expanded(pid);
                }
            });
        }
        item.set_child(Some(&w));
    });
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
        let (Some(obj), Some(w)) = (item.item().and_downcast::<ProcObject>(), item.child()) else { return };
        let Some(st) = state() else { return };
        let l = look(&st.borrow());
        render(col, &obj.row(), &w, l);
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

// ---------- Section headings ----------

fn header_factory() -> gtk::SignalListItemFactory {
    let f = gtk::SignalListItemFactory::new();
    f.connect_setup(|_, item| {
        let Some(h) = item.downcast_ref::<gtk::ListHeader>() else { return };
        let row = nyx_ui::sidebar::group_label("", false);
        row.add_css_class("proc-section");
        // The count follows the heading, not the far end of the row.
        if let Some(label) = row.first_child().and_downcast::<gtk::Label>() {
            label.set_hexpand(false);
            label.set_ellipsize(pango::EllipsizeMode::None);
        }
        let count = widgets::label("", "proc-section-count");
        count.add_css_class("tnum");
        row.append(&count);
        h.set_child(Some(&row));
    });
    f.connect_bind(|_, item| {
        let Some(h) = item.downcast_ref::<gtk::ListHeader>() else { return };
        let Some(st) = state() else { return };
        render_header(&st.borrow(), h);
        st.borrow_mut().headers.insert(key(h), h.clone());
    });
    f.connect_unbind(|_, item| {
        if let (Some(h), Some(st)) = (item.downcast_ref::<gtk::ListHeader>(), state()) {
            st.borrow_mut().headers.remove(&key(h));
        }
    });
    f
}

/// A heading in its section's colour, with how many rows are under it.
fn render_header(s: &State, h: &gtk::ListHeader) {
    let (Some(obj), Some(row)) = (h.item().and_downcast::<ProcObject>(), h.child()) else { return };
    let (Some(label), Some(count)) = (row.first_child().and_downcast::<gtk::Label>(), row.last_child().and_downcast::<gtk::Label>())
    else {
        return;
    };
    let sec = obj.row().section;
    set_text(&label, &sec.title().to_uppercase());
    for x in SECS {
        let class = format!("section-{}", x.colour());
        set_class(&row, &class, x == sec);
        set_class(&label, &class, x == sec);
    }
    set_text(&count, &s.counts[sec.index()].to_string());
}

fn refresh_headers(st: &Rc<RefCell<State>>) {
    let s = st.borrow();
    for h in s.headers.values() {
        render_header(&s, h);
    }
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
        || row.display.to_lowercase().contains(q)
        || row.p.cmdline.to_lowercase().contains(q)
        || row.p.pid.to_string() == q
        || row.p.user.to_lowercase() == q
        || row.title.to_lowercase().contains(q)
}

/// Apps for a group: an app with a window of its own, else yours or the system's.
fn group_section(g: &Group, windows: &HashSet<i32>, leader_uid: Option<u32>, my_uid: u32) -> Sec {
    let windowed = g.pids.iter().any(|p| windows.contains(p));
    if windowed && (g.kind == Kind::App || windows.contains(&g.leader)) {
        Sec::Apps
    } else if leader_uid == Some(my_uid) {
        Sec::Background
    } else {
        Sec::System
    }
}

fn proc_section(p: &Proc, windows: &HashSet<i32>, my_uid: u32) -> Sec {
    if windows.contains(&p.pid) {
        Sec::Apps
    } else if p.uid == my_uid && !p.kernel {
        Sec::Background
    } else {
        Sec::System
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
    let conns = snap.conns.as_ref().map(|c| &c.by_pid);
    let member: HashMap<i32, &Group> = snap.groups.iter().flat_map(|g| g.pids.iter().map(move |p| (*p, g))).collect();
    let uids: HashMap<i32, u32> = procs.iter().map(|p| (p.pid, p.uid)).collect();
    let (store, removed, added) = {
        let mut s = st.borrow_mut();
        let apps = s.view == View::Apps;
        let my_uid = s.my_uid;
        let section_of = |g: &Group| group_section(g, &window_pids, uids.get(&g.leader).copied(), my_uid);
        let headers = if apps { group_rows(&mut s, snap, procs, &section_of) } else { Vec::new() };
        let alive: HashSet<i32> = procs.iter().map(|p| p.pid).chain(headers.iter().map(|r| r.p.pid)).collect();
        let dead: Vec<i32> = s.objects.keys().filter(|p| !alive.contains(p)).copied().collect();
        let removed: Vec<ProcObject> = dead.iter().filter_map(|pid| s.objects.remove(pid)).collect();
        let mut added = Vec::new();
        // A process to show opens the group it's in; its leader stands for the group.
        if let Some(pid) = s.pending_reveal
            && let Some((id, g)) =
                headers.iter().find_map(|r| r.group.as_ref().filter(|g| g.pids.contains(&pid)).map(|g| (r.p.pid, g)))
        {
            s.opened_groups.insert(g.path.clone());
            if pid == g.leader {
                s.pending_reveal = Some(id);
            }
        }
        for row in headers {
            if let Some(obj) = s.objects.get(&row.p.pid) {
                let mut r = obj.row_mut();
                r.p = row.p;
                r.icon = row.icon;
                r.address = row.address;
                r.display = row.display;
                r.section = row.section;
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
            let class = win.map(|w| w.class.clone()).unwrap_or_default();
            let id_key = (p.pid, class);
            let (name, icon) = match s.identities.get(&id_key) {
                Some(i) => i.clone(),
                None => {
                    let i = top::app_identity(&p.app_name(), &[p.pid], snap);
                    s.identities.insert(id_key, i.clone());
                    i
                }
            };
            let group = member.get(&p.pid).copied();
            // A lone process stands for its app at the top of Apps, under the app's name.
            let display = if apps && group.is_none_or(|g| g.procs == 1) { name } else { String::new() };
            let section = group.map_or_else(|| proc_section(p, &window_pids, my_uid), section_of);
            let title = win.map(|w| w.title.clone()).unwrap_or_default();
            let address = win.map(|w| w.address.clone()).unwrap_or_default();
            let n = conns.and_then(|c| c.get(&p.pid)).copied().unwrap_or(0) as u32;
            if let Some(obj) = s.objects.get(&p.pid) {
                let mut r = obj.row_mut();
                r.p = p.clone();
                r.icon = icon;
                r.title = title;
                r.address = address;
                r.display = display;
                r.section = section;
                r.conns = n;
            } else {
                let row = Row { p: p.clone(), icon, title, address, display, section, conns: n, ..Default::default() };
                let obj = ProcObject::new(row);
                added.push(obj.clone());
                s.objects.insert(p.pid, obj);
            }
        }
        s.identities.retain(|(pid, _), _| alive.contains(pid));
        s.light = theme::palette().light;
        layout(&mut s);
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
    apply_view(&st);
    let pending = st.borrow().pending_reveal;
    if let Some(pid) = pending {
        let (exists, visible) = st.borrow().objects.get(&pid).map_or((false, false), |o| (true, o.row().visible));
        if visible || !exists {
            st.borrow_mut().pending_reveal = None;
        }
        if visible {
            // After `apply_view` has put the scroll back where it was.
            glib::idle_add_local_once(move || select_pid(pid));
        }
    }
    {
        let s = st.borrow();
        let threads = super::dashboard::thousands(snap.thread_count);
        s.totals.set_text(&format!("{} processes · {threads} threads", super::dashboard::thousands(snap.process_count as u64)));
        let none = s.selection.n_items() == 0;
        let q = s.query.trim();
        if none && !q.is_empty() {
            s.empty.set_text(&format!("No processes match “{q}”."));
        }
        s.table.set_visible_child_name(if none && !q.is_empty() { "empty" } else { "table" });
    }
    follow_width();
    refresh_details(snap);
}

/// Re-run the filter and sort, then refresh the cells on screen.
///
/// ListView keeps its top row anchored, so after a re-sort it would scroll to
/// wherever that row moved. We hold the scroll position still instead.
fn apply_view(st: &Rc<RefCell<State>>) {
    let (filter, sorter, sections, adj) = {
        let s = st.borrow();
        (s.filter.clone(), s.sorter.clone(), s.section_sorter.clone(), s.scroll.vadjustment())
    };
    let before = adj.value();
    filter.changed(gtk::FilterChange::Different);
    sections.changed(gtk::SorterChange::Different);
    sorter.changed(gtk::SorterChange::Different);
    // At the top, the top: scrolling to the first row would hide the heading above it.
    let at = if before < 1.0 { 0.0 } else { before };
    adj.set_value(at);
    glib::idle_add_local_once(move || adj.set_value(at));
    let (l, bound): (Look, Vec<(ProcObject, Col, gtk::Widget)>) = {
        let s = st.borrow();
        (look(&s), s.bound.values().cloned().collect())
    };
    for (obj, col, w) in bound {
        render(col, &obj.row(), &w, l);
    }
    refresh_headers(st);
}

/// Per pid: visible, order, depth, has children, expanded, collapsed totals.
type Placement = (bool, u32, u32, bool, bool, Option<Agg>);

/// A row for each app or service with more than one process (Apps).
fn group_rows(s: &mut State, snap: &Snapshot, procs: &[Proc], section_of: &dyn Fn(&Group) -> Sec) -> Vec<Row> {
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
        let mut row = group_row(id, g, snap, by_pid.get(&g.leader).copied());
        row.section = section_of(g);
        rows.push(row);
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
        name: name.clone(),
        cmdline: leader.map(|l| l.cmdline.clone()).unwrap_or_default(),
        uid: leader.map_or(0, |l| l.uid),
        user: leader.map(|l| l.user.clone()).unwrap_or_default(),
        start: leader.map_or(0.0, |l| l.start),
        nice: leader.map_or(0, |l| l.nice),
        cgroup: g.path.clone(),
        ..Default::default()
    };
    let group = GroupRow {
        path: g.path.clone(),
        unit: g.unit.clone(),
        kind: g.kind,
        pids: g.pids.clone(),
        leader: g.leader,
        memory: g.mem,
    };
    Row { p, icon, address, display: name, group: Some(group), ..Default::default() }
}

/// Processes added up, as a collapsed branch shows them.
fn sum<'a>(rows: impl Iterator<Item = &'a Row>) -> Agg {
    rows.fold(Agg::default(), |mut a, r| {
        a.cpu += r.p.cpu;
        a.mem += r.p.rss;
        a.disk += r.p.read_bps + r.p.write_bps;
        a.gpu = (a.gpu + r.p.gpu).min(100.0);
        a.threads += r.p.threads;
        a.conns += r.conns;
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
        let mut a = sum(pids.iter().map(|p| &rows[p]));
        if let Some(r) = rows.get_mut(id) {
            if let Some(m) = r.group.as_ref().and_then(|g| g.memory) {
                a.mem = m;
            }
            r.agg = Some(a);
        }
    }
    members
}

/// Apps: group rows and lone processes under their headings, sorted within each,
/// each group's processes under it while it's open. A filter shows the groups with
/// a match, opened to the matching processes.
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
    top.sort_by(|a, b| rows[a].section.cmp(&rows[b].section).then_with(|| cmp(a, b)));
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

/// All and Mine as a tree: each process under the one that started it, open unless
/// the user closed it. A filter keeps the path down to each match open.
fn place_tree(
    rows: &HashMap<i32, Row>,
    include: &dyn Fn(&Row) -> bool,
    closed: &HashSet<i32>,
    q: &str,
    cmp: impl Fn(&i32, &i32) -> Ordering,
) -> HashMap<i32, Placement> {
    let included: HashSet<i32> = rows.values().filter(|r| include(r)).map(|r| r.p.pid).collect();
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    let mut roots = Vec::new();
    for pid in &included {
        let r = &rows[pid];
        if r.p.ppid != r.p.pid && included.contains(&r.p.ppid) {
            children.entry(r.p.ppid).or_default().push(*pid);
        } else {
            roots.push(*pid);
        }
    }
    for list in children.values_mut() {
        list.sort_by(&cmp);
    }
    roots.sort_by(&cmp);
    fn agg(pid: i32, rows: &HashMap<i32, Row>, children: &HashMap<i32, Vec<i32>>, memo: &mut HashMap<i32, Agg>) -> Agg {
        if let Some(a) = memo.get(&pid) {
            return *a;
        }
        let mut a = sum(std::iter::once(&rows[&pid]));
        for c in children.get(&pid).into_iter().flatten() {
            let ca = agg(*c, rows, children, memo);
            a.cpu += ca.cpu;
            a.mem += ca.mem;
            a.disk += ca.disk;
            a.gpu = (a.gpu + ca.gpu).min(100.0);
            a.threads += ca.threads;
            a.conns += ca.conns;
            a.count += ca.count;
        }
        memo.insert(pid, a);
        a
    }
    let mut memo = HashMap::new();
    let keep: Option<HashSet<i32>> = (!q.is_empty()).then(|| {
        let parents: HashMap<i32, i32> = included.iter().map(|p| (*p, rows[p].p.ppid)).collect();
        let hits: HashSet<i32> = included.iter().filter(|p| matches(&rows[p], q)).copied().collect();
        with_ancestors(&hits, &parents)
    });
    let mut out = HashMap::new();
    let mut order = 0u32;
    let mut stack: Vec<(i32, u32, bool)> = roots.iter().rev().map(|p| (*p, 0, true)).collect();
    while let Some((pid, depth, shown)) = stack.pop() {
        let kids = children.get(&pid).cloned().unwrap_or_default();
        let has_children = !kids.is_empty();
        let (shown, expanded) = match &keep {
            Some(keep) => (shown && keep.contains(&pid), has_children && kids.iter().any(|k| keep.contains(k))),
            None => (shown, has_children && !closed.contains(&pid)),
        };
        let a = if has_children && !expanded { Some(agg(pid, rows, &children, &mut memo)) } else { None };
        out.insert(pid, (shown, order, depth, has_children, expanded, a));
        order += 1;
        for k in kids.iter().rev() {
            stack.push((*k, depth + 1, shown && expanded));
        }
    }
    out
}

/// Work out visibility, order, depth and aggregates for the current view.
fn layout(s: &mut State) {
    let (col, desc) = sort_state(s);
    let q = s.query.to_lowercase();
    let q = q.trim();
    let show_kernel = prefs::get().kernel_threads;
    let mut rows: HashMap<i32, Row> = s.objects.iter().map(|(pid, o)| (*pid, o.row().clone())).collect();
    let apps = s.view == View::Apps;
    let members = if apps { add_group_totals(&mut rows) } else { HashMap::new() };
    let rows = rows;
    let cmp = |a: &i32, b: &i32| {
        let o = compare(col, &rows[a], &rows[b]);
        if desc { o.reverse() } else { o }
    };
    let mut counts = [0usize; 3];
    let out: HashMap<i32, Placement> = if apps {
        let out = place_grouped(&rows, members, &s.opened_groups, q, show_kernel, cmp);
        for (id, (shown, _, depth, ..)) in &out {
            if *shown && *depth == 0 {
                counts[rows[id].section.index()] += 1;
            }
        }
        out
    } else {
        let (my_uid, mine) = (s.my_uid, s.view == View::Mine);
        let include = move |r: &Row| {
            r.group.is_none() && (show_kernel || !r.p.kernel || mine) && (!mine || (r.p.uid == my_uid && !r.p.kernel))
        };
        if s.tree {
            place_tree(&rows, &include, &s.closed, q, cmp)
        } else {
            let mut list: Vec<i32> = rows.values().filter(|r| include(r) && matches(r, q)).map(|r| r.p.pid).collect();
            list.sort_by(cmp);
            list.iter().enumerate().map(|(i, pid)| (*pid, (true, i as u32, 0, false, false, None))).collect()
        }
    };
    s.counts = counts;
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
        } else if !s.closed.remove(&pid) {
            s.closed.insert(pid);
        }
    }
    refilter();
}

/// The row above `pid` in the current view: its group row in Apps, else its parent.
fn parent_row(s: &State, pid: i32, ppid: i32) -> i32 {
    if s.view != View::Apps {
        return ppid;
    }
    s.objects.iter().find(|(_, o)| o.row().group.as_ref().is_some_and(|g| g.pids.contains(&pid))).map_or(ppid, |(id, _)| *id)
}

/// Apps shows sections, with their headings; All and Mine are one list.
fn set_sections(s: &State) {
    if s.view == View::Apps {
        s.sorted.set_section_sorter(Some(&s.section_sorter));
        s.view_widget.set_header_factory(Some(&s.header_factory));
    } else {
        s.view_widget.set_header_factory(None::<&gtk::ListItemFactory>);
        s.sorted.set_section_sorter(None::<&gtk::Sorter>);
    }
}

fn set_view(v: View) {
    let Some(st) = state() else { return };
    let segments = {
        let mut s = st.borrow_mut();
        if s.view == v {
            return;
        }
        s.view = v;
        s.segments.clone()
    };
    set_sections(&st.borrow());
    for (view, b) in segments {
        if view == v && !b.is_active() {
            b.set_active(true);
        }
    }
    prefs::update(|p| p.process_view = v.id().into());
    refilter();
}

fn set_tree(on: bool) {
    let Some(st) = state() else { return };
    let check = {
        let mut s = st.borrow_mut();
        if s.tree == on {
            return;
        }
        s.tree = on;
        s.tree_check.clone()
    };
    if check.is_active() != on {
        check.set_active(on);
    }
    prefs::update(|p| p.process_tree = on);
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

/// Show one process in the table (from the Dashboard, Go to, Services). In Apps its
/// app opens to it; an app's own first process shows the app.
pub fn reveal(pid: i32) {
    window::navigate("processes");
    let Some(st) = state() else { return };
    let (search, other_users) = {
        let mut s = st.borrow_mut();
        s.query.clear();
        let theirs = s.objects.get(&pid).is_some_and(|o| o.row().p.uid != s.my_uid);
        (s.search.clone(), s.view == View::Mine && theirs)
    };
    search.set_text("");
    if other_users {
        set_view(View::All);
    }
    {
        let mut s = st.borrow_mut();
        if s.view != View::Apps && s.tree {
            let parents: HashMap<i32, i32> = s.objects.iter().map(|(p, o)| (*p, o.row().p.ppid)).collect();
            let path = with_ancestors(&HashSet::from([pid]), &parents);
            s.closed.retain(|p| !path.contains(p));
        }
        s.pending_reveal = Some(pid);
    }
    refilter();
}

/// Open Processes sorted by a column ("cpu", "mem"), biggest first (from the Dashboard).
pub fn show_sorted(column: &str) {
    window::navigate("processes");
    let Some(st) = state() else { return };
    let (cv, c, adj) = {
        let s = st.borrow();
        let col = COLUMNS.iter().find(|(_, id, ..)| *id == column).map(|(c, ..)| *c);
        let c = col.and_then(|col| s.columns.iter().find(|(x, _)| *x == col).map(|(_, c)| c.clone()));
        (s.view_widget.clone(), c, s.scroll.vadjustment())
    };
    if let Some(c) = c {
        cv.sort_by_column(Some(&c), gtk::SortType::Descending);
        adj.set_value(0.0);
    }
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

thread_local! {
    /// The ending a key press or a menu choice asked for, waiting to be confirmed.
    static ARMED: Cell<Option<(Signal, Instant)>> = const { Cell::new(None) };
}

/// End or kill the selection. With "Ask before ending tasks" on, the first request
/// only says how to confirm it; asking again within 3 seconds does it.
fn end_or_ask(sig: Signal, again: &str) {
    let Some(st) = state() else { return };
    let t = targets(&st.borrow());
    if t.is_empty() {
        return;
    }
    let ready = matches!(ARMED.get(), Some((s, at)) if s == sig && at.elapsed().as_secs_f64() < 3.0);
    if !prefs::get().confirm_kill || ready {
        ARMED.set(None);
        end_selected(sig);
    } else {
        ARMED.set(Some((sig, Instant::now())));
        let what = if t.len() == 1 { t[0].1.clone() } else { format!("{} processes", t.len()) };
        let verb = if sig == Signal::Kill { "kill" } else { "end" };
        window::toast(&format!("{again} to {verb} {what}"));
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

fn first_selected() -> Option<ProcObject> {
    state().and_then(|s| selected(&s.borrow()).first().cloned())
}

fn object(pid: i32) -> Option<ProcObject> {
    state().and_then(|s| s.borrow().objects.get(&pid).cloned())
}

// ---------- Actions and menus ----------

const PRIORITIES: &[(&str, i32)] = &[
    ("Highest (-15)", -15),
    ("High (-5)", -5),
    ("Normal (0)", 0),
    ("Low (5)", 5),
    ("Lowest (10)", 10),
    ("Background (19)", 19),
];

fn priority_name(nice: i32) -> String {
    match PRIORITIES.iter().find(|(_, n)| *n == nice) {
        Some((label, _)) => label.to_string(),
        None => format!("Custom ({nice})"),
    }
}

/// What the table, its context menu, the inspector and the foot bar can do to the
/// selection, as `proc.*` actions. Those that signal go to every selected process.
fn proc_actions() -> gio::SimpleActionGroup {
    let group = gio::SimpleActionGroup::new();
    let add = |name: &str, f: fn()| {
        let a = gio::SimpleAction::new(name, None);
        a.connect_activate(move |_, _| f());
        group.add_action(&a);
    };
    add("switch", || {
        if let Some(o) = first_selected() {
            actions::focus_window(&o.row().address);
        }
    });
    add("close", || {
        if let Some(o) = first_selected() {
            actions::close_window(&o.row().address);
        }
    });
    add("pause", pause_selected);
    add("end", || end_selected(Signal::Term));
    add("kill", || end_or_ask(Signal::Kill, "Choose Kill again"));
    add("tree", || {
        if let Some(o) = state().and_then(|s| selected(&s.borrow()).first().cloned()) {
            let pid = o.row().pid();
            show_in_tree(pid);
        }
    });
    add("location", || {
        if let Some(o) = first_selected() {
            actions::open_location(o.row().pid());
        }
    });
    add("copy", || {
        if let Some(o) = first_selected() {
            actions::copy(&o.row().p.cmdline);
        }
    });
    let priority = gio::SimpleAction::new_stateful("priority", Some(glib::VariantTy::INT32), &i32::MIN.to_variant());
    priority.connect_activate(|_, v| {
        let Some(nice) = v.and_then(|v| v.get::<i32>()) else { return };
        let t = state().map(|s| targets(&s.borrow())).unwrap_or_default();
        if !t.is_empty() {
            actions::renice(&t, nice);
        }
    });
    group.add_action(&priority);
    group
}

/// Turn the actions on or off for what's selected, and tick its shared priority.
fn refresh_actions(s: &State) {
    let sel = selected(s);
    let one = sel.len() == 1;
    let has_window = one && sel.first().is_some_and(|o| !o.row().address.is_empty());
    let live_sel: Vec<ProcObject> = selected_procs(s).into_iter().filter(|o| !o.row().p.kernel).collect();
    let enable = |name: &str, on: bool| {
        if let Some(a) = s.actions.lookup_action(name).and_downcast::<gio::SimpleAction>()
            && a.is_enabled() != on
        {
            a.set_enabled(on);
        }
    };
    enable("switch", has_window);
    enable("close", has_window);
    for name in ["pause", "end", "kill", "priority"] {
        enable(name, !live_sel.is_empty());
    }
    for name in ["tree", "location", "copy"] {
        enable(name, one);
    }
    let nice = live_sel.first().map(|o| o.row().p.nice);
    let shared = nice.filter(|v| live_sel.iter().all(|o| o.row().p.nice == *v)).unwrap_or(i32::MIN);
    if let Some(a) = s.actions.lookup_action("priority")
        && a.state().and_then(|v| v.get::<i32>()) != Some(shared)
    {
        a.change_state(&shared.to_variant());
    }
}

/// The Priority submenu, with a custom value at its foot (see `custom_priority`).
fn priority_menu() -> gio::Menu {
    let prio = gio::Menu::new();
    for (label, nice) in PRIORITIES {
        let item = gio::MenuItem::new(Some(label), None);
        item.set_action_and_target_value(Some("proc.priority"), Some(&nice.to_variant()));
        prio.append_item(&item);
    }
    let custom = gio::Menu::new();
    let item = gio::MenuItem::new(None, None);
    item.set_attribute_value("custom", Some(&"priority-custom".to_variant()));
    custom.append_item(&item);
    prio.append_section(None, &custom);
    prio
}

/// Any value from -20 to 19, set on the selection.
fn custom_priority(pop: &gtk::PopoverMenu) {
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
        let t = state().map(|s| targets(&s.borrow())).unwrap_or_default();
        if !t.is_empty() {
            actions::renice(&t, spin2.value_as_int());
        }
    });
    custom.append(&apply);
    pop.add_child(&custom, "priority-custom");
    // Start at the selection's current value.
    pop.connect_show(move |_| {
        if let Some(o) = first_selected() {
            let pid = o.row().pid();
            if let Some(p) = object(pid) {
                spin.set_value(p.row().p.nice as f64);
            }
        }
    });
}

fn context_model() -> gio::Menu {
    let menu = gio::Menu::new();
    let window = gio::Menu::new();
    window.append(Some("Switch to"), Some("proc.switch"));
    window.append(Some("Close window"), Some("proc.close"));
    menu.append_section(None, &window);
    let control = gio::Menu::new();
    control.append(Some("Pause / resume"), Some("proc.pause"));
    control.append_submenu(Some("Priority"), &priority_menu());
    menu.append_section(None, &control);
    let info = gio::Menu::new();
    info.append(Some("Show in the tree"), Some("proc.tree"));
    info.append(Some("Open location"), Some("proc.location"));
    info.append(Some("Copy command"), Some("proc.copy"));
    menu.append_section(None, &info);
    let stop = gio::Menu::new();
    stop.append(Some("End task"), Some("proc.end"));
    stop.append(Some("Kill"), Some("proc.kill"));
    menu.append_section(None, &stop);
    menu
}

/// The inspector's More: what doesn't have a button of its own.
fn more_model() -> gio::Menu {
    let menu = gio::Menu::new();
    let control = gio::Menu::new();
    control.append_submenu(Some("Priority"), &priority_menu());
    menu.append_section(None, &control);
    let info = gio::Menu::new();
    info.append(Some("Open location"), Some("proc.location"));
    info.append(Some("Copy command"), Some("proc.copy"));
    info.append(Some("Show in the tree"), Some("proc.tree"));
    menu.append_section(None, &info);
    let window = gio::Menu::new();
    window.append(Some("Close window"), Some("proc.close"));
    menu.append_section(None, &window);
    let stop = gio::Menu::new();
    stop.append(Some("Kill"), Some("proc.kill"));
    menu.append_section(None, &stop);
    menu
}

// ---------- Inspector ----------

/// The buttons the inspector and the foot bar share.
struct Buttons {
    focus: gtk::Button,
    pause: gtk::Button,
    end: gtk::Button,
}

fn buttons() -> Buttons {
    let focus = widgets::labeled_button("activity-window-symbolic", "Switch to");
    focus.set_action_name(Some("proc.switch"));
    focus.set_tooltip_text(Some("Bring its window forward"));
    let pause = widgets::icon_button("media-playback-pause-symbolic", "Pause");
    pause.connect_clicked(|_| pause_selected());
    let end = kill_button("End task", "Click again to end", Signal::Term);
    end.set_tooltip_text(Some("Ask it to quit (SIGTERM)"));
    for b in [&focus, &pause, &end] {
        b.set_focusable(false);
        b.set_valign(gtk::Align::Center);
    }
    Buttons { focus, pause, end }
}

struct Inspector {
    root: gtk::Box,
    stack: gtk::Stack,
    face: gtk::Box,
    title: gtk::Label,
    subtitle: gtk::Label,
    cpu_now: gtk::Label,
    mem_now: gtk::Label,
    cpu_graph: gtk::Box,
    mem_graph: gtk::Box,
    /// Whose history the graphs show.
    shows: RefCell<String>,
    about: Vec<(gtk::Box, gtk::Label)>,
    buttons: Buttons,
}

/// The facts under About it, in order.
const ABOUT: &[&str] = &["Started", "Threads", "Disk", "GPU", "Network", "Priority", "Open files", "Parent", "Command"];

fn inspector() -> Inspector {
    let root = widgets::vbox(0);
    root.add_css_class("proc-inspector");
    root.set_size_request(INSPECTOR_WIDTH, -1);
    root.set_hexpand(false);
    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    let none = widgets::label("Choose an app or process to see its graphs and details.", "empty-state");
    none.set_wrap(true);
    none.set_justify(gtk::Justification::Center);
    none.set_xalign(0.5);
    none.set_valign(gtk::Align::Center);
    none.set_margin_start(24);
    none.set_margin_end(24);
    stack.add_named(&none, Some("none"));

    let info = widgets::vbox(0);
    let head = widgets::hbox(12);
    head.add_css_class("insp-head");
    let face = face("large", 40);
    let text = widgets::vbox(1);
    text.set_hexpand(true);
    let title = widgets::label("", "insp-title");
    title.set_ellipsize(pango::EllipsizeMode::End);
    let subtitle = widgets::label("", "insp-sub");
    subtitle.set_wrap(true);
    subtitle.set_wrap_mode(pango::WrapMode::WordChar);
    subtitle.add_css_class("tnum");
    text.append(&title);
    text.append(&subtitle);
    head.append(&face);
    head.append(&text);
    info.append(&head);

    let body = widgets::vbox(0);
    body.add_css_class("insp-body");
    let lab = |text: &str| {
        let row = widgets::hbox(6);
        row.add_css_class("insp-lab");
        row.append(&widgets::label(text, ""));
        let now = widgets::label("", "insp-now");
        now.add_css_class("tnum");
        row.append(&now);
        (row, now)
    };
    let graph_card = || {
        let b = widgets::vbox(0);
        b.add_css_class("insp-graph");
        b
    };
    let (cpu_lab, cpu_now) = lab("Processor");
    let cpu_graph = graph_card();
    let (mem_lab, mem_now) = lab("Memory");
    let mem_graph = graph_card();
    let (about_lab, _) = lab("About it");
    let card = widgets::vbox(0);
    card.add_css_class("insp-card");
    let mut about = Vec::new();
    for k in ABOUT {
        let row = widgets::hbox(10);
        row.add_css_class("insp-row");
        let key = widgets::label(k, "insp-key");
        key.set_valign(gtk::Align::Start);
        let value = widgets::label("", "insp-value");
        value.add_css_class("tnum");
        value.set_hexpand(true);
        value.set_valign(gtk::Align::Start);
        value.set_xalign(1.0);
        value.set_yalign(0.0);
        value.set_justify(gtk::Justification::Right);
        value.set_wrap(true);
        value.set_wrap_mode(pango::WrapMode::WordChar);
        value.set_selectable(*k == "Command");
        if *k == "Command" {
            value.set_lines(3);
            value.set_ellipsize(pango::EllipsizeMode::End);
            value.add_css_class("mono");
        }
        // "Parent" links to the parent process.
        value.connect_activate_link(|_, uri| {
            if let Some(pid) = uri.strip_prefix("pid:").and_then(|p| p.parse().ok()) {
                glib::idle_add_local_once(move || reveal(pid));
            }
            glib::Propagation::Stop
        });
        row.append(&key);
        row.append(&value);
        card.append(&row);
        about.push((row, value));
    }
    for w in [cpu_lab.upcast_ref::<gtk::Widget>(), cpu_graph.upcast_ref(), mem_lab.upcast_ref(), mem_graph.upcast_ref()] {
        body.append(w);
    }
    body.append(&about_lab);
    body.append(&card);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .vexpand(true)
        .child(&body)
        .build();
    info.append(&scroll);

    let buttons = buttons();
    let foot = widgets::hbox(6);
    foot.add_css_class("insp-foot");
    let more_pop = gtk::PopoverMenu::from_model(Some(&more_model()));
    custom_priority(&more_pop);
    let more = gtk::MenuButton::new();
    more.set_icon_name("view-more-horizontal-symbolic");
    more.set_has_frame(false);
    more.set_tooltip_text(Some("More: priority, open location, copy command, show in the tree, kill"));
    more.set_popover(Some(&more_pop));
    more.set_valign(gtk::Align::Center);
    let spacer = widgets::hbox(0);
    spacer.set_hexpand(true);
    foot.append(&buttons.focus);
    foot.append(&buttons.pause);
    foot.append(&more);
    foot.append(&spacer);
    foot.append(&buttons.end);
    info.append(&foot);
    stack.add_named(&info, Some("info"));
    root.append(&stack);

    Inspector {
        root,
        stack,
        face,
        title,
        subtitle,
        cpu_now,
        mem_now,
        cpu_graph,
        mem_graph,
        shows: RefCell::new(String::new()),
        about,
        buttons,
    }
}

/// The selection's name and actions along the foot, while the window is too narrow for the inspector.
struct Foot {
    root: gtk::Box,
    face: gtk::Box,
    title: gtk::Label,
    subtitle: gtk::Label,
    buttons: Buttons,
}

fn foot() -> Foot {
    let root = widgets::hbox(8);
    root.add_css_class("proc-foot");
    root.set_visible(false);
    let face = face("foot", 30);
    let text = widgets::vbox(0);
    text.set_hexpand(true);
    let title = widgets::label("", "foot-title");
    title.set_ellipsize(pango::EllipsizeMode::End);
    let subtitle = widgets::label("", "foot-sub");
    subtitle.add_css_class("tnum");
    subtitle.set_ellipsize(pango::EllipsizeMode::End);
    text.append(&title);
    text.append(&subtitle);
    let buttons = buttons();
    root.append(&face);
    root.append(&text);
    root.append(&buttons.focus);
    root.append(&buttons.pause);
    root.append(&buttons.end);
    Foot { root, face, title, subtitle, buttons }
}

/// End/Kill buttons: two clicks while "Ask before ending tasks" is on.
fn kill_button(label: &str, armed: &str, sig: Signal) -> gtk::Button {
    widgets::two_click_if(label, armed, || prefs::get().confirm_kill, move || end_selected(sig))
}

/// Set a button's label unless it's waiting for its confirming click.
fn relabel(b: &gtk::Button, text: &str) {
    if !b.has_css_class("armed") && b.label().as_deref() != Some(text) {
        b.set_label(text);
    }
}

/// `rows` selected (an app counts once), `n` processes they hold that signals can reach.
fn refresh_buttons(b: &Buttons, has_window: bool, rows: usize, n: usize, all_stopped: bool) {
    b.focus.set_visible(has_window);
    let verb = if all_stopped { "Resume" } else { "Pause" };
    let tip = if rows > 1 { format!("{verb} {n} processes") } else { verb.to_string() };
    if b.pause.tooltip_text().as_deref() != Some(tip.as_str()) {
        b.pause.set_tooltip_text(Some(&tip));
        b.pause.update_property(&[gtk::accessible::Property::Label(&tip)]);
        b.pause.set_icon_name(if all_stopped { "media-playback-start-symbolic" } else { "media-playback-pause-symbolic" });
    }
    relabel(&b.end, &if rows > 1 { format!("End {rows} tasks") } else { "End task".to_string() });
    b.pause.set_sensitive(n > 0);
    b.end.set_sensitive(n > 0);
}

/// "Today 14:02", "Yesterday 09:10", "Nov 2 18:40".
fn when(t: &glib::DateTime, now: &glib::DateTime) -> String {
    let time = t.format("%H:%M").map(|s| s.to_string()).unwrap_or_default();
    let day = |d: &glib::DateTime| (d.year(), d.day_of_year());
    if day(t) == day(now) {
        format!("Today {time}")
    } else if now.add_days(-1).is_ok_and(|y| day(&y) == day(t)) {
        format!("Yesterday {time}")
    } else {
        t.format("%b %-e %H:%M").map(|s| s.to_string()).unwrap_or(time)
    }
}

/// Reads "12 MiB/s write", "2 MiB/s read · 10 MiB/s write" or "–".
fn disk_text(read: f64, write: f64) -> String {
    match (read >= 1.0, write >= 1.0) {
        (true, true) => format!("{} read · {} write", fmt::rate(read), fmt::rate(write)),
        (true, false) => format!("{} read", fmt::rate(read)),
        (false, true) => format!("{} write", fmt::rate(write)),
        (false, false) => "–".into(),
    }
}

fn refresh_details(snap: &Snapshot) {
    let Some(st) = state() else { return };
    let s = st.borrow();
    refresh_actions(&s);
    let (Some(d), Some(f)) = (&s.inspector, &s.foot) else { return };
    let sel = selected(&s);
    let Some(obj) = sel.first() else {
        d.stack.set_visible_child_name("none");
        f.root.set_visible(false);
        return;
    };
    let r = obj.row().clone();
    d.stack.set_visible_child_name("info");
    f.root.set_visible(s.narrow);

    let kid = s.view == View::Apps && r.depth > 0;
    let name = if kid { r.p.name.clone() } else { r.name().to_string() };
    let procs: Vec<Proc> = match &r.group {
        Some(g) => g.pids.iter().filter_map(|p| s.objects.get(p)).map(|o| o.row().p.clone()).collect(),
        None => vec![r.p.clone()],
    };
    let count = procs.len() as u32;
    let cpu = procs.iter().map(|p| p.cpu).sum::<f64>();
    let mem = r.mem();
    let user = if r.p.uid == s.my_uid && !r.p.kernel { "you".to_string() } else { r.p.user.clone() };
    let now = glib::DateTime::now_local().ok();
    let started = now.as_ref().and_then(|n| {
        let boot = n.to_unix() as f64 - snap.cpu.uptime;
        glib::DateTime::from_unix_local((boot + r.p.start) as i64).ok().map(|t| (t, n.clone()))
    });
    let since = started.as_ref().and_then(|(t, _)| t.format("%H:%M").ok()).map(|t| format!(" · since {t}")).unwrap_or_default();
    let more = if sel.len() > 1 { format!(" · {} selected", sel.len()) } else { String::new() };
    let head = match &r.group {
        Some(_) => format!("{} · {user}{since}{more}", plural(count, "process", "processes")),
        None => format!("PID {} · {user}{since}{more}", r.p.pid),
    };

    set_face(d.face.upcast_ref(), &r.icon, &name, s.light);
    set_text(&d.title, &name);
    set_text(&d.subtitle, &head);
    set_text(&d.cpu_now, &format!("{cpu:.1}%"));
    set_text(&d.mem_now, &fmt::bytes(mem as f64));

    // Graphs: the group's history added up, or the process's own.
    let shows = match &r.group {
        Some(g) => format!("group.{}", g.path),
        None => format!("proc.{}", r.p.pid),
    };
    if *d.shows.borrow() != shows {
        match &r.group {
            Some(g) => live::track_group(&g.path),
            None => live::track(r.p.pid),
        }
        let tone = super::dashboard::app_swatch(&name);
        let memory_tone = if tone == "violet" { "blue" } else { "violet" };
        for (holder, key, tone, scale, f) in [
            (&d.cpu_graph, format!("{shows}.cpu"), tone, graph::Scale::Auto { floor: 5.0 }, fmt::pct as fn(f64) -> String),
            (&d.mem_graph, format!("{shows}.mem"), memory_tone, graph::Scale::Auto { floor: 1e6 }, fmt::bytes),
        ] {
            while let Some(c) = holder.first_child() {
                holder.remove(&c);
            }
            holder.append(&graph::sparkline(vec![graph::Series::new(key, "", graph::Tone::Swatch(tone))], scale, 46, f));
        }
        *d.shows.borrow_mut() = shows;
    }

    let nices: HashSet<i32> = procs.iter().map(|p| p.nice).collect();
    let fds: u32 = procs.iter().map(|p| p.fds).sum();
    let gpu = r.gpu();
    let parent = (r.group.is_none() && r.p.ppid > 0).then(|| match s.objects.get(&r.p.ppid) {
        Some(o) => format!(
            "<a href=\"pid:{0}\" title=\"Show the parent process\">{1} ({0})</a>",
            r.p.ppid,
            glib::markup_escape_text(&o.row().p.name)
        ),
        None => r.p.ppid.to_string(),
    });
    let conns = r.conns();
    let values: [Option<String>; 9] = [
        started.as_ref().map(|(t, n)| when(t, n)),
        Some(procs.iter().map(|p| p.threads).sum::<u32>().to_string()),
        Some(disk_text(procs.iter().map(|p| p.read_bps).sum(), procs.iter().map(|p| p.write_bps).sum())),
        Some(if gpu >= 0.05 {
            fmt::pct(gpu)
        } else if procs.iter().any(|p| p.nvidia) {
            "Uses the NVIDIA GPU".into()
        } else {
            "–".into()
        }),
        (conns > 0).then(|| plural(conns, "open connection", "open connections")),
        Some(if nices.len() == 1 { nices.iter().next().map_or_else(String::new, |n| priority_name(*n)) } else { "Mixed".into() }),
        (fds > 0).then(|| fds.to_string()),
        parent,
        Some(if r.p.cmdline.is_empty() { format!("[{}] (kernel thread)", r.p.name) } else { r.p.cmdline.clone() }),
    ];
    for ((row, label), value) in d.about.iter().zip(values) {
        row.set_visible(value.is_some());
        let value = value.unwrap_or_default();
        if label.label() != value {
            if value.starts_with("<a ") {
                label.set_markup(&value);
            } else {
                label.set_text(&value);
            }
        }
    }

    let has_window = !r.address.is_empty() && sel.len() == 1;
    let live_sel: Vec<ProcObject> = selected_procs(&s).into_iter().filter(|o| !o.row().p.kernel).collect();
    let n = live_sel.len();
    let all_stopped = n > 0 && live_sel.iter().all(|o| o.row().p.state == 'T');
    refresh_buttons(&d.buttons, has_window, sel.len(), n, all_stopped);

    set_face(f.face.upcast_ref(), &r.icon, &name, s.light);
    set_text(&f.title, &name);
    let lead = if r.group.is_some() { plural(count, "process", "processes") } else { format!("PID {}", r.p.pid) };
    set_text(&f.subtitle, &format!("{lead} · {} CPU · {}{more}", fmt::pct(cpu), fmt::bytes(mem as f64)));
    refresh_buttons(&f.buttons, has_window, sel.len(), n, all_stopped);
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

/// Whether a point is on the column titles.
fn on_titles(cv: &gtk::ColumnView, x: f64, y: f64) -> bool {
    let mut w = cv.pick(x, y, gtk::PickFlags::DEFAULT);
    while let Some(widget) = w {
        if widget.parent().as_ref() == Some(cv.upcast_ref()) {
            return widget.css_name() == "header";
        }
        w = widget.parent();
    }
    false
}

fn position_of(obj: &ProcObject) -> Option<u32> {
    let st = state()?;
    let s = st.borrow();
    (0..s.selection.n_items()).find(|i| s.selection.item(*i).and_downcast::<ProcObject>().as_ref() == Some(obj))
}

/// Right-click: a row's actions, or the column chooser on the titles.
fn context_menu(cv: &gtk::ColumnView) {
    let pop = gtk::PopoverMenu::from_model(Some(&context_model()));
    custom_priority(&pop);
    pop.set_parent(cv);
    pop.set_has_arrow(false);
    pop.set_halign(gtk::Align::Start);
    let p2 = pop.clone();
    cv.connect_destroy(move |_| p2.unparent());

    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_SECONDARY);
    let cv2 = cv.clone();
    click.connect_pressed(move |g, _, x, y| {
        if on_titles(&cv2, x, y) {
            g.set_state(gtk::EventSequenceState::Claimed);
            if let Some(st) = state() {
                let menu = st.borrow().columns_menu.clone();
                menu.popup();
            }
            return;
        }
        let Some(obj) = object_at(&cv2, x, y) else { return };
        g.set_state(gtk::EventSequenceState::Claimed);
        if let (Some(pos), Some(st)) = (position_of(&obj), state()) {
            let sel = st.borrow().selection.clone();
            if !sel.is_selected(pos) {
                sel.select_item(pos, true);
            }
            refresh_actions(&st.borrow());
        }
        pop.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        pop.popup();
    });
    cv.add_controller(click);
}

/// Switch to All as a tree with this process selected.
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
    set_tree(true);
    set_view(View::All);
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
    let id = col_id(col).to_string();
    let p = prefs::get();
    if p.sort_column != id || p.sort_desc != desc {
        prefs::update(|p| {
            p.sort_column = id;
            p.sort_desc = desc;
        });
    }
}

/// The columns the user chose; a narrow window keeps only Name, CPU and Memory.
fn apply_columns(s: &State) {
    let hidden = prefs::get().hidden_columns;
    let changes: Vec<(gtk::ColumnViewColumn, bool)> = s
        .columns
        .iter()
        .filter_map(|(col, c)| {
            let wanted = *col == Col::Name || !hidden.iter().any(|h| h == col_id(*col));
            let shown = wanted && (!s.narrow || NARROW_COLUMNS.contains(col));
            (c.is_visible() != shown).then(|| (c.clone(), shown))
        })
        .collect();
    let cv = s.view_widget.clone();
    // Showing a column binds its cells at once, and binding borrows the state the
    // caller holds: change them once it's let go.
    glib::idle_add_local_once(move || {
        for (c, shown) in changes {
            c.set_visible(shown);
        }
        align_titles(&cv);
    });
}

/// Number columns' titles sit over their numbers, at the right.
fn align_titles(cv: &gtk::ColumnView) {
    fn walk(w: &gtk::Widget) {
        let mut c = w.first_child();
        while let Some(x) = c {
            if let Some(l) = x.downcast_ref::<gtk::Label>()
                && let Some(parent) = l.parent()
            {
                let number = l.text() != "Name";
                l.set_hexpand(number);
                l.set_xalign(if number { 1.0 } else { 0.0 });
                parent.set_hexpand(true);
                parent.set_halign(gtk::Align::Fill);
            }
            walk(&x);
            c = x.next_sibling();
        }
    }
    if let Some(header) = cv.first_child() {
        walk(&header);
    }
}

// ---------- Build ----------

const INSPECTOR_WIDTH: i32 = 320;

/// The inspector shows beside the table, or the foot bar under it in a narrow window.
fn follow_width() {
    let Some(st) = state() else { return };
    let changed = {
        let mut s = st.borrow_mut();
        let narrow = s.body.has_css_class("narrow");
        let changed = narrow != s.narrow;
        s.narrow = narrow;
        changed
    };
    if !changed {
        return;
    }
    let s = st.borrow();
    if let Some(d) = &s.inspector {
        d.root.set_visible(!s.narrow);
    }
    apply_columns(&s);
    drop(s);
    if let Some(snap) = live::latest() {
        refresh_details(&snap);
    }
}

pub fn build(page: &Page) {
    if prefs::get().processes_layout < prefs::PROCESSES_LAYOUT {
        prefs::update(|p| {
            p.migrate_processes();
        });
    }
    let p = prefs::get();
    let view = View::from_id(&p.process_view);

    // Toolbar: views, filter, how many.
    let toolbar = widgets::hbox(8);
    toolbar.add_css_class("proc-toolbar");
    let switch = widgets::segmented(&widgets::opts(&[("apps", "Apps"), ("all", "All"), ("mine", "Mine")]), view.id(), |id| {
        set_view(View::from_id(&id))
    });
    let mut segments = Vec::new();
    let mut child = switch.first_child();
    for (v, tip) in [
        (View::Apps, "Each app and service, with the processes it runs"),
        (View::All, "Every process"),
        (View::Mine, "Only your own processes"),
    ] {
        let Some(b) = child.clone().and_downcast::<gtk::ToggleButton>() else { break };
        b.set_tooltip_text(Some(tip));
        segments.push((v, b.clone()));
        child = b.next_sibling();
    }
    toolbar.append(&switch);
    let search = gtk::SearchEntry::new();
    search.add_css_class("proc-filter");
    search.set_placeholder_text(Some("Filter by name, command or PID"));
    // About 300 px when there's room; it gives way first so a half-screen window fits.
    search.set_max_width_chars(34);
    // A funnel, not a magnifier: this narrows the list, Go to finds things.
    if let Some(icon) = search.first_child().and_downcast::<gtk::Image>() {
        icon.set_icon_name(Some("activity-filter-symbolic"));
    }
    toolbar.append(&search);
    let spacer = widgets::hbox(0);
    spacer.set_hexpand(true);
    toolbar.append(&spacer);
    let totals = widgets::label("", "proc-totals");
    totals.add_css_class("tnum");
    totals.set_ellipsize(gtk::pango::EllipsizeMode::End);
    toolbar.append(&totals);
    let columns_menu = gtk::MenuButton::new();
    columns_menu.set_icon_name("view-list-symbolic");
    columns_menu.set_has_frame(false);
    columns_menu.set_tooltip_text(Some("Columns and the process tree (or right-click the column titles)"));
    columns_menu.set_valign(gtk::Align::Center);
    toolbar.append(&columns_menu);
    page.body.append(&toolbar);

    // Model: store -> filter -> sort (by section in Apps) -> selection.
    let store = gio::ListStore::new::<ProcObject>();
    let filter = gtk::CustomFilter::new(|o| o.downcast_ref::<ProcObject>().is_some_and(|o| o.row().visible));
    let filtered = gtk::FilterListModel::new(Some(store.clone()), Some(filter.clone()));
    let by = |f: fn(&Row, &Row) -> Ordering| {
        gtk::CustomSorter::new(move |a, b| {
            let (Some(a), Some(b)) = (a.downcast_ref::<ProcObject>(), b.downcast_ref::<ProcObject>()) else {
                return gtk::Ordering::Equal;
            };
            f(&a.row(), &b.row()).into()
        })
    };
    let sorter = by(|a, b| a.order.cmp(&b.order));
    let section_sorter = by(|a, b| a.section.cmp(&b.section));
    let sorted = gtk::SortListModel::new(Some(filtered), Some(sorter.clone()));
    let selection = gtk::MultiSelection::new(Some(sorted.clone()));
    let cv = gtk::ColumnView::new(Some(selection.clone()));
    cv.add_css_class("proc-table");
    cv.set_show_column_separators(false);
    cv.set_reorderable(true);
    cv.set_hexpand(true);
    cv.set_vexpand(true);

    // Columns in the saved order (any new ones at the end), at their saved widths.
    let mut ordered: Vec<&(Col, &str, &str, i32)> = COLUMNS.iter().collect();
    ordered.sort_by_key(|(_, id, ..)| p.column_order.iter().position(|o| o == id).unwrap_or(usize::MAX));
    let mut columns = Vec::new();
    let colbox = widgets::vbox(2);
    colbox.add_css_class("columns-menu");
    colbox.append(&widgets::label("Columns", "columns-menu-title"));
    for (col, id, title, width) in COLUMNS {
        let c = make_column(*col, title, p.column_widths.get(*id).copied().filter(|_| *width > 0).unwrap_or(*width));
        c.set_id(Some(id));
        c.connect_fixed_width_notify(|_| save_columns_soon());
        if *col != Col::Name {
            let check = gtk::CheckButton::with_label(title);
            check.set_active(!p.hidden_columns.iter().any(|h| h == id));
            let id = id.to_string();
            check.connect_toggled(move |b| {
                let on = b.is_active();
                let id = id.clone();
                prefs::update(move |p| {
                    p.hidden_columns.retain(|h| *h != id);
                    if !on {
                        p.hidden_columns.push(id);
                    }
                });
                if let Some(st) = state() {
                    apply_columns(&st.borrow());
                }
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
    cv.columns().connect_items_changed(|cols, _, _, _| {
        save_columns_soon();
        if let Some(st) = state() {
            let cv = st.borrow().view_widget.clone();
            glib::idle_add_local_once(move || align_titles(&cv));
        }
        let _ = cols;
    });
    colbox.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let tree_check = gtk::CheckButton::with_label("Indent by parent");
    tree_check.set_tooltip_text(Some("All and Mine: each process under the one that started it, as a tree"));
    tree_check.set_active(p.process_tree);
    tree_check.connect_toggled(|b| set_tree(b.is_active()));
    colbox.append(&tree_check);
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
    let empty = widgets::label("", "empty-state");
    empty.set_valign(gtk::Align::Start);
    empty.set_wrap(true);
    empty.set_margin_top(24);
    empty.set_margin_start(24);
    let table = gtk::Stack::new();
    table.set_hexpand(true);
    table.set_vexpand(true);
    table.add_named(&scroll, Some("table"));
    table.add_named(&empty, Some("empty"));

    // The table with the inspector beside it; the foot bar under both when narrow.
    let cols = widgets::hbox(0);
    cols.set_vexpand(true);
    cols.append(&table);
    let insp = inspector();
    cols.append(&insp.root);
    page.body.append(&cols);
    let foot = foot();
    page.body.append(&foot.root);
    let group = proc_actions();
    page.body.insert_action_group("proc", Some(&group));

    let my_uid = unsafe { libc::getuid() };
    let header_factory = header_factory();
    let st = State {
        store,
        objects: HashMap::new(),
        filter,
        sorter,
        sorted,
        section_sorter,
        header_factory,
        selection: selection.clone(),
        view_widget: cv.clone(),
        scroll: scroll.clone(),
        columns,
        columns_menu,
        view,
        query: String::new(),
        tree: p.process_tree,
        tree_check,
        closed: HashSet::new(),
        group_ids: HashMap::new(),
        next_group_id: -1,
        opened_groups: HashSet::new(),
        bound: HashMap::new(),
        headers: HashMap::new(),
        counts: [0; 3],
        identities: HashMap::new(),
        search: search.clone(),
        segments,
        totals,
        pending_reveal: None,
        inspector: Some(insp),
        foot: Some(foot),
        actions: group,
        my_uid,
        table,
        empty,
        body: page.body.clone(),
        narrow: false,
        light: theme::palette().light,
    };
    set_sections(&st);
    apply_columns(&st);
    STATE.with(|s| *s.borrow_mut() = Some(Rc::new(RefCell::new(st))));
    page.body.connect_css_classes_notify(|_| follow_width());

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
        end_or_ask(sig, "Press again");
        glib::Propagation::Stop
    });
    // Ahead of the list's own arrow handling.
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    cv.add_controller(keys);
    context_menu(&cv);

    live::on_tick(&cv, update);
}

/// Left/Right in Apps and the tree: close or open the selected branch; Left on a
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
    fn a_groups_memory_is_its_cgroups() {
        let mut rows = sample();
        if let Some(g) = rows.get_mut(&-1).and_then(|r| r.group.as_mut()) {
            g.memory = Some(1500);
        }
        add_group_totals(&mut rows);
        assert_eq!(rows[&-1].agg.map(|a| a.mem), Some(1500));
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
        assert!(out[&20].0);
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
    fn sections_come_before_the_sort() {
        let mut rows = sample();
        // foot is a background program: under Background, below Firefox's Apps even when busier.
        if let Some(r) = rows.get_mut(&20) {
            r.section = Sec::Background;
            r.p.cpu = 90.0;
        }
        let members = add_group_totals(&mut rows);
        let out = place_grouped(&rows, members, &HashSet::new(), "", false, by_cpu(&rows));
        assert!(out[&-1].1 < out[&20].1);
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
    fn the_tree_nests_and_closes() {
        // 1 ← 10 ← 100, 1 ← 20.
        let rows: HashMap<i32, Row> = [(1, 0), (10, 1), (100, 10), (20, 1)]
            .into_iter()
            .map(|(pid, ppid)| {
                let mut r = proc_row(pid, &format!("p{pid}"), pid as f64, false);
                r.p.ppid = ppid;
                (pid, r)
            })
            .collect();
        let all = |_: &Row| true;
        let out = place_tree(&rows, &all, &HashSet::new(), "", by_cpu(&rows));
        assert_eq!((out[&1].2, out[&10].2, out[&100].2), (0, 1, 2));
        // Busier first under each parent: 20 before 10.
        assert!(out[&20].1 < out[&10].1);
        let out = place_tree(&rows, &all, &HashSet::from([10]), "", by_cpu(&rows));
        assert!(!out[&100].0);
        assert_eq!(out[&10].5.map(|a| a.count), Some(2));
    }

    #[test]
    fn rows_land_under_the_right_heading() {
        let windows = HashSet::from([42]);
        let me = 1000;
        let p = |pid, uid, kernel| Proc { pid, uid, kernel, ..Default::default() };
        assert_eq!(proc_section(&p(42, me, false), &windows, me), Sec::Apps);
        assert_eq!(proc_section(&p(43, me, false), &windows, me), Sec::Background);
        assert_eq!(proc_section(&p(44, 0, false), &windows, me), Sec::System);
        assert_eq!(proc_section(&p(45, 0, true), &windows, me), Sec::System);
        let g = Group { kind: Kind::App, leader: 40, pids: vec![40, 42], ..Default::default() };
        assert_eq!(group_section(&g, &windows, Some(me), me), Sec::Apps);
        // The session's own group holds windows launched straight into it; it isn't an app.
        let session = Group { kind: Kind::Other, ..g.clone() };
        assert_eq!(group_section(&session, &windows, Some(me), me), Sec::Background);
        assert_eq!(group_section(&Group { pids: vec![40], ..g }, &windows, Some(0), me), Sec::System);
    }

    #[test]
    fn says_when() {
        let now = glib::DateTime::from_local(2026, 11, 13, 22, 30, 0.0).unwrap();
        let at = |d, h| glib::DateTime::from_local(2026, 11, d, h, 5, 0.0).unwrap();
        assert_eq!(when(&at(13, 14), &now), "Today 14:05");
        assert_eq!(when(&at(12, 9), &now), "Yesterday 09:05");
        assert_eq!(when(&at(2, 18), &now), "Nov 2 18:05");
        assert_eq!(disk_text(0.0, 12.0 * 1024.0 * 1024.0), "12.0 MiB/s write");
        assert_eq!(disk_text(0.0, 0.0), "–");
    }
}
