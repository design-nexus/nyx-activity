use super::top;
use crate::charts::{self, Bar};
use crate::graph::{self, Scale, Series, Tone};
use crate::sampler::Snapshot;
use crate::sampler::net::{Iface, Kind};
use crate::widgets::{self, Page};
use crate::{fmt, live, parts, prefs};
use gtk::pango;
use gtk::prelude::*;

/// The links worth a lane: real ones, and virtual ones that are up.
fn links(s: &Snapshot) -> Vec<&Iface> {
    let mut shown: Vec<&Iface> = s.nets.iter().filter(|n| n.kind != Kind::Virtual || n.up).collect();
    shown.sort_by_key(|n| (n.gateway.is_none(), !n.up, n.kind == Kind::Virtual, n.name.clone()));
    shown
}

/// "Wi-Fi · SkyNet", "Ethernet", "Tailscale", or the link's own name.
fn link_name(n: &Iface) -> String {
    match n.kind {
        Kind::Wifi => match n.ssid.as_ref().or(n.wifi.as_ref().and_then(|w| w.ssid.as_ref())) {
            Some(ssid) => format!("Wi-Fi · {ssid}"),
            None => "Wi-Fi".into(),
        },
        Kind::Ethernet => "Ethernet".into(),
        Kind::Virtual if n.name.starts_with("tailscale") => "Tailscale".into(),
        Kind::Virtual => n.name.clone(),
    }
}

/// A link that's carried nothing in the graphs' range.
fn quiet(name: &str) -> bool {
    let rx = live::history(&format!("net.{name}.rx"));
    let tx = live::history(&format!("net.{name}.tx"));
    rx.iter().chain(&tx).all(|v| *v < 16.0)
}

pub fn build(page: &Page) {
    let holder = widgets::vbox(0);
    page.body.append(&holder);
    let page = Page { root: page.root.clone(), body: holder.clone() };
    live::ready(move |snap| {
        let shown = links(snap);
        let g = page.cards("");
        let head = parts::head("Network", "");
        head.root.append(&parts::range());
        g.add(&head.root);
        live::on_tick(&head.root, move |s| {
            let l = links(s);
            let online = l.iter().filter(|n| n.gateway.is_some()).count();
            head.line.set_text(&format!(
                "{} · {}",
                if l.len() == 1 { "1 link".to_string() } else { format!("{} links", l.len()) },
                match online {
                    0 => "none connected to the internet".to_string(),
                    1 => "1 connected to the internet".to_string(),
                    n => format!("{n} connected to the internet"),
                }
            ));
        });
        if shown.is_empty() {
            g.add(&widgets::banner("No network interfaces are up.", false));
            return;
        }
        g.add(&lanes(&shown));
        let conns = connections();
        let main = shown[0];
        let details = link_details(main);
        g.add(&parts::columns(&[conns.upcast_ref(), details.upcast_ref()], 2));
    });
}

// ---------- Lanes ----------

/// Every link as a lane of traffic on one clock; a quiet one is a thin line.
fn lanes(shown: &[&Iface]) -> gtk::Box {
    let card = widgets::vbox(0);
    card.add_css_class("graph-card");
    card.add_css_class("net-lanes");
    let grid = gtk::Grid::new();
    grid.set_column_spacing(18);
    grid.set_row_spacing(10);
    let mut rows = Vec::new();
    for (i, n) in shown.iter().enumerate() {
        let name = n.name.clone();
        let label = widgets::vbox(1);
        label.set_valign(gtk::Align::Center);
        let title = widgets::label(&link_name(n), "net-lane-name");
        title.set_xalign(0.0);
        title.set_ellipsize(pango::EllipsizeMode::End);
        title.set_width_chars(14);
        title.set_max_width_chars(20);
        title.set_tooltip_text(Some(&n.name));
        let sub = widgets::label("", "net-lane-sub");
        sub.add_css_class("tnum");
        sub.set_xalign(0.0);
        label.append(&title);
        label.append(&sub);
        let main = i == 0 && n.gateway.is_some();
        let spark = graph::sparkline(
            vec![
                Series::new(format!("net.{name}.rx"), "Down", Tone::Swatch(if main { "pink" } else { "blue" })),
                Series {
                    dashed: true,
                    ..Series::new(format!("net.{name}.tx"), "Up", Tone::Swatch(if main { "pink" } else { "blue" }))
                },
            ],
            Scale::Auto { floor: 1024.0 },
            if main { 70 } else { 30 },
            fmt::net_rate,
        );
        let flat = widgets::hbox(0);
        flat.add_css_class("net-flat");
        flat.set_valign(gtk::Align::Center);
        flat.set_hexpand(true);
        let stack = gtk::Stack::new();
        stack.set_hexpand(true);
        stack.set_vhomogeneous(false);
        stack.add_named(&spark, Some("graph"));
        stack.add_named(&flat, Some("flat"));
        grid.attach(&label, 0, i as i32, 1, 1);
        grid.attach(&stack, 1, i as i32, 1, 1);
        rows.push((name, sub, stack));
    }
    let axis = widgets::hbox(0);
    let ago = widgets::label("", "graph-axis");
    ago.set_hexpand(true);
    ago.set_xalign(0.0);
    axis.append(&ago);
    axis.append(&widgets::label("now", "graph-axis"));
    grid.attach(&axis, 1, rows.len() as i32, 1, 1);
    card.append(&grid);
    live::on_tick(&card, move |s| {
        ago.set_text(&format!("{} ago", fmt::duration(prefs::get().history_secs as f64)));
        for (name, sub, stack) in &rows {
            let Some(n) = s.nets.iter().find(|x| &x.name == name) else {
                sub.set_text("Gone");
                stack.set_visible_child_name("flat");
                continue;
            };
            let q = quiet(name);
            stack.set_visible_child_name(if q { "flat" } else { "graph" });
            sub.set_text(&if !n.up {
                "Not connected".to_string()
            } else if q {
                "Quiet".to_string()
            } else {
                format!("{} ↓ · {} ↑", fmt::net_rate(n.rx_bps), fmt::net_rate(n.tx_bps))
            });
        }
    });
    card
}

// ---------- Connections ----------

/// Apps with the most open connections.
fn conn_bars(s: &Snapshot) -> Vec<Bar> {
    let Some(c) = &s.conns else { return Vec::new() };
    let mut by_group: Vec<((String, String), i32, usize)> = Vec::new();
    for (pid, n) in &c.by_pid {
        let group = s.groups.iter().find(|g| g.pids.contains(pid));
        let (identity, leader) = match group {
            Some(g) => (top::group_identity(g, s), g.leader),
            None => {
                let name = s.procs.as_ref().and_then(|p| p.iter().find(|p| p.pid == *pid)).map(|p| p.name.clone());
                (top::app_identity(&name.unwrap_or_else(|| pid.to_string()), &[*pid], s), *pid)
            }
        };
        match by_group.iter_mut().find(|(_, l, _)| *l == leader) {
            Some(e) => e.2 += n,
            None => by_group.push((identity, leader, *n)),
        }
    }
    by_group.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.0.cmp(&b.0.0)));
    by_group.truncate(5);
    let max = by_group.first().map_or(1, |g| g.2).max(1) as f64;
    by_group
        .into_iter()
        .map(|((name, icon), leader, n)| Bar {
            name,
            icon,
            frac: n as f64 / max,
            value: format!("{n} open"),
            tip: String::new(),
            pid: leader,
        })
        .collect()
}

fn connections() -> gtk::Box {
    let c = parts::card("network-transmit-receive-symbolic", "Connections");
    let open = charts::tile("Open");
    let listen = charts::tile("Listening");
    let closing = charts::tile("Waiting to close");
    c.root.append(&parts::readings(&[&open, &listen, &closing]));
    let apps = parts::app_list(5, "No open connections.", false);
    c.root.append(&apps.root);
    c.root.set_tooltip_text(Some("Open TCP connections each app holds (only your own processes can be seen)"));
    live::on_tick(&c.root, move |s| {
        let Some(k) = &s.conns else {
            for t in [&open, &listen, &closing] {
                t.value.set_text("–");
                t.sub.set_text("not available");
            }
            apps.set(&[]);
            return;
        };
        open.value.set_text(&k.established.to_string());
        open.sub.set_text(&format!("TCP · {} UDP sockets", k.udp));
        let public = k.listeners.iter().filter(|l| l.public()).count();
        listen.value.set_text(&k.listeners.len().to_string());
        listen.sub.set_text(&format!("{public} reachable from the network"));
        closing.value.set_text(&(k.close_wait + k.time_wait).to_string());
        closing.sub.set_text("");
        apps.set(&conn_bars(s).into_iter().map(|b| parts::App::from_bar(b, String::new())).collect::<Vec<_>>());
    });
    c.root
}

// ---------- The link ----------

fn kv_rows(card: &gtk::Box, keys: &[&str]) -> Vec<gtk::Label> {
    let list = widgets::vbox(0);
    list.add_css_class("kv-list");
    let mut out = Vec::new();
    for k in keys {
        let row = widgets::hbox(12);
        row.add_css_class("kv-row");
        let key = widgets::label(k, "kv-key");
        key.set_xalign(0.0);
        let v = widgets::label("–", "kv-value");
        v.add_css_class("tnum");
        v.set_hexpand(true);
        v.set_xalign(1.0);
        v.set_ellipsize(pango::EllipsizeMode::Start);
        row.append(&key);
        row.append(&v);
        list.append(&row);
        out.push(v);
    }
    card.append(&list);
    out
}

/// The link that reaches the internet, in words.
fn link_details(n: &Iface) -> gtk::Box {
    let wifi = n.kind == Kind::Wifi;
    let title = if wifi { "Wi-Fi".to_string() } else { link_name(n) };
    let c = parts::card(if wifi { "network-wireless-symbolic" } else { "network-wired-symbolic" }, &title);
    let keys: &[&str] = if wifi {
        &["Signal", "Band", "Standard", "Link speed", "Address"]
    } else {
        &["State", "Link speed", "Address", "Gateway", "DNS"]
    };
    let v = kv_rows(&c.root, keys);
    let name = n.name.clone();
    live::on_tick(&c.root, move |s| {
        let Some(n) = s.nets.iter().find(|x| x.name == name) else { return };
        let dash = || "–".to_string();
        c.meta.set_text(&n.name);
        let addr = n.addrs.first().map(|a| a.split('/').next().unwrap_or(a).to_string()).unwrap_or_else(dash);
        if wifi {
            let w = n.wifi.clone().unwrap_or_default();
            v[0].set_text(
                &n.signal
                    .map(|d| format!("{} · {d:.0} dBm", quality(d)))
                    .unwrap_or_else(|| if n.up { dash() } else { "Not connected".into() }),
            );
            let mut band = Vec::new();
            if let Some(b) = w.band() {
                band.push(b.to_string());
            }
            if let Some(ch) = w.channel() {
                band.push(format!("channel {ch}"));
            }
            if let Some(wd) = &w.width {
                band.push(wd.clone());
            }
            v[1].set_text(&if band.is_empty() { dash() } else { band.join(" · ") });
            v[2].set_text(w.standard.unwrap_or("–"));
            v[3].set_text(&match (w.rx_rate, w.tx_rate) {
                (Some(r), Some(t)) => format!("{r:.0} Mb/s down · {t:.0} up"),
                (Some(r), None) => format!("{r:.0} Mb/s"),
                _ => dash(),
            });
            v[4].set_text(&addr);
        } else {
            v[0].set_text(if n.up { "Connected" } else { "Not connected" });
            v[1].set_text(
                &n.speed_mbps
                    .map(|m| if m >= 1000 { format!("{} Gb/s", m / 1000) } else { format!("{m} Mb/s") })
                    .unwrap_or_else(dash),
            );
            v[2].set_text(&addr);
            v[3].set_text(n.gateway.as_deref().unwrap_or("–"));
            v[4].set_text(&if n.dns.is_empty() { dash() } else { n.dns.join(", ") });
        }
        v[if wifi { 4 } else { 2 }].set_tooltip_text(Some(&n.addrs.join("\n")));
    });
    c.root
}

fn quality(dbm: f64) -> &'static str {
    match dbm {
        d if d >= -55.0 => "Excellent",
        d if d >= -67.0 => "Good",
        d if d >= -75.0 => "Fair",
        _ => "Weak",
    }
}
