//! Bluetooth over BlueZ's D-Bus API on the system bus. Everything is polled
//! from the bar's tick and actions run on a throwaway thread so a slow
//! Connect() or a thirty-second discovery never freezes the bar.
//!
//! The bar has no panel of its own, so it cannot show a list of devices to
//! pick from; that lives in Huginn's quick settings. What the bar can do on
//! its own is pair the one device named in the config: click, and it scans
//! for that MAC, pairs, trusts and connects it. Pairing needs an agent on the
//! bus or BlueZ refuses, so one is registered for the duration — a
//! NoInputNoOutput one, which makes every pairing "just works": the person
//! clicking already named the device they want, so there is nobody to ask.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

type Managed = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

const BLUEZ: &str = "org.bluez";
const AGENT_PATH: &str = "/org/raven/roostbar/agent";
/// How long a click's discovery waits for the configured device to show up.
const SCAN_FOR: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BtState {
    /// BlueZ is not on the bus at all (not installed or bluetoothd not running).
    NoStack,
    /// Adapter present but powered off.
    Off,
    /// Powered on, nothing connected.
    Idle,
    Connected(String),
    /// A click's pairing is in flight; the text says what stage it is at.
    Busy(String),
}

/// One device BlueZ knows about, paired or merely seen.
#[derive(Clone, Debug)]
pub struct Device {
    pub path: OwnedObjectPath,
    pub addr: String,
    pub name: String,
    pub paired: bool,
    pub trusted: bool,
    pub connected: bool,
}

pub struct Bluetooth {
    conn: Option<Connection>,
    /// What a background action is doing right now, for the bar to show.
    /// `None` when nothing is in flight.
    activity: Arc<Mutex<Option<String>>>,
}

struct Snapshot {
    adapter: Option<(OwnedObjectPath, bool)>,
    discovering: bool,
    devices: Vec<Device>,
}

impl Snapshot {
    fn connected(&self) -> Option<&Device> {
        self.devices.iter().find(|d| d.connected)
    }
    fn by_addr(&self, mac: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.addr.eq_ignore_ascii_case(mac))
    }
}

fn get_bool(m: &HashMap<String, OwnedValue>, k: &str) -> bool {
    m.get(k).and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(false)
}
fn get_str(m: &HashMap<String, OwnedValue>, k: &str) -> String {
    m.get(k).and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default()
}

fn snapshot(conn: &Connection) -> Option<Snapshot> {
    let om = Proxy::new(conn, BLUEZ, "/", "org.freedesktop.DBus.ObjectManager").ok()?;
    let objs: Managed = om.call("GetManagedObjects", &()).ok()?;
    let mut s = Snapshot { adapter: None, discovering: false, devices: vec![] };
    for (path, ifaces) in objs {
        if let Some(a) = ifaces.get("org.bluez.Adapter1") {
            if s.adapter.is_none() {
                s.adapter = Some((path.clone(), get_bool(a, "Powered")));
                s.discovering = get_bool(a, "Discovering");
            }
        }
        if let Some(d) = ifaces.get("org.bluez.Device1") {
            let name = {
                let n = get_str(d, "Alias");
                if n.is_empty() { get_str(d, "Name") } else { n }
            };
            let addr = get_str(d, "Address");
            let name = if name.is_empty() { addr.clone() } else { name };
            s.devices.push(Device {
                path,
                addr,
                name,
                paired: get_bool(d, "Paired"),
                trusted: get_bool(d, "Trusted"),
                connected: get_bool(d, "Connected"),
            });
        }
    }
    s.devices.sort_by(|a, b| (!a.connected, !a.paired, &a.name).cmp(&(!b.connected, !b.paired, &b.name)));
    Some(s)
}

fn device_proxy<'a>(conn: &'a Connection, path: &OwnedObjectPath) -> zbus::Result<Proxy<'a>> {
    Proxy::new(conn, BLUEZ, path.clone(), "org.bluez.Device1")
}

fn adapter_proxy<'a>(conn: &'a Connection, path: &OwnedObjectPath) -> zbus::Result<Proxy<'a>> {
    Proxy::new(conn, BLUEZ, path.clone(), "org.bluez.Adapter1")
}

// ---------------------------------------------------------------------------
// The pairing agent
// ---------------------------------------------------------------------------

/// BlueZ's agent errors, spelled the way bluetoothd expects them.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    Rejected(String),
}

/// An `org.bluez.Agent1` that says yes to everything it can and cannot type.
///
/// Registered as NoInputNoOutput, so BlueZ negotiates "just works" pairing and
/// only ever asks for authorization, which is granted: the pairing was
/// started by a click on a device the config named. A device that insists
/// on a PIN or a typed passkey (an old keyboard) cannot be paired from the
/// bar; that needs the panel in Huginn, which can display one.
struct Agent;

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self) {}
    fn cancel(&self) {}
    fn request_pin_code(&self, _device: OwnedObjectPath) -> Result<String, AgentError> {
        Err(AgentError::Rejected("roostbar cannot type a PIN".into()))
    }
    fn display_pin_code(&self, _device: OwnedObjectPath, _pincode: String) {}
    fn request_passkey(&self, _device: OwnedObjectPath) -> Result<u32, AgentError> {
        Err(AgentError::Rejected("roostbar cannot type a passkey".into()))
    }
    fn display_passkey(&self, _device: OwnedObjectPath, _passkey: u32, _entered: u16) {}
    fn request_confirmation(&self, _device: OwnedObjectPath, _passkey: u32) -> Result<(), AgentError> {
        Ok(())
    }
    fn request_authorization(&self, _device: OwnedObjectPath) -> Result<(), AgentError> {
        Ok(())
    }
    fn authorize_service(&self, _device: OwnedObjectPath, _uuid: String) -> Result<(), AgentError> {
        Ok(())
    }
}

/// The agent, alive for as long as this is held. Its own connection, so a
/// slow callback can never sit in the way of the bar's polling.
struct AgentGuard {
    conn: Connection,
}

impl AgentGuard {
    fn register() -> zbus::Result<Self> {
        let conn = Connection::system()?;
        conn.object_server().at(AGENT_PATH, Agent)?;
        let path = ObjectPath::try_from(AGENT_PATH)?;
        let mgr = Proxy::new(&conn, BLUEZ, "/org/bluez", "org.bluez.AgentManager1")?;
        mgr.call_method("RegisterAgent", &(&path, "NoInputNoOutput"))?;
        // Best effort: bluetoothctl, if someone has it open, may already be
        // the default, and BlueZ still routes our own pairing to us.
        let _ = mgr.call_method("RequestDefaultAgent", &(&path,));
        Ok(Self { conn })
    }
}

impl Drop for AgentGuard {
    fn drop(&mut self) {
        let path = ObjectPath::try_from(AGENT_PATH).expect("static path");
        if let Ok(mgr) = Proxy::new(&self.conn, BLUEZ, "/org/bluez", "org.bluez.AgentManager1") {
            let _ = mgr.call_method("UnregisterAgent", &(&path,));
        }
        let _ = self.conn.object_server().remove::<Agent, _>(AGENT_PATH);
    }
}

// ---------------------------------------------------------------------------
// Blocking operations, for the click thread and the CLI
// ---------------------------------------------------------------------------

/// Turn discovery on, wait until `found` says yes or `deadline` passes, turn
/// it off again. Returns the device that satisfied `found`, if any.
fn discover_until(
    conn: &Connection,
    adapter: &OwnedObjectPath,
    deadline: Instant,
    mut found: impl FnMut(&Snapshot) -> Option<Device>,
) -> Result<Option<Device>, String> {
    let ad = adapter_proxy(conn, adapter).map_err(|e| e.to_string())?;
    // Already-known devices count too: a device paired elsewhere and then
    // forgotten by the remote still shows up here without a scan.
    if let Some(d) = snapshot(conn).and_then(|s| found(&s)) {
        return Ok(Some(d));
    }
    ad.call_method("StartDiscovery", &()).map_err(|e| format!("StartDiscovery: {e}"))?;
    let result = loop {
        std::thread::sleep(Duration::from_millis(500));
        let Some(s) = snapshot(conn) else { break Ok(None) };
        if let Some(d) = found(&s) {
            break Ok(Some(d));
        }
        if Instant::now() >= deadline {
            break Ok(None);
        }
    };
    let _ = ad.call_method("StopDiscovery", &());
    result
}

/// Pair `dev` if it is not already, mark it trusted so it may reconnect on
/// its own, then connect. Each step reports through `progress`.
fn pair_trust_connect(conn: &Connection, dev: &Device, mut progress: impl FnMut(String)) -> Result<(), String> {
    let p = device_proxy(conn, &dev.path).map_err(|e| e.to_string())?;
    if !dev.paired {
        progress(format!("pairing {}", dev.name));
        let _agent = AgentGuard::register().map_err(|e| format!("agent: {e}"))?;
        p.call_method("Pair", &()).map_err(|e| format!("Pair: {}", short(&e)))?;
    }
    if !dev.trusted {
        let _ = p.set_property("Trusted", true);
    }
    progress(format!("connecting {}", dev.name));
    p.call_method("Connect", &()).map_err(|e| format!("Connect: {}", short(&e)))?;
    Ok(())
}

/// BlueZ's errors come as `org.bluez.Error.Failed: br-connection-profile-unavailable`;
/// the part after the last dot of the name plus the message is what a person
/// wants to read.
fn short(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(name, msg, _) => {
            let kind = name.as_str().rsplit('.').next().unwrap_or("Failed");
            match msg {
                Some(m) if !m.is_empty() => format!("{kind} ({m})"),
                _ => kind.to_owned(),
            }
        }
        other => other.to_string(),
    }
}

impl Bluetooth {
    pub fn new() -> Self {
        Self { conn: Connection::system().ok(), activity: Arc::new(Mutex::new(None)) }
    }

    pub fn state(&mut self) -> BtState {
        if let Some(a) = self.activity.lock().ok().and_then(|a| a.clone()) {
            return BtState::Busy(a);
        }
        if self.conn.is_none() {
            self.conn = Connection::system().ok();
        }
        let Some(conn) = &self.conn else { return BtState::NoStack };
        let Some(s) = snapshot(conn) else { return BtState::NoStack };
        match s.adapter {
            None => BtState::NoStack,
            Some((_, false)) => BtState::Off,
            Some((_, true)) => match s.connected() {
                Some(d) => BtState::Connected(d.name.clone()),
                None => BtState::Idle,
            },
        }
    }

    /// Left click: connected -> disconnect; off -> power on; idle -> connect
    /// to the preferred device (config MAC, else a trusted paired one, else
    /// any paired one), pairing the config MAC first if it never has been.
    ///
    /// Returns false when there is nothing the bar can do by itself — no
    /// config MAC and nothing paired — so the caller can open the panel that
    /// has a device list.
    pub fn primary_action(&self, preferred_mac: String) -> bool {
        let Some(conn) = self.conn.clone() else { return false };
        if self.activity.lock().map(|a| a.is_some()).unwrap_or(false) {
            return true; // a click mid-pairing is a click that waits
        }
        let Some(s) = snapshot(&conn) else { return false };
        let Some((adapter, powered)) = s.adapter.clone() else { return false };

        if !powered {
            std::thread::spawn(move || {
                if let Ok(p) = adapter_proxy(&conn, &adapter) {
                    let _ = p.set_property("Powered", true);
                }
            });
            return true;
        }
        if let Some(d) = s.connected() {
            let path = d.path.clone();
            std::thread::spawn(move || {
                if let Ok(p) = device_proxy(&conn, &path) {
                    let _ = p.call_method("Disconnect", &());
                }
            });
            return true;
        }

        let preferred = (!preferred_mac.is_empty()).then(|| s.by_addr(&preferred_mac).cloned());
        let pick = match preferred {
            // Named in the config and already paired: connect it.
            Some(Some(d)) if d.paired => Some(d),
            // Named in the config but never paired: find and pair it.
            Some(_) => {
                self.pair_in_background(conn, adapter, preferred_mac);
                return true;
            }
            None => s
                .devices
                .iter()
                .find(|d| d.paired && d.trusted)
                .or_else(|| s.devices.iter().find(|d| d.paired))
                .cloned(),
        };
        let Some(dev) = pick else { return false };
        let activity = Arc::clone(&self.activity);
        std::thread::spawn(move || {
            let set = |m: String| {
                if let Ok(mut a) = activity.lock() {
                    *a = Some(m);
                }
            };
            if let Err(e) = pair_trust_connect(&conn, &dev, set) {
                eprintln!("roostbar: bluetooth: {e}");
            }
            if let Ok(mut a) = activity.lock() {
                *a = None;
            }
        });
        true
    }

    fn pair_in_background(&self, conn: Connection, adapter: OwnedObjectPath, mac: String) {
        let activity = Arc::clone(&self.activity);
        std::thread::spawn(move || {
            let set = |m: String| {
                if let Ok(mut a) = activity.lock() {
                    *a = Some(m);
                }
            };
            set(format!("scanning for {mac}"));
            let r = discover_until(&conn, &adapter, Instant::now() + SCAN_FOR, |s| s.by_addr(&mac).cloned());
            match r {
                Ok(Some(dev)) => {
                    if let Err(e) = pair_trust_connect(&conn, &dev, set) {
                        eprintln!("roostbar: bluetooth: {e}");
                    }
                }
                Ok(None) => eprintln!("roostbar: bluetooth: {mac} not seen in {}s; is it in pairing mode?", SCAN_FOR.as_secs()),
                Err(e) => eprintln!("roostbar: bluetooth: {e}"),
            }
            if let Ok(mut a) = activity.lock() {
                *a = None;
            }
        });
    }

    /// Middle click: toggle adapter power.
    pub fn toggle_power(&self) {
        let Some(conn) = self.conn.clone() else { return };
        std::thread::spawn(move || {
            let Some(s) = snapshot(&conn) else { return };
            let Some((adapter, powered)) = s.adapter else { return };
            if let Ok(p) = adapter_proxy(&conn, &adapter) {
                let _ = p.set_property("Powered", !powered);
            }
        });
    }
}

// ---------------------------------------------------------------------------
// `roostbar bt ...`
// ---------------------------------------------------------------------------

/// The command line, for a machine without `bluetoothctl`. Blocking, prints
/// as it goes, exits non-zero on failure.
pub fn cli(args: &[String], preferred_mac: &str) -> i32 {
    let conn = match Connection::system() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("roostbar bt: system bus: {e}");
            return 1;
        }
    };
    let Some(s) = snapshot(&conn) else {
        eprintln!("roostbar bt: org.bluez is not on the bus (is bluetoothd running?)");
        return 1;
    };
    let Some((adapter, powered)) = s.adapter.clone() else {
        eprintln!("roostbar bt: no Bluetooth adapter");
        return 1;
    };
    let verb = args.first().map(String::as_str).unwrap_or("list");
    let arg = args.get(1).map(String::as_str);
    let say = |m: String| println!("{m}…");

    // A MAC from the command line, else the config, else nothing.
    let target = |s: &Snapshot| -> Option<Device> {
        let mac = arg.or_else(|| (!preferred_mac.is_empty()).then_some(preferred_mac))?;
        s.by_addr(mac).cloned()
    };

    let result: Result<(), String> = match verb {
        "list" | "ls" => {
            println!("adapter: {}", if powered { "on" } else { "off" });
            for d in &s.devices {
                let mut flags = vec![];
                if d.connected {
                    flags.push("connected");
                }
                if d.paired {
                    flags.push("paired");
                }
                if d.trusted {
                    flags.push("trusted");
                }
                println!("{}  {}  {}", d.addr, d.name, flags.join(","));
            }
            Ok(())
        }
        "on" | "off" => adapter_proxy(&conn, &adapter)
            .map_err(|e| e.to_string())
            .and_then(|p| p.set_property("Powered", verb == "on").map_err(|e| e.to_string())),
        "scan" => {
            let secs: u64 = arg.and_then(|a| a.parse().ok()).unwrap_or(15);
            if !powered {
                return fail("adapter is off; `roostbar bt on` first");
            }
            // What has been printed for each address. A device is announced
            // when it first appears and again once its name arrives: BlueZ
            // creates the object from the advertisement, with the address
            // as a stand-in alias, and fills the name in a beat later.
            let mut seen: HashMap<String, String> = s.devices.iter().map(|d| (d.addr.clone(), d.name.clone())).collect();
            eprintln!("scanning for {secs}s…");
            discover_until(&conn, &adapter, Instant::now() + Duration::from_secs(secs), |s| {
                for d in &s.devices {
                    if seen.get(&d.addr) != Some(&d.name) {
                        seen.insert(d.addr.clone(), d.name.clone());
                        let name = if d.name.replace('-', ":") == d.addr { "(no name yet)" } else { d.name.as_str() };
                        println!("{}  {name}", d.addr);
                    }
                }
                None
            })
            .map(|_| ())
        }
        "pair" => {
            let Some(mac) = arg.or_else(|| (!preferred_mac.is_empty()).then_some(preferred_mac)) else {
                return fail("pair needs a MAC (or `bluetooth_device` in the config)");
            };
            if !powered {
                return fail("adapter is off; `roostbar bt on` first");
            }
            eprintln!("looking for {mac} (put it in pairing mode)…");
            match discover_until(&conn, &adapter, Instant::now() + SCAN_FOR, |s| s.by_addr(mac).cloned()) {
                Ok(Some(dev)) => pair_trust_connect(&conn, &dev, say),
                Ok(None) => Err(format!("{mac} not seen in {}s", SCAN_FOR.as_secs())),
                Err(e) => Err(e),
            }
        }
        "connect" => match target(&s).or_else(|| s.devices.iter().find(|d| d.paired && d.trusted).cloned()) {
            Some(dev) => pair_trust_connect(&conn, &dev, say),
            None => return fail("nothing to connect: give a MAC or pair something first"),
        },
        "disconnect" => match s.connected() {
            Some(d) => device_proxy(&conn, &d.path)
                .and_then(|p| p.call_method("Disconnect", &()).map(|_| ()))
                .map_err(|e| short(&e)),
            None => Ok(()),
        },
        "forget" | "remove" => match target(&s) {
            Some(dev) => adapter_proxy(&conn, &adapter)
                .and_then(|p| p.call_method("RemoveDevice", &(&dev.path,)).map(|_| ()))
                .map_err(|e| short(&e)),
            None => return fail("forget needs the MAC of a known device"),
        },
        other => return fail(&format!("unknown action {other:?} (list|on|off|scan [secs]|pair [MAC]|connect [MAC]|disconnect|forget MAC)")),
    };
    match result {
        Ok(()) => 0,
        Err(e) => fail(&e),
    }
}

fn fail(msg: &str) -> i32 {
    eprintln!("roostbar bt: {msg}");
    1
}
