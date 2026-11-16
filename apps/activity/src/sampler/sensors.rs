//! Temperatures and fans from hwmon, and batteries/AC from power_supply.

use super::read;
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct Reading {
    pub chip: String,
    pub label: String,
    pub value: f64,
    /// For temperatures: the critical/max threshold, when the chip reports one.
    pub high: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct Battery {
    pub name: String,
    pub percent: f64,
    pub status: String,
    /// Positive while discharging or charging, in watts.
    pub watts: f64,
    pub energy_full: f64,
    pub energy_design: f64,
    /// Hours until empty (discharging) or full (charging).
    pub hours_left: Option<f64>,
}

/// A USB-C port that can charge the machine.
#[derive(Debug, Clone, Default)]
pub struct PowerSource {
    pub name: String,
    pub online: bool,
    /// The negotiated mode ("PD", "PD_PPS", "C").
    pub mode: String,
    pub watts: f64,
}

impl Battery {
    pub fn health(&self) -> Option<f64> {
        (self.energy_design > 0.0).then(|| self.energy_full / self.energy_design * 100.0)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Sensors {
    pub temps: Vec<Reading>,
    pub fans: Vec<Reading>,
    pub cpu_temp: Option<f64>,
    pub batteries: Vec<Battery>,
    pub ac: Option<bool>,
    pub sources: Vec<PowerSource>,
}

/// "[C] PD PD_PPS" -> "C" (the bracketed one is in use).
pub fn usb_mode(text: &str) -> String {
    text.split_whitespace().find_map(|t| t.strip_prefix('[')?.strip_suffix(']')).unwrap_or("").to_string()
}

fn num(p: &Path) -> Option<f64> {
    read(&p.to_string_lossy()).trim().parse().ok()
}

fn chip_label(chip: &str) -> String {
    match chip {
        "coretemp" | "k10temp" | "zenpower" => "CPU".into(),
        "nvme" => "NVMe".into(),
        "acpitz" => "ACPI zone".into(),
        "iwlwifi_1" | "iwlwifi" => "Wi-Fi".into(),
        "amdgpu" => "AMD GPU".into(),
        "asus" => "ASUS".into(),
        "acpi_fan" => "ACPI".into(),
        other => other.to_string(),
    }
}

pub fn sample() -> Sensors {
    let mut s = Sensors::default();
    if let Ok(dir) = std::fs::read_dir("/sys/class/hwmon") {
        let mut hw: Vec<_> = dir.flatten().map(|e| e.path()).collect();
        hw.sort();
        for h in hw {
            let chip = read(&h.join("name").to_string_lossy()).trim().to_string();
            // Several drives all call themselves "nvme"; tell them apart by device.
            let device = std::fs::read_link(h.join("device"))
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_default();
            let Ok(files) = std::fs::read_dir(&h) else { continue };
            let mut names: Vec<String> = files.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
            names.sort_by_key(|n| (n.len(), n.clone()));
            for f in names {
                let Some(prefix) = f.strip_suffix("_input") else { continue };
                let Some(v) = num(&h.join(&f)) else { continue };
                let label = read(&h.join(format!("{prefix}_label")).to_string_lossy()).trim().to_string();
                if prefix.starts_with("temp") {
                    let value = v / 1000.0;
                    if value <= 0.0 {
                        continue;
                    }
                    let high = num(&h.join(format!("{prefix}_crit")))
                        .or_else(|| num(&h.join(format!("{prefix}_max"))))
                        .map(|v| v / 1000.0)
                        .filter(|v| *v > 0.0 && *v < 200.0);
                    if (chip == "coretemp" && label.starts_with("Package"))
                        || (chip == "k10temp" && label == "Tctl")
                        || (s.cpu_temp.is_none() && matches!(chip.as_str(), "coretemp" | "k10temp" | "zenpower"))
                    {
                        s.cpu_temp = Some(value);
                    }
                    s.temps.push(Reading {
                        chip: if chip == "nvme" && device.starts_with("nvme") {
                            format!("NVMe {}", &device[4..])
                        } else {
                            chip_label(&chip)
                        },
                        label: if label.is_empty() { prefix.to_string() } else { label },
                        value,
                        high,
                    });
                } else if prefix.starts_with("fan") {
                    s.fans.push(Reading {
                        chip: chip_label(&chip),
                        label: if label.is_empty() { prefix.to_string() } else { label },
                        value: v,
                        high: None,
                    });
                }
            }
        }
    }
    if let Ok(dir) = std::fs::read_dir("/sys/class/power_supply") {
        let mut supplies: Vec<_> = dir.flatten().map(|e| e.path()).collect();
        supplies.sort();
        for p in supplies {
            let kind = read(&p.join("type").to_string_lossy()).trim().to_string();
            if kind == "Mains" {
                s.ac = Some(num(&p.join("online")).unwrap_or(0.0) > 0.0);
            } else if kind == "Battery" && read(&p.join("scope").to_string_lossy()).trim() != "Device" {
                s.batteries.push(battery(&p));
            } else if kind == "USB" {
                let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                let port = name.rsplit(':').next().and_then(|n| n.parse::<u32>().ok());
                let volts = num(&p.join("voltage_now")).unwrap_or(0.0) / 1e6;
                let amps = num(&p.join("current_now")).unwrap_or(0.0) / 1e6;
                s.sources.push(PowerSource {
                    name: port.map_or_else(|| "USB-C".to_string(), |n| format!("USB-C port {n}")),
                    online: num(&p.join("online")).unwrap_or(0.0) > 0.0,
                    mode: usb_mode(&read(&p.join("usb_type").to_string_lossy())),
                    watts: (volts * amps).abs(),
                });
            }
        }
    }
    s
}

fn battery(p: &Path) -> Battery {
    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let status = read(&p.join("status").to_string_lossy()).trim().to_string();
    let volts = num(&p.join("voltage_now")).unwrap_or(0.0) / 1e6;
    // Energy in Wh: prefer energy_*, otherwise charge_* (µAh) × voltage.
    let energy =
        |e: &str, c: &str| num(&p.join(e)).map(|v| v / 1e6).or_else(|| num(&p.join(c)).map(|v| v / 1e6 * volts)).unwrap_or(0.0);
    let energy_now = energy("energy_now", "charge_now");
    let energy_full = energy("energy_full", "charge_full");
    let energy_design = energy("energy_full_design", "charge_full_design");
    let watts = num(&p.join("power_now"))
        .map(|w| w / 1e6)
        .or_else(|| num(&p.join("current_now")).map(|c| c / 1e6 * volts))
        .unwrap_or(0.0)
        .abs();
    let percent =
        num(&p.join("capacity")).unwrap_or_else(|| if energy_full > 0.0 { energy_now / energy_full * 100.0 } else { 0.0 });
    let hours_left = if watts > 0.1 {
        match status.as_str() {
            "Discharging" => Some(energy_now / watts),
            "Charging" => Some((energy_full - energy_now).max(0.0) / watts),
            _ => None,
        }
    } else {
        None
    };
    Battery { name, percent, status, watts, energy_full, energy_design, hours_left }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_usb_mode() {
        assert_eq!(usb_mode("[C] PD PD_PPS\n"), "C");
        assert_eq!(usb_mode("C [PD] PD_PPS"), "PD");
        assert_eq!(usb_mode(""), "");
    }
}
