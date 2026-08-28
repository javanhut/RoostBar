//! Battery (sysfs) and Wi-Fi (CAW's `caw status`).

use std::process::Command;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Battery {
    pub percent: u32,
    pub charging: bool,
    pub full: bool,
}

pub fn battery(name: &str) -> Option<Battery> {
    let base = format!("/sys/class/power_supply/{name}");
    let read = |f: &str| std::fs::read_to_string(format!("{base}/{f}")).ok().map(|s| s.trim().to_string());
    let percent = read("capacity")?.parse().ok()?;
    let status = read("status").unwrap_or_default();
    Some(Battery {
        percent,
        charging: status == "Charging",
        full: status == "Full" || (status == "Not charging" && percent >= 95),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wifi {
    Unavailable,
    Disconnected,
    Connected(String),
}

pub fn wifi(_iface: &str) -> Wifi {
    let out = match Command::new("caw").arg("status").output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return Wifi::Unavailable,
    };
    let mut state = String::new();
    let mut network = String::new();
    for line in out.lines() {
        let mut it = line.split_whitespace();
        match (it.next(), it.next()) {
            (Some("state"), Some(v)) => state = v.to_string(),
            (Some("network"), Some(v)) => network = it.fold(v.to_string(), |a, w| a + " " + w),
            _ => {}
        }
    }
    if state.eq_ignore_ascii_case("connected") && !network.is_empty() {
        Wifi::Connected(network)
    } else {
        Wifi::Disconnected
    }
}
