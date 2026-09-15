//! The clock panel's notifications, from Huginn's notification centre.
//!
//! Huginn is the notification server. Beside `org.freedesktop.Notifications`
//! it serves `org.raven.Notifications` on the same object: `List` for what is
//! open and what recently closed, `Remove` and `Clear`, and a `Changed`
//! signal. A thread follows `Changed` and hands each new list to the bar's
//! loop. Removing is a call made on a short-lived thread of its own, as the
//! Bluetooth actions are, so a click never waits on the bus.
//!
//! Without a session bus, or with a Huginn too old to have the centre, the
//! list is simply empty and the panel says there is nothing.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use calloop::channel::Sender;
use zbus::blocking::{Connection, Proxy};

const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const CENTRE: &str = "org.raven.Notifications";

/// How long to wait before trying the session bus again.
const RETRY: Duration = Duration::from_secs(5);

/// One notification, as the panel shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: u32,
    pub app_name: String,
    pub summary: String,
    /// Plain text; Huginn has already taken the markup out.
    pub body: String,
    /// Unix seconds.
    pub arrived: i64,
    /// Still open, rather than closed and kept in Huginn's history.
    pub open: bool,
}

/// `List`'s reply: id, application, icon, summary, body, arrival, open.
type Listed = (u32, String, String, String, String, i64, bool);

pub struct Centre {
    /// The session bus, while the following thread has it.
    conn: Arc<Mutex<Option<Connection>>>,
    tx: Sender<Vec<Entry>>,
}

impl Centre {
    /// Start following the centre. Every list goes to `tx`.
    pub fn start(tx: Sender<Vec<Entry>>) -> Self {
        let conn = Arc::new(Mutex::new(None));
        let (shared, sender) = (Arc::clone(&conn), tx.clone());
        let spawned = std::thread::Builder::new()
            .name("notifications".into())
            .spawn(move || follow(&shared, &sender));
        if let Err(e) = spawned {
            eprintln!("roostbar: could not start the notifications thread: {e}");
        }
        Self { conn, tx }
    }

    /// Ask for the list again: the panel opened, and Huginn may have restarted
    /// since the last `Changed`.
    pub fn refresh(&self) {
        self.call(|proxy, tx| {
            let _ = tx.send(list(proxy));
        });
    }

    pub fn remove(&self, id: u32) {
        self.call(move |proxy, _| {
            if let Err(e) = proxy.call::<_, _, ()>("Remove", &(id,)) {
                eprintln!("roostbar: could not remove notification {id}: {e}");
            }
        });
    }

    pub fn clear(&self) {
        self.call(|proxy, _| {
            if let Err(e) = proxy.call::<_, _, ()>("Clear", &()) {
                eprintln!("roostbar: could not clear notifications: {e}");
            }
        });
    }

    fn call(&self, f: impl FnOnce(&Proxy<'static>, &Sender<Vec<Entry>>) + Send + 'static) {
        let Some(conn) = self.conn.lock().unwrap_or_else(PoisonError::into_inner).clone() else {
            return;
        };
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            if let Ok(proxy) = centre(&conn) {
                f(&proxy, &tx);
            }
        });
    }
}

fn centre(conn: &Connection) -> zbus::Result<Proxy<'static>> {
    Proxy::new(conn, NAME, PATH, CENTRE)
}

/// Send a list now and after every `Changed`, reconnecting when the bus goes.
/// Returns once the bar's loop is gone.
fn follow(shared: &Mutex<Option<Connection>>, tx: &Sender<Vec<Entry>>) {
    loop {
        if let Ok(conn) = Connection::session() {
            if let Ok(proxy) = centre(&conn) {
                // Subscribed before the first List, so a change in between
                // is not missed.
                if let Ok(changes) = proxy.receive_signal("Changed") {
                    *shared.lock().unwrap_or_else(PoisonError::into_inner) = Some(conn.clone());
                    if tx.send(list(&proxy)).is_err() {
                        return;
                    }
                    for _ in changes {
                        if tx.send(list(&proxy)).is_err() {
                            return;
                        }
                    }
                }
            }
        }
        *shared.lock().unwrap_or_else(PoisonError::into_inner) = None;
        std::thread::sleep(RETRY);
    }
}

/// The centre's list, or nothing if it cannot be had.
fn list(proxy: &Proxy) -> Vec<Entry> {
    match proxy.call::<_, _, Vec<Listed>>("List", &()) {
        Ok(listed) => listed
            .into_iter()
            .map(|(id, app_name, _icon, summary, body, arrived, open)| Entry { id, app_name, summary, body, arrived, open })
            .collect(),
        Err(e) => {
            if std::env::var_os("ROOSTBAR_DEBUG").is_some() {
                eprintln!("roostbar: notifications: {e}");
            }
            Vec::new()
        }
    }
}
