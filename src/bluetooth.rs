//! Bluetooth over BlueZ's D-Bus API on the system bus. Everything is polled
//! from the bar's tick and actions run on a throwaway thread so a slow
//! Connect() never freezes the bar.

use std::collections::HashMap;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

type Managed = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BtState {
    /// BlueZ is not on the bus at all (not installed or bluetoothd not running).
    NoStack,
    /// Adapter present but powered off.
    Off,
    /// Powered on, nothing connected.
    Idle,
    Connected(String),
}

pub struct Bluetooth {
    conn: Option<Connection>,
}

struct Snapshot {
    adapter: Option<(OwnedObjectPath, bool)>,
    connected: Vec<(OwnedObjectPath, String)>,
    paired: Vec<(OwnedObjectPath, String, String, bool)>, // path, addr, name, trusted
}

fn get_bool(m: &HashMap<String, OwnedValue>, k: &str) -> bool {
    m.get(k).and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(false)
}
fn get_str(m: &HashMap<String, OwnedValue>, k: &str) -> String {
    m.get(k).and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default()
}

fn snapshot(conn: &Connection) -> Option<Snapshot> {
    let om = Proxy::new(conn, "org.bluez", "/", "org.freedesktop.DBus.ObjectManager").ok()?;
    let objs: Managed = om.call("GetManagedObjects", &()).ok()?;
    let mut s = Snapshot { adapter: None, connected: vec![], paired: vec![] };
    for (path, ifaces) in objs {
        if let Some(a) = ifaces.get("org.bluez.Adapter1") {
            if s.adapter.is_none() {
                s.adapter = Some((path.clone(), get_bool(a, "Powered")));
            }
        }
        if let Some(d) = ifaces.get("org.bluez.Device1") {
            let name = {
                let n = get_str(d, "Alias");
                if n.is_empty() { get_str(d, "Name") } else { n }
            };
            let addr = get_str(d, "Address");
            if get_bool(d, "Connected") {
                s.connected.push((path.clone(), name.clone()));
            }
            if get_bool(d, "Paired") {
                s.paired.push((path, addr, name, get_bool(d, "Trusted")));
            }
        }
    }
    Some(s)
}

impl Bluetooth {
    pub fn new() -> Self {
        Self { conn: Connection::system().ok() }
    }

    pub fn state(&mut self) -> BtState {
        if self.conn.is_none() {
            self.conn = Connection::system().ok();
        }
        let Some(conn) = &self.conn else { return BtState::NoStack };
        let Some(s) = snapshot(conn) else { return BtState::NoStack };
        match s.adapter {
            None => BtState::NoStack,
            Some((_, false)) => BtState::Off,
            Some((_, true)) => match s.connected.first() {
                Some((_, name)) => BtState::Connected(name.clone()),
                None => BtState::Idle,
            },
        }
    }

    /// Left click: connected -> disconnect; off -> power on; idle -> connect
    /// to the preferred device (config MAC, else a trusted paired one, else
    /// any paired one).
    pub fn primary_action(&self, preferred_mac: String) {
        let Some(conn) = self.conn.clone() else { return };
        std::thread::spawn(move || {
            let Some(s) = snapshot(&conn) else { return };
            let Some((adapter, powered)) = s.adapter else { return };
            if !powered {
                if let Ok(p) = Proxy::new(&conn, "org.bluez", adapter, "org.bluez.Adapter1") {
                    let _ = p.set_property("Powered", true);
                }
                return;
            }
            if let Some((path, _)) = s.connected.first() {
                if let Ok(p) = Proxy::new(&conn, "org.bluez", path.clone(), "org.bluez.Device1") {
                    let _ = p.call_method("Disconnect", &());
                }
                return;
            }
            let pick = s
                .paired
                .iter()
                .find(|(_, addr, _, _)| !preferred_mac.is_empty() && addr.eq_ignore_ascii_case(&preferred_mac))
                .or_else(|| s.paired.iter().find(|(_, _, _, trusted)| *trusted))
                .or_else(|| s.paired.first());
            if let Some((path, _, _, _)) = pick {
                if let Ok(p) = Proxy::new(&conn, "org.bluez", path.clone(), "org.bluez.Device1") {
                    let _ = p.call_method("Connect", &());
                }
            }
        });
    }

    /// Middle click: toggle adapter power.
    pub fn toggle_power(&self) {
        let Some(conn) = self.conn.clone() else { return };
        std::thread::spawn(move || {
            let Some(s) = snapshot(&conn) else { return };
            let Some((adapter, powered)) = s.adapter else { return };
            if let Ok(p) = Proxy::new(&conn, "org.bluez", adapter, "org.bluez.Adapter1") {
                let _ = p.set_property("Powered", !powered);
            }
        });
    }
}
