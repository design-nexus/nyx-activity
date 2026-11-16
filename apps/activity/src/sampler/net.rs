//! Network interfaces: rates from `/proc/net/dev`, addresses and Wi-Fi details.

use super::read;
use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Counters {
    pub rx: u64,
    pub tx: u64,
}

pub fn parse_net_dev(text: &str) -> HashMap<String, Counters> {
    text.lines()
        .skip(2)
        .filter_map(|l| {
            let (name, rest) = l.split_once(':')?;
            let f: Vec<u64> = rest.split_whitespace().filter_map(|v| v.parse().ok()).collect();
            if f.len() < 9 {
                return None;
            }
            let n = |i: usize| f.get(i).copied().unwrap_or(0);
            Some((name.trim().to_string(), Counters { rx: n(0), tx: n(8) }))
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    Wifi,
    #[default]
    Ethernet,
    Virtual,
}

#[derive(Debug, Clone, Default)]
pub struct Iface {
    pub name: String,
    pub kind: Kind,
    pub up: bool,
    pub rx_bps: f64,
    pub tx_bps: f64,
    pub rx_total: u64,
    pub addrs: Vec<String>,
    pub ssid: Option<String>,
    /// Signal level in dBm.
    pub signal: Option<f64>,
    pub speed_mbps: Option<u64>,
    pub wifi: Option<WifiLink>,
    /// The default route's gateway, when it goes through here.
    pub gateway: Option<String>,
    pub dns: Vec<String>,
}

/// What `iw dev X link` says about the connection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WifiLink {
    pub ssid: Option<String>,
    pub signal: Option<f64>,
    /// MHz.
    pub freq: Option<f64>,
    /// Mbit/s.
    pub rx_rate: Option<f64>,
    pub tx_rate: Option<f64>,
    /// "160MHz EHT-MCS 4" → "160 MHz", "Wi-Fi 7".
    pub width: Option<String>,
    pub standard: Option<&'static str>,
    pub bssid: Option<String>,
}

impl WifiLink {
    pub fn band(&self) -> Option<&'static str> {
        let f = self.freq?;
        Some(if f >= 5925.0 {
            "6 GHz"
        } else if f >= 4900.0 {
            "5 GHz"
        } else {
            "2.4 GHz"
        })
    }

    pub fn channel(&self) -> Option<u32> {
        let f = self.freq? as u32;
        Some(match f {
            2484 => 14,
            2412..=2472 => (f - 2407) / 5,
            5955..=7115 => (f - 5950) / 5,
            _ => f.saturating_sub(5000) / 5,
        })
    }
}

fn kind(name: &str) -> Kind {
    let base = format!("/sys/class/net/{name}");
    if Path::new(&format!("{base}/wireless")).exists() {
        Kind::Wifi
    } else if Path::new(&format!("{base}/device")).exists() {
        Kind::Ethernet
    } else {
        Kind::Virtual
    }
}

/// Addresses by interface, from `ip -j addr`.
pub fn parse_ip_json(text: &str) -> HashMap<String, Vec<String>> {
    let mut out = HashMap::new();
    let Ok(serde_json::Value::Array(list)) = serde_json::from_str::<serde_json::Value>(text) else { return out };
    for iface in list {
        let Some(name) = iface["ifname"].as_str() else { continue };
        let addrs: Vec<String> = iface["addr_info"]
            .as_array()
            .map(|a| a.iter().filter_map(|i| Some(format!("{}/{}", i["local"].as_str()?, i["prefixlen"].as_u64()?))).collect())
            .unwrap_or_default();
        out.insert(name.to_string(), addrs);
    }
    out
}

/// Signal level by interface from `/proc/net/wireless`.
pub fn parse_wireless(text: &str) -> HashMap<String, f64> {
    text.lines()
        .skip(2)
        .filter_map(|l| {
            let (name, rest) = l.split_once(':')?;
            let f: Vec<&str> = rest.split_whitespace().collect();
            let level: f64 = f.get(2)?.trim_end_matches('.').parse().ok()?;
            Some((name.trim().to_string(), level))
        })
        .collect()
}

/// The link details from `iw dev X link`.
pub fn parse_iw_link(text: &str) -> WifiLink {
    let mut w = WifiLink::default();
    let first = |v: &str| v.split_whitespace().next().and_then(|n| n.parse::<f64>().ok());
    for l in text.lines().map(str::trim) {
        if let Some(v) = l.strip_prefix("Connected to ") {
            w.bssid = v.split_whitespace().next().map(String::from);
        } else if let Some(v) = l.strip_prefix("SSID: ") {
            w.ssid = Some(v.to_string());
        } else if let Some(v) = l.strip_prefix("signal: ") {
            w.signal = first(v);
        } else if let Some(v) = l.strip_prefix("freq: ") {
            w.freq = first(v);
        } else if let Some(v) = l.strip_prefix("rx bitrate: ") {
            w.rx_rate = first(v);
        } else if let Some(v) = l.strip_prefix("tx bitrate: ") {
            w.tx_rate = first(v);
            w.width = v.split_whitespace().find_map(|t| t.strip_suffix("MHz")).map(|m| format!("{m} MHz"));
            w.standard = if v.contains("EHT-") {
                Some("Wi-Fi 7")
            } else if v.contains("HE-") {
                Some("Wi-Fi 6")
            } else if v.contains("VHT-") {
                Some("Wi-Fi 5")
            } else if v.contains("MCS") {
                Some("Wi-Fi 4")
            } else {
                None
            };
        }
    }
    w
}

/// The default route's gateway and interface, from `ip -j route show default`.
pub fn parse_default_route(text: &str) -> Vec<(String, String)> {
    let Ok(serde_json::Value::Array(list)) = serde_json::from_str::<serde_json::Value>(text) else { return Vec::new() };
    list.iter().filter_map(|r| Some((r["dev"].as_str()?.to_string(), r["gateway"].as_str()?.to_string()))).collect()
}

/// DNS servers by interface from `resolvectl dns` ("Link 3 (wlo1): 192.168.1.1"), "" for global.
pub fn parse_resolvectl(text: &str) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for line in text.lines() {
        // IPv6 servers contain colons too, so the head ends at the first one ("Link 3 (wlo1):").
        let Some(i) = line.find(':').filter(|_| line.starts_with("Link") || line.starts_with("Global")) else { continue };
        let head = &line[..i];
        let servers: Vec<String> = line[i + 1..].split_whitespace().map(String::from).collect();
        let name = head.split_once('(').and_then(|(_, r)| r.strip_suffix(')')).unwrap_or("").to_string();
        if !servers.is_empty() {
            out.insert(name, servers);
        }
    }
    out
}

/// Nameservers in `/etc/resolv.conf`.
pub fn parse_resolv_conf(text: &str) -> Vec<String> {
    text.lines().filter_map(|l| l.trim().strip_prefix("nameserver")).map(|v| v.trim().to_string()).collect()
}

fn output(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

pub struct NetSampler {
    prev: HashMap<String, Counters>,
    tick: u64,
    addrs: HashMap<String, Vec<String>>,
    links: HashMap<String, WifiLink>,
    gateways: Vec<(String, String)>,
    dns: HashMap<String, Vec<String>>,
}

impl NetSampler {
    pub fn new() -> Self {
        NetSampler {
            prev: HashMap::new(),
            tick: 0,
            addrs: HashMap::new(),
            links: HashMap::new(),
            gateways: Vec::new(),
            dns: HashMap::new(),
        }
    }

    pub fn sample(&mut self, elapsed: f64) -> Vec<Iface> {
        let stats = parse_net_dev(&read("/proc/net/dev"));
        let slow = self.tick.is_multiple_of(10);
        // Newer Wi-Fi drivers leave /proc/net/wireless empty, so the signal comes from iw.
        let wifi_tick = self.tick.is_multiple_of(2);
        self.tick += 1;
        if slow {
            self.addrs = parse_ip_json(&output("ip", &["-j", "addr"]));
            self.gateways = parse_default_route(&output("ip", &["-j", "route", "show", "default"]));
            self.dns = parse_resolvectl(&output("resolvectl", &["dns"]));
            if self.dns.is_empty() {
                self.dns.insert(String::new(), parse_resolv_conf(&read("/etc/resolv.conf")));
            }
        }
        let signals = parse_wireless(&read("/proc/net/wireless"));
        let mut out: Vec<Iface> = Vec::new();
        for (name, now) in &stats {
            if name == "lo" || name.starts_with("veth") {
                continue;
            }
            let base = format!("/sys/class/net/{name}");
            let k = kind(name);
            let mut i = Iface {
                name: name.clone(),
                kind: k,
                up: read(&format!("{base}/operstate")).trim() == "up",
                rx_total: now.rx,
                addrs: self.addrs.get(name).cloned().unwrap_or_default(),
                signal: signals.get(name).copied(),
                speed_mbps: read(&format!("{base}/speed")).trim().parse::<i64>().ok().filter(|s| *s > 0).map(|s| s as u64),
                gateway: self.gateways.iter().find(|(dev, _)| dev == name).map(|(_, g)| g.clone()),
                dns: self.dns.get(name.as_str()).or_else(|| self.dns.get("")).cloned().unwrap_or_default(),
                ..Default::default()
            };
            if let Some(prev) = self.prev.get(name)
                && elapsed > 0.0
            {
                let rate = |a: u64, b: u64| a.saturating_sub(b) as f64 / elapsed;
                i.rx_bps = rate(now.rx, prev.rx);
                i.tx_bps = rate(now.tx, prev.tx);
            }
            if k == Kind::Wifi && i.up {
                if wifi_tick || !self.links.contains_key(name) {
                    let link = parse_iw_link(&output("iw", &["dev", name, "link"]));
                    if link.ssid.is_some() {
                        self.links.insert(name.clone(), link);
                    } else {
                        self.links.remove(name);
                    }
                }
                if let Some(link) = self.links.get(name) {
                    i.ssid = link.ssid.clone();
                    i.signal = i.signal.or(link.signal);
                    i.wifi = Some(link.clone());
                }
            }
            out.push(i);
        }
        // Physical first, then by name.
        out.sort_by(|a, b| (a.kind == Kind::Virtual, !a.up, &a.name).cmp(&(b.kind == Kind::Virtual, !b.up, &b.name)));
        self.prev = stats;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_net_dev() {
        let text = "Inter-|   Receive\n face |bytes packets\n\
                    wlo1: 1000 10 0 0 0 0 0 0 2000 20 0 0 0 0 0 0\n    lo: 5 1 0 0 0 0 0 0 5 1 0 0 0 0 0 0\n";
        let s = parse_net_dev(text);
        assert_eq!(s["wlo1"], Counters { rx: 1000, tx: 2000 });
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn parses_routes_and_dns() {
        let r = r#"[{"dst":"default","gateway":"192.168.1.1","dev":"wlo1","metric":600}]"#;
        assert_eq!(parse_default_route(r), vec![("wlo1".into(), "192.168.1.1".into())]);
        let d = parse_resolvectl("Global:\nLink 2 (eno1):\nLink 3 (wlo1): 192.168.1.1 fe80::1\n");
        assert_eq!(d["wlo1"], vec!["192.168.1.1".to_string(), "fe80::1".to_string()]);
        assert!(!d.contains_key("eno1"));
        assert_eq!(parse_resolv_conf("# x\nnameserver 127.0.0.53\noptions edns0\n"), vec!["127.0.0.53".to_string()]);
    }

    #[test]
    fn parses_ip_json() {
        let text = r#"[{"ifname":"wlo1","addr_info":[{"family":"inet","local":"192.168.1.5","prefixlen":24}]}]"#;
        assert_eq!(parse_ip_json(text)["wlo1"], vec!["192.168.1.5/24".to_string()]);
    }

    #[test]
    fn parses_iw_link() {
        let text = "Connected to aa:bb (on wlo1)\n\tSSID: SkyNet\n\tfreq: 6135.0\n\tsignal: -52 dBm\n\
                    \trx bitrate: 864.6 MBit/s 160MHz EHT-MCS 4 EHT-NSS 2 EHT-GI 0\n\
                    \ttx bitrate: 432.3 MBit/s 160MHz EHT-MCS 4 EHT-NSS 1 EHT-GI 0\n";
        let w = parse_iw_link(text);
        assert_eq!(w.ssid.as_deref(), Some("SkyNet"));
        assert_eq!(w.signal, Some(-52.0));
        assert_eq!(w.rx_rate, Some(864.6));
        assert_eq!(w.tx_rate, Some(432.3));
        assert_eq!(w.width.as_deref(), Some("160 MHz"));
        assert_eq!(w.standard, Some("Wi-Fi 7"));
        assert_eq!(w.band(), Some("6 GHz"));
        assert_eq!(w.channel(), Some(37));
        assert_eq!(w.bssid.as_deref(), Some("aa:bb"));
    }

    #[test]
    fn parses_wireless() {
        let text = "Inter-| sta-|\n face | tus |\n  wlo1: 0000   60.  -50.  -256        0      0\n";
        assert_eq!(parse_wireless(text)["wlo1"], -50.0);
    }
}
