use super::top;
use crate::charts::{self, Bar, PAIR};
use crate::graph::{self, Scale, Series, Tone};
use crate::sampler::Snapshot;
use crate::sampler::net::{Iface, Kind};
use crate::widgets::{self, Page};
use crate::{fmt, live};
use gtk::pango;
use gtk::prelude::*;

pub fn build(page: &Page) {
    let holder = widgets::vbox(0);
    page.body.append(&holder);
    let page = Page { root: page.root.clone(), body: holder.clone() };
    live::ready(move |snap| {
        let mut shown: Vec<&Iface> = snap.nets.iter().filter(|n| n.kind != Kind::Virtual || n.up).collect();
        shown.sort_by_key(|n| (!n.up, n.kind == Kind::Virtual));
        if shown.is_empty() {
            holder.append(&widgets::banner("No network interfaces are up.", false));
        }
        for n in shown {
            iface_section(&page, n);
        }
        overall(&page);
    });
}

/// Processes with the most open connections, by app.
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
    by_group.truncate(8);
    let max = by_group.first().map_or(1, |g| g.2).max(1) as f64;
    by_group
        .into_iter()
        .map(|((name, icon), leader, n)| Bar {
            name,
            icon,
            frac: n as f64 / max,
            value: if n == 1 { "1 conn.".into() } else { format!("{n} conn.") },
            tip: String::new(),
            pid: leader,
            warn: false,
        })
        .collect()
}

fn overall(page: &Page) {
    let g = page.cards("Connections");
    let down = charts::tile("Download");
    let up = charts::tile("Upload");
    let open = charts::tile("Open connections");
    let listen = charts::tile("Listening");
    g.add(&charts::row(&[down.root.upcast_ref(), up.root.upcast_ref(), open.root.upcast_ref(), listen.root.upcast_ref()], 150));

    let apps = charts::bar_list(8, "No open connections.", true);
    let (apps_card, apps_sum) = charts::card("Apps with connections", &apps.root);
    apps_card.set_tooltip_text(Some("Open TCP connections each app holds (only your own processes can be seen)"));

    let states = charts::stack_bar(&["Open", "Closing", "Waiting to close", "Other"], None);
    let ports = widgets::vbox(0);
    ports.add_css_class("bar-list");
    let ports_box = widgets::vbox(12);
    ports_box.append(&states.root);
    ports_box.append(&widgets::label("LISTENING", "stat-title"));
    ports_box.append(&ports);
    let (ports_card, ports_sum) = charts::card("Sockets", &ports_box);
    g.add(&apps_card);
    let sockets = widgets::vbox(12);
    sockets.append(&ports_card);
    let gr = graph::graph(vec![Series::new("net.conns", "Open", Tone::Accent)], Scale::Auto { floor: 20.0 }, fmt::count, 120);
    let (connection_history, _) = charts::card("Connection history", &gr.root);
    sockets.append(&connection_history);
    g.add(&widgets::disclosure("network.sockets", "Sockets and listening ports", &sockets));

    let port_rows: Vec<(gtk::Box, gtk::Label, gtk::Label, gtk::Label)> = (0..8)
        .map(|_| {
            let r = widgets::hbox(10);
            r.add_css_class("port-row");
            let port = widgets::label("", "bar-name");
            port.add_css_class("tnum");
            port.set_width_chars(10);
            let who = widgets::label("", "bar-name");
            who.set_hexpand(true);
            who.set_ellipsize(pango::EllipsizeMode::End);
            let scope = widgets::label("", "bar-value");
            r.append(&port);
            r.append(&who);
            r.append(&scope);
            ports.append(&r);
            (r, port, who, scope)
        })
        .collect();

    live::on_tick(&down.root, move |s| {
        let rx = s.net_rx();
        let tx = s.net_tx();
        down.value.set_text(&fmt::net_rate(rx));
        up.value.set_text(&fmt::net_rate(tx));
        let phys = s.nets.iter().filter(|n| n.kind != Kind::Virtual);
        let (rt, tt) = phys.fold((0u64, 0u64), |(r, t), n| (r + n.rx_total, t + n.tx_total));
        down.sub.set_text(&format!("{} since connecting", fmt::bytes(rt as f64)));
        up.sub.set_text(&format!("{} since connecting", fmt::bytes(tt as f64)));
        let Some(c) = &s.conns else {
            open.value.set_text("–");
            open.sub.set_text("Not available");
            listen.value.set_text("–");
            listen.sub.set_text("Not available");
            apps.set(&[]);
            apps_sum.set_text("Not available");
            ports_sum.set_text("Not available");
            for (row, ..) in &port_rows {
                row.set_visible(false);
            }
            return;
        };
        open.value.set_text(&c.established.to_string());
        open.sub.set_text(&format!("{} UDP sockets", c.udp));
        let public = c.listeners.iter().filter(|l| l.public()).count();
        listen.value.set_text(&c.listeners.len().to_string());
        listen.sub.set_text(&format!("{public} reachable from the network"));
        widgets::set_warn(&listen.sub, false);

        apps.set(&conn_bars(s));
        apps_sum.set_text(&format!("{} open", c.established));
        let parts = [c.established as f64, c.close_wait as f64, c.time_wait as f64, (c.other + c.listening) as f64];
        let total: f64 = parts.iter().sum();
        states.set(&parts, total.max(1.0), &parts.map(|v| (v as u64).to_string()));
        ports_sum.set_text(&format!("{} TCP · {} UDP", total as u64, c.udp));
        let procs = s.procs.as_deref().unwrap_or(&[]);
        for (i, (row, port, who, scope)) in port_rows.iter().enumerate() {
            let Some(l) = c.listeners.get(i) else {
                row.set_visible(false);
                continue;
            };
            row.set_visible(true);
            port.set_text(&format!("{} {}", if l.udp { "UDP" } else { "TCP" }, l.port));
            let name = procs.iter().find(|p| p.pid == l.pid).map(|p| p.name.clone());
            who.set_text(&name.unwrap_or_else(|| "system".into()));
            scope.set_text(if l.public() { "network" } else { "this computer" });
            row.set_tooltip_text(Some(&format!("Listening on {}:{}", l.addr, l.port)));
        }
    });
}

fn iface_section(page: &Page, n: &Iface) {
    let kind = match n.kind {
        Kind::Wifi => "Wi-Fi",
        Kind::Ethernet => "Ethernet",
        Kind::Virtual => "Virtual",
    };
    let g = page.cards(&format!("{kind} · {}", n.name));
    let name = n.name.clone();
    let wifi = n.kind == Kind::Wifi;

    let gr = graph::graph(
        vec![
            Series::new(format!("net.{name}.rx"), "Download", Tone::Accent),
            Series::new(format!("net.{name}.tx"), "Upload", Tone::Second),
        ],
        Scale::Auto { floor: 64.0 * 1024.0 },
        fmt::net_rate,
        130,
    );
    let (traffic, traffic_sum) = charts::card("Traffic", &gr.root);

    // The link: signal for Wi-Fi, how much of the line is used for wired.
    let items: Vec<(&str, Option<usize>)> = if wifi {
        vec![("Network", None), ("Band", None), ("Channel", None), ("Standard", None), ("Link speed", None)]
    } else {
        vec![("State", None), ("Link speed", None), ("MTU", None), ("Errors", None), ("Dropped", None)]
    };
    let link_body = widgets::vbox(12);
    let signal = widgets::label("–", "stat-value");
    signal.add_css_class("tnum");
    link_body.append(&signal);
    let (legend, lv) = charts::legend(&items);
    link_body.append(&legend);
    let (link, link_sum) = charts::card("Connection", &link_body);
    g.add(&charts::primary(&link, &traffic));
    let diagnostics = widgets::vbox(12);

    let gr = graph::graph(
        vec![
            Series::new(format!("net.{name}.rxp"), "Received", Tone::Accent),
            Series::new(format!("net.{name}.txp"), "Sent", Tone::Second),
        ],
        Scale::Auto { floor: 100.0 },
        fmt::per_sec,
        100,
    );
    let (packets, packets_sum) = charts::card("Packets", &gr.root);
    if wifi {
        let gr =
            graph::graph(vec![Series::new(format!("net.{name}.signal"), "Signal", Tone::Accent)], Scale::Percent, fmt::pct, 100);
        let (sig, _) = charts::card("Signal over time", &gr.root);
        diagnostics.append(&charts::row(&[sig.upcast_ref(), packets.upcast_ref()], PAIR));
    } else {
        diagnostics.append(&packets);
    }

    let spec = widgets::vbox(0);
    spec.add_css_class("graph-card");
    spec.add_css_class("spec-card");
    let (flow, v) =
        widgets::kv_flow(&["Addresses", "Gateway", "DNS", "Hardware address", "Received", "Sent", "Errors", "Dropped"]);
    spec.append(&flow);
    diagnostics.append(&spec);
    g.add(&widgets::disclosure(&format!("network.{name}.details"), "Link diagnostics and configuration", &diagnostics));

    live::on_tick(&traffic, move |s| {
        let Some(n) = s.nets.iter().find(|x| x.name == name) else {
            traffic_sum.set_text("Gone");
            return;
        };
        traffic_sum.set_text(&format!("↓ {}  ↑ {}", fmt::net_rate(n.rx_bps), fmt::net_rate(n.tx_bps)));
        packets_sum.set_text(&format!("{} errors · {} dropped", fmt::count(n.errors as f64), fmt::count(n.drops as f64)));
        let dash = || "–".to_string();
        if wifi {
            let w = n.wifi.clone().unwrap_or_default();
            match n.signal {
                Some(dbm) => {
                    let q = fmt::signal_quality(dbm);
                    signal.set_text(&format!("{dbm:.0} dBm"));
                    widgets::set_warn(&signal, q < 30.0);
                    link_sum.set_text(quality(dbm));
                }
                None => {
                    signal.set_text("–");
                    widgets::set_warn(&signal, false);
                    link_sum.set_text(if n.up { "" } else { "Disconnected" });
                }
            }
            lv[0].set_text(w.ssid.as_deref().unwrap_or(if n.up { "–" } else { "Disconnected" }));
            lv[1].set_text(w.band().unwrap_or("–"));
            lv[2].set_text(&match (w.channel(), &w.width) {
                (Some(c), Some(wd)) => format!("{c} · {wd}"),
                (Some(c), None) => c.to_string(),
                _ => dash(),
            });
            lv[3].set_text(w.standard.unwrap_or("–"));
            lv[4].set_text(&match (w.rx_rate, w.tx_rate) {
                (Some(r), Some(t)) => format!("↓ {r:.0} · ↑ {t:.0} Mb/s"),
                (Some(r), None) => format!("{r:.0} Mb/s"),
                _ => dash(),
            });
        } else {
            let speed = n.speed_mbps.map(|m| m as f64 * 1e6 / 8.0);
            let used = n.rx_bps.max(n.tx_bps);
            match speed {
                Some(sp) if sp > 0.0 => {
                    let f = used / sp;
                    signal.set_text(&format!("{} of link", fmt::pct(f * 100.0)));
                    widgets::set_warn(&signal, f > 0.9);
                }
                _ => {
                    signal.set_text(if n.up { "Up" } else { "Down" });
                    widgets::set_warn(&signal, false);
                }
            }
            link_sum.set_text(if n.up { "Connected" } else { "Down" });
            lv[0].set_text(if n.up { "Connected" } else { "Down" });
            lv[1].set_text(
                &n.speed_mbps
                    .map(|m| if m >= 1000 { format!("{} Gb/s", m / 1000) } else { format!("{m} Mb/s") })
                    .unwrap_or_else(dash),
            );
            lv[2].set_text(&if n.mtu > 0 { n.mtu.to_string() } else { dash() });
            lv[3].set_text(&fmt::count(n.errors as f64));
            lv[4].set_text(&fmt::count(n.drops as f64));
        }
        let addrs = if n.addrs.is_empty() { dash() } else { n.addrs.join("\n") };
        v[0].set_text(n.addrs.first().map(String::as_str).unwrap_or("–"));
        v[0].set_tooltip_text(Some(&addrs));
        v[1].set_text(n.gateway.as_deref().unwrap_or("–"));
        v[2].set_text(&if n.dns.is_empty() { dash() } else { n.dns.join(", ") });
        v[2].set_tooltip_text(Some(&n.dns.join("\n")));
        v[3].set_text(if n.mac.is_empty() { "–" } else { &n.mac });
        v[4].set_text(&fmt::bytes(n.rx_total as f64));
        v[5].set_text(&fmt::bytes(n.tx_total as f64));
        v[6].set_text(&fmt::count(n.errors as f64));
        v[7].set_text(&fmt::count(n.drops as f64));
    });
}

fn quality(dbm: f64) -> &'static str {
    match dbm {
        d if d >= -55.0 => "Excellent",
        d if d >= -67.0 => "Good",
        d if d >= -75.0 => "Fair",
        _ => "Weak",
    }
}
