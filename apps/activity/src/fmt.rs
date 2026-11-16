//! Number formatting for readouts. Everything here is shown in the mono font.

use crate::prefs;

/// Binary sizes: 512 B, 1.4 KiB, 23.5 MiB, 7.81 GiB.
pub fn bytes(v: f64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = v.max(0.0);
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} B", v.round() as u64)
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[i])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[i])
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

pub fn rate(v: f64) -> String {
    format!("{}/s", bytes(v))
}

/// Network rates follow the bits/bytes preference; bits use decimal prefixes.
pub fn net_rate(v: f64) -> String {
    if !prefs::get().net_bits {
        return rate(v);
    }
    const UNITS: [&str; 5] = ["b/s", "kb/s", "Mb/s", "Gb/s", "Tb/s"];
    let mut v = v.max(0.0) * 8.0;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 { format!("{} {}", v.round() as u64, UNITS[i]) } else { format!("{v:.1} {}", UNITS[i]) }
}

pub fn pct(v: f64) -> String {
    if v > 0.0 && v < 1.0 { format!("{v:.1}%") } else { format!("{}%", v.round() as i64) }
}

/// A temperature given in °C, shown in the unit the user picked.
pub fn temp(celsius: f64) -> String {
    temp_in(celsius, prefs::get().fahrenheit)
}

fn temp_in(celsius: f64, fahrenheit: bool) -> String {
    if fahrenheit {
        format!("{} °F", (celsius * 9.0 / 5.0 + 32.0).round() as i64)
    } else {
        format!("{} °C", celsius.round() as i64)
    }
}

pub fn mhz(v: f64) -> String {
    if v >= 1000.0 { format!("{:.2} GHz", v / 1000.0) } else { format!("{} MHz", v.round() as i64) }
}

pub fn watts(v: f64) -> String {
    if v >= 10.0 { format!("{v:.0} W") } else { format!("{v:.1} W") }
}

/// Long durations: 3d 4h, 5h 12m, 7m 3s.
pub fn duration(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    let (d, h, m, sec) = (s / 86400, (s % 86400) / 3600, (s % 3600) / 60, s % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {sec}s")
    } else {
        format!("{sec}s")
    }
}

/// A moment relative to `now` (both Unix seconds): "in 3h 2m", "5m 3s ago".
pub fn relative(when: f64, now: f64) -> String {
    let d = when - now;
    if d.abs() < 1.0 {
        "now".into()
    } else if d > 0.0 {
        format!("in {}", duration(d))
    } else {
        format!("{} ago", duration(-d))
    }
}

/// Wi-Fi signal as 0–100: -100 dBm is nothing, -50 dBm and up is full.
pub fn signal_quality(dbm: f64) -> f64 {
    ((dbm + 100.0) * 2.0).clamp(0.0, 100.0)
}

pub fn ms(v: f64) -> String {
    if v >= 100.0 {
        format!("{v:.0} ms")
    } else if v >= 10.0 {
        format!("{v:.1} ms")
    } else {
        format!("{v:.2} ms")
    }
}

/// Hours as "2h 15m".
pub fn hours(h: f64) -> String {
    duration(h * 3600.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes() {
        assert_eq!(bytes(512.0), "512 B");
        assert_eq!(bytes(1536.0), "1.50 KiB");
        assert_eq!(bytes(8.0 * 1024.0 * 1024.0 * 1024.0), "8.00 GiB");
        assert_eq!(bytes(150.0 * 1024.0 * 1024.0), "150 MiB");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(duration(59.0), "59s");
        assert_eq!(duration(3700.0), "1h 1m");
        assert_eq!(duration(90000.0), "1d 1h");
    }

    #[test]
    fn formats_relative() {
        assert_eq!(relative(1060.0, 1000.0), "in 1m 0s");
        assert_eq!(relative(1000.0, 4700.0), "1h 1m ago");
        assert_eq!(relative(1000.2, 1000.0), "now");
    }

    #[test]
    fn formats_temperatures() {
        assert_eq!(temp_in(42.4, false), "42 °C");
        assert_eq!(temp_in(100.0, true), "212 °F");
        assert_eq!(temp_in(-40.0, true), "-40 °F");
    }

    #[test]
    fn formats_percent() {
        assert_eq!(pct(0.4), "0.4%");
        assert_eq!(pct(42.6), "43%");
    }
}
