mod audio;
mod bluetooth;
mod config;
mod notifications;
#[cfg(feature = "pipewire-native")]
mod pipewire_audio;
#[cfg(not(feature = "pipewire-native"))]
mod pipewire_cli;
#[cfg(not(feature = "pipewire-native"))]
use pipewire_cli as pipewire_audio;
mod raven_shell;
mod render;
mod system;

use std::io::Read;
use std::time::{Duration, Instant, SystemTime};

use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, Interest, Mode, PostAction};
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_dispatch2, delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        pointer::{PointerEvent, PointerEventKind, PointerHandler, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, Dispatch, QueueHandle,
};

use audio::{Audio, Volume};
use bluetooth::{BtState, Bluetooth};
use config::{parse_color, Config};
use raven_shell::raven_shell_manager_v1::RavenShellManagerV1;
use render::{Canvas, Text};
use system::{Battery, Wifi};

enum AudioBackend {
    PipeWire(pipewire_audio::PwClient),
    Alsa(Audio),
    None,
}

impl AudioBackend {
    fn name(&self) -> &'static str {
        match self {
            Self::PipeWire(_) => "PipeWire",
            Self::Alsa(_) => "ALSA",
            Self::None => "none",
        }
    }

    /// Pick the best backend available right now. PipeWire wins when it is
    /// running *and* actually has a sink (a daemon without pipewire-audio
    /// installed has none); otherwise plain ALSA on the laptop codec.
    fn select(cfg: &Config) -> Self {
        if let Some(p) = pipewire_audio::PwClient::start() {
            if p.has_sinks() {
                return Self::PipeWire(p);
            }
        }
        match Audio::open(&cfg.alsa_card, &cfg.alsa_mixer) {
            Some(a) => Self::Alsa(a),
            None => Self::None,
        }
    }

    fn get(&self) -> Option<Volume> {
        match self {
            Self::PipeWire(p) => p.get(),
            Self::Alsa(a) => a.get(),
            Self::None => None,
        }
    }
    fn adjust(&self, d: i64) {
        match self {
            Self::PipeWire(p) => p.adjust(d),
            Self::Alsa(a) => a.adjust(d),
            Self::None => {}
        }
    }
    fn toggle_mute(&self) {
        match self {
            Self::PipeWire(p) => p.toggle_mute(),
            Self::Alsa(a) => a.toggle_mute(),
            Self::None => {}
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Module {
    Date,
    Wifi,
    Bluetooth,
    Volume,
    Battery,
    Clock,
}

impl Module {
    /// Whether a click does something here; see [`Bar::click`].
    fn clickable(self) -> bool {
        matches!(self, Self::Date | Self::Wifi | Self::Volume | Self::Bluetooth | Self::Battery | Self::Clock)
    }
}

/// White at ~9%: the line where the bar meets the desktop. ARGB.
const HAIRLINE: [u8; 4] = [0x16, 0xFF, 0xFF, 0xFF];
/// White at ~14%: the pill behind a hovered, clickable module. ARGB.
const HOVER_PILL: [u8; 4] = [0x24, 0xFF, 0xFF, 0xFF];

// The clock panel, in logical pixels.
const PANEL_WIDTH: u32 = 360;
/// Between the panel and the bar, and the screen's edge.
const PANEL_MARGIN: i32 = 8;
const PANEL_PAD: f32 = 16.0;
const PANEL_TOP: f32 = 12.0;
const PANEL_BOTTOM: f32 = 14.0;
const PANEL_GAP: f32 = 12.0;
const PANEL_GAP_SMALL: f32 = 6.0;
const PANEL_RADIUS: f32 = 14.0;
const ROW_PAD: f32 = 7.0;
const ROW_GAP: f32 = 6.0;
const ROW_RADIUS: f32 = 10.0;
/// The tallest the notification list gets before it scrolls.
const LIST_MAX: f32 = 420.0;
/// The list's height when it only says there is nothing in it.
const EMPTY_HEIGHT: f32 = 64.0;
/// How far one notch of a wheel scrolls the list.
const SCROLL_NOTCH: f32 = 48.0;
/// White at ~5% and ~9%: a notification's row, and the row under the pointer.
const ROW_FILL: [u8; 4] = [0x0D, 0xFF, 0xFF, 0xFF];
const ROW_HOVER: [u8; 4] = [0x18, 0xFF, 0xFF, 0xFF];
const EMPTY_TEXT: &str = "No new notifications";

/// Something on the clock panel a click does something to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PanelHit {
    Remove(u32),
    Clear,
}

/// The clock panel: the time, and the notifications Huginn holds.
struct Panel {
    layer: LayerSurface,
    pool: SlotPool,
    /// A transparent surface over the rest of the screen, under the panel: a
    /// press on it is a click outside the panel, and closes it. Its exclusive
    /// zone is 0, so it stays clear of the bar's own strip.
    catcher: LayerSurface,
    catcher_pool: SlotPool,
    /// The height last asked of the compositor, logical.
    requested: u32,
    /// The size the compositor configured, logical.
    width: u32,
    height: u32,
    scale: i32,
    configured: bool,
    dirty: bool,
    /// Where the pointer is on the panel, logical, while it is on it.
    pointer: Option<(f64, f64)>,
    /// The visible part of each row, and each control, as last drawn, in
    /// buffer pixels: drawing and clicking share one layout.
    rows: Vec<[f32; 4]>,
    hits: Vec<([f32; 4], PanelHit)>,
    /// How far the list is scrolled, logical.
    scroll: f32,
}

impl Panel {
    /// The row and the control under the pointer, as indices into `rows` and
    /// `hits`.
    fn hover(&self) -> (Option<usize>, Option<usize>) {
        let Some((x, y)) = self.pointer else { return (None, None) };
        let s = self.scale as f32;
        let (x, y) = (x as f32 * s, y as f32 * s);
        let inside = |r: &[f32; 4]| x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3];
        (self.rows.iter().position(inside), self.hits.iter().position(|(r, _)| inside(r)))
    }
}

/// The heights of the clock panel's parts, from the bar's font size.
struct PanelMetrics {
    clock: f32,
    line: f32,
    small: f32,
    header: f32,
    row: f32,
}

impl PanelMetrics {
    fn new(font_size: f32) -> Self {
        let line = (font_size * 1.6).round();
        let small = (font_size * 1.4).round();
        Self {
            clock: (font_size * 3.4).round(),
            line,
            small,
            header: (font_size * 2.3).round(),
            row: ROW_PAD * 2.0 + small * 2.0 + line,
        }
    }

    fn content_height(&self, n: usize) -> f32 {
        if n == 0 {
            EMPTY_HEIGHT
        } else {
            n as f32 * self.row + (n - 1) as f32 * ROW_GAP
        }
    }

    fn list_height(&self, n: usize) -> f32 {
        self.content_height(n).min(LIST_MAX)
    }

    fn max_scroll(&self, n: usize) -> f32 {
        (self.content_height(n) - self.list_height(n)).max(0.0)
    }

    /// The whole panel's height; [`Bar::paint_panel`] lays out the same sum.
    fn height(&self, n: usize) -> u32 {
        let sum = PANEL_TOP
            + self.clock
            + self.line
            + PANEL_GAP
            + 1.0
            + PANEL_GAP_SMALL
            + self.header
            + PANEL_GAP_SMALL
            + self.list_height(n)
            + PANEL_BOTTOM;
        sum.ceil() as u32
    }
}

/// How long ago a notification arrived, as the panel says it.
fn ago(arrived: i64, now: i64) -> String {
    match now - arrived {
        s if s < 60 => "now".into(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86_400 => format!("{} h ago", s / 3600),
        _ => chrono::DateTime::from_timestamp(arrived, 0)
            .map(|t| t.with_timezone(&chrono::Local).format("%d %b").to_string())
            .unwrap_or_default(),
    }
}

/// Text on one line: a body's line breaks and runs of spaces become one space.
fn flatten(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

struct Segment {
    module: Module,
    text: String,
    color: [u8; 4],
    x0: f32,
    x1: f32,
}

struct Bar {
    cfg: Config,
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    /// Huginn's shell protocol, bound only when the compositor speaks the
    /// version that has `open_quick_settings`: the fallback for a click when
    /// Raven Settings or Raven Power is not installed. None on an older Huginn.
    shell: Option<RavenShellManagerV1>,
    /// Kept to make the clock panel's surface when it opens.
    compositor: CompositorState,
    layer_shell: LayerShell,
    qh: QueueHandle<Bar>,
    /// Open while the clock has been clicked.
    panel: Option<Panel>,
    centre: notifications::Centre,
    /// Huginn's notifications, newest first, as the centre last sent them.
    notes: Vec<notifications::Entry>,
    pointer: Option<wl_pointer::WlPointer>,
    loop_handle: calloop::LoopHandle<'static, Bar>,
    wake_token: Option<calloop::RegistrationToken>,
    debug: bool,

    width: u32,
    height: u32,
    scale: i32,
    configured: bool,
    dirty: bool,
    exit: bool,
    text: Text,
    colors: Colors,
    segments: Vec<Segment>,
    pointer_x: f64,
    hover: Option<Module>,
    /// Sub-pixel scroll accumulator so smooth-scrolling mice change volume sanely.
    scroll_acc: f64,

    audio: AudioBackend,
    bt: Bluetooth,
    volume: Option<Volume>,
    bt_state: BtState,
    wifi: Wifi,
    battery: Option<Battery>,
    /// raven-powerd says the machine is in its power-saver profile.
    eco: bool,
    clock: String,
    date: String,
    /// The date as the clock panel spells it out: "Monday 15 September".
    long_date: String,
    last_slow_poll: Instant,
    /// Mtime of the config file as of the last read, so the slow poll can
    /// tell an edit from a file that has not moved.
    cfg_mtime: Option<SystemTime>,
}

struct Colors {
    bg: [u8; 4],
    fg: [u8; 4],
    accent: [u8; 4],
    muted: [u8; 4],
    warning: [u8; 4],
    charging: [u8; 4],
}

impl Colors {
    fn from_config(cfg: &Config) -> Self {
        Self {
            bg: parse_color(&cfg.background),
            fg: parse_color(&cfg.foreground),
            accent: parse_color(&cfg.accent),
            muted: parse_color(&cfg.muted),
            warning: parse_color(&cfg.warning),
            charging: parse_color(if cfg.charging.trim().is_empty() {
                &cfg.accent
            } else {
                &cfg.charging
            }),
        }
    }
}

fn is_running(name: &str) -> bool {
    let Ok(rd) = std::fs::read_dir("/proc") else { return false };
    for e in rd.flatten() {
        if let Ok(comm) = std::fs::read_to_string(e.path().join("comm")) {
            if comm.trim() == name {
                return true;
            }
        }
    }
    false
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

/// Start a desktop app detached from the bar: its own process group, so it
/// outlives a bar restart, and no stdio, so its logging stays out of ours.
/// False when it could not be started at all (not installed).
fn launch(bin: &str, args: &[&str]) -> bool {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let spawned = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
    match spawned {
        Ok(mut child) => {
            // Reap it when it exits, or every closed window leaves a zombie.
            std::thread::spawn(move || child.wait());
            true
        }
        Err(e) => {
            eprintln!("roostbar: could not start {bin}: {e}");
            false
        }
    }
}

/// Raven has no systemd user session, so nothing starts PipeWire for us.
/// Bring it up if it's installed and not already running. Without a session
/// manager PipeWire exposes no devices, so wireplumber is the gate.
fn spawn_pipewire() {
    use std::process::{Command, Stdio};
    if !which("pipewire") || is_running("pipewire") {
        return;
    }
    // Without the SPA audio plugins (package pipewire-audio) PipeWire cannot
    // touch a sound card, so starting it would only produce a useless daemon.
    let spa_ok = ["alsa", "audioconvert"].iter().all(|d| std::path::Path::new("/usr/lib/spa-0.2").join(d).is_dir());
    if !spa_ok {
        eprintln!("roostbar: pipewire-audio not installed (no /usr/lib/spa-0.2/alsa); using ALSA. `sudo rvn install -y pipewire-audio wireplumber pipewire-pulse` to switch.");
        return;
    }
    // With wireplumber: the standard trio. Without it PipeWire would expose no
    // devices at all, so use its shipped minimal.conf, which enumerates ALSA
    // via udev -- patched to drop the pulse/jack modules that need packages
    // which may be missing.
    let plan: Vec<(&str, Vec<String>)> = if which("wireplumber") {
        let mut v = vec![("pipewire", vec![]), ("wireplumber", vec![])];
        if which("pipewire-pulse") {
            v.push(("pipewire-pulse", vec![]));
        }
        v
    } else {
        let Some(conf) = minimal_conf() else { return };
        eprintln!("roostbar: wireplumber not installed; starting pipewire with a session-manager-free config");
        vec![("pipewire", vec!["-c".to_string(), conf])]
    };
    let mut started = false;
    for (bin, args) in plan {
        if which(bin) && !is_running(bin) {
            let ok = Command::new(bin)
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .is_ok();
            if ok {
                eprintln!("roostbar: started {bin}");
                started = true;
            }
            if bin == "pipewire" {
                std::thread::sleep(Duration::from_millis(300));
            }
        }
    }
    if started {
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// A copy of PipeWire's minimal.conf with the pulse and jack modules turned
/// off, written to XDG_RUNTIME_DIR. Only the property lines are touched, not
/// the `condition = [ { … } ]` lines that test them.
fn minimal_conf() -> Option<String> {
    let src = std::fs::read_to_string("/usr/share/pipewire/minimal.conf").ok()?;
    let patched: String = src
        .lines()
        .map(|l| {
            let t = l.trim_start();
            if t.starts_with("minimal.use-pulse") && !which("pipewire-pulse") && t.contains("true") {
                l.replacen("true", "false", 1)
            } else if t.starts_with("minimal.use-jack-tunnel") && t.contains("true") {
                l.replacen("true", "false", 1)
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let dir = std::env::var("XDG_RUNTIME_DIR").ok()?;
    let path = format!("{dir}/roostbar-pipewire.conf");
    std::fs::write(&path, patched).ok()?;
    Some(path)
}

/// Connect to the compositor, waiting for it if we were started ahead of it
/// (the Raven session script runs us before it execs Huginn). If
/// WAYLAND_DISPLAY is unset, find the socket ourselves -- Huginn's is
/// `wayland-1`, not the `wayland-0` libwayland assumes.
fn connect_wayland() -> Option<Connection> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
                let mut socks: Vec<String> = std::fs::read_dir(&rt)
                    .map(|rd| {
                        rd.flatten()
                            .filter_map(|e| e.file_name().into_string().ok())
                            .filter(|n| n.starts_with("wayland-") && !n.ends_with(".lock"))
                            .collect()
                    })
                    .unwrap_or_default();
                socks.sort();
                if let Some(s) = socks.first() {
                    std::env::set_var("WAYLAND_DISPLAY", s);
                }
            }
        }
        if let Ok(c) = Connection::connect_to_env() {
            return Some(c);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::env::remove_var("WAYLAND_DISPLAY");
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn main() {
    let cfg_mtime = Config::mtime();
    let cfg = Config::load();
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("roostbar {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if let Some(cmd) = std::env::args().nth(1).filter(|a| a == "vol" || a == "volume") {
        let _ = cmd;
        let arg = std::env::args().nth(2).unwrap_or_else(|| "get".into());
        let audio = AudioBackend::select(&cfg);
        match arg.as_str() {
            "up" | "+" => audio.adjust(cfg.volume_step),
            "down" | "-" => audio.adjust(-cfg.volume_step),
            "mute" | "toggle" => audio.toggle_mute(),
            "get" => {}
            other => {
                eprintln!("roostbar vol: unknown action {other:?} (up|down|mute|get)");
                std::process::exit(2);
            }
        }
        std::thread::sleep(Duration::from_millis(120));
        match audio.get() {
            Some(v) => println!("{}%{} ({})", v.percent, if v.muted { " muted" } else { "" }, audio.name()),
            None => {
                println!("no audio");
                std::process::exit(1);
            }
        }
        return;
    }
    if std::env::args().nth(1).is_some_and(|a| a == "bt" || a == "bluetooth") {
        let rest: Vec<String> = std::env::args().skip(2).collect();
        std::process::exit(bluetooth::cli(&rest, &cfg.bluetooth_device));
    }
    if cfg.start_pipewire {
        spawn_pipewire();
    }

    let conn = connect_wayland().unwrap_or_else(|| {
        eprintln!("roostbar: no Wayland compositor found within 15s");
        std::process::exit(1);
    });
    let (globals, event_queue) = registry_queue_init(&conn).expect("roostbar: registry");
    let qh: QueueHandle<Bar> = event_queue.handle();
    let mut event_loop: EventLoop<Bar> = EventLoop::try_new().expect("roostbar: event loop");
    WaylandSource::new(conn.clone(), event_queue).insert(event_loop.handle()).expect("roostbar: wayland source");

    let compositor = CompositorState::bind(&globals, &qh).expect("roostbar: wl_compositor");
    let layer_shell = LayerShell::bind(&globals, &qh).expect("roostbar: compositor has no zwlr_layer_shell_v1");
    let shm = Shm::bind(&globals, &qh).expect("roostbar: wl_shm");
    // Version 2 is where open_quick_settings appeared; a compositor that
    // only offers 1 cannot take the request, so it is the same as no global.
    let shell: Option<RavenShellManagerV1> = globals.bind(&qh, 2..=2, ()).ok();
    if shell.is_none() {
        eprintln!("roostbar: compositor has no raven_shell_manager_v1 v2; no quick-settings fallback if Settings or Raven Power is missing");
    }

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Top, Some("roostbar"), None);
    let edge = if cfg.position == "bottom" { Anchor::BOTTOM } else { Anchor::TOP };
    layer.set_anchor(edge | Anchor::LEFT | Anchor::RIGHT);
    layer.set_size(0, cfg.height);
    layer.set_exclusive_zone(if cfg.exclusive { cfg.height as i32 } else { 0 });
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.commit();

    let pool = SlotPool::new(1920 * cfg.height as usize * 4, &shm).expect("roostbar: shm pool");
    let text = match Text::load(&cfg.ui_font, &cfg.font, cfg.font_size) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("roostbar: font: {e}");
            std::process::exit(1);
        }
    };
    let colors = Colors::from_config(&cfg);

    let audio = AudioBackend::select(&cfg);
    match &audio {
        AudioBackend::Alsa(a) => eprintln!("roostbar: audio via ALSA ({}, {})", a.card, cfg.alsa_mixer),
        other => eprintln!("roostbar: audio via {}", other.name()),
    }
    let debug = std::env::var_os("ROOSTBAR_DEBUG").is_some();

    let (notes_tx, notes_rx) = calloop::channel::channel::<Vec<notifications::Entry>>();
    event_loop
        .handle()
        .insert_source(notes_rx, |event, _, bar: &mut Bar| {
            if let calloop::channel::Event::Msg(notes) = event {
                bar.notes_changed(notes);
            }
        })
        .expect("roostbar: notifications source");
    let centre = notifications::Centre::start(notes_tx);

    let mut bar = Bar {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        layer,
        shell,
        compositor,
        layer_shell,
        qh: qh.clone(),
        panel: None,
        centre,
        notes: Vec::new(),
        pointer: None,
        loop_handle: event_loop.handle(),
        wake_token: None,
        debug,
        width: 0,
        height: cfg.height,
        scale: 1,
        configured: false,
        dirty: true,
        exit: false,
        text,
        colors,
        segments: Vec::new(),
        pointer_x: 0.0,
        hover: None,
        scroll_acc: 0.0,
        audio,
        bt: Bluetooth::new(),
        volume: None,
        bt_state: BtState::NoStack,
        wifi: Wifi::Unavailable,
        battery: None,
        eco: false,
        clock: String::new(),
        date: String::new(),
        long_date: String::new(),
        last_slow_poll: Instant::now() - Duration::from_secs(60),
        cfg_mtime,
        cfg,
    };
    bar.install_wake_source();
    bar.refresh_fast();
    bar.refresh_slow();

    event_loop
        .handle()
        .insert_source(Timer::from_duration(Duration::from_secs(1)), |_, _, bar: &mut Bar| {
            bar.refresh_fast();
            if bar.last_slow_poll.elapsed() >= Duration::from_secs(4) {
                bar.refresh_slow();
            }
            bar.draw();
            bar.draw_panel();
            // Align to the next whole second so the clock flips on time.
            let now = chrono::Local::now();
            let ms = now.timestamp_subsec_millis() as u64;
            TimeoutAction::ToDuration(Duration::from_millis(1000 - ms.min(999)))
        })
        .expect("roostbar: timer");

    loop {
        if let Err(e) = event_loop.dispatch(None, &mut bar) {
            eprintln!("roostbar: {e}");
            break;
        }
        if bar.exit {
            break;
        }
        // A dead compositor connection delivers no more events, but its
        // socket reports HUP forever — and calloop's level-triggered poll
        // turns that into a busy loop: an invisible bar burning a core for
        // hours. It happens for real: a flapping monitor cable can make the
        // compositor kill every client with a protocol error mid-session.
        // Restarting reconnects (waiting for the compositor if need be) and
        // puts the bar back on screen.
        if conn.protocol_error().is_some() || conn.flush().is_err() {
            eprintln!("roostbar: wayland connection lost; restarting");
            restart();
        }
    }
}

/// Replace this process with a fresh copy of itself.
///
/// Used when the compositor connection dies: the session manager only starts
/// the bar once, so exiting would leave the session bar-less for good. The
/// pause bounds the restart rate if the compositor is refusing us on sight.
fn restart() -> ! {
    use std::os::unix::process::CommandExt;
    std::thread::sleep(Duration::from_secs(1));
    let exe = std::env::current_exe().unwrap_or_else(|_| "/proc/self/exe".into());
    let err = std::process::Command::new(exe).args(std::env::args_os().skip(1)).exec();
    eprintln!("roostbar: restart failed: {err}");
    std::process::exit(1);
}

impl Bar {
    /// PipeWire wakes us through its self-pipe so volume keys and other
    /// clients show up instantly rather than on the next tick.
    fn install_wake_source(&mut self) {
        if let Some(t) = self.wake_token.take() {
            self.loop_handle.remove(t);
        }
        let AudioBackend::PipeWire(p) = &self.audio else { return };
        let Ok(fd) = p.wake_rx.try_clone() else { return };
        self.wake_token = self
            .loop_handle
            .insert_source(Generic::new(fd, Interest::READ, Mode::Level), |_, stream, bar: &mut Bar| {
                let mut buf = [0u8; 64];
                let mut closed = false;
                // SAFETY: Generic hands back the same UnixStream we inserted.
                while let Ok(n) = unsafe { stream.get_mut() }.read(&mut buf) {
                    if n == 0 {
                        closed = true;
                        break;
                    }
                }
                bar.refresh_fast();
                bar.draw();
                // A read of zero is the writer gone: PipeWire's thread ended.
                // Under level-triggered polling an EOF'd socket is readable
                // forever, so leaving the source in place would spin the
                // loop; the next slow poll re-selects a backend anyway.
                Ok(if closed { PostAction::Remove } else { PostAction::Continue })
            })
            .ok();
    }

    /// Re-evaluate the audio backend: PipeWire may have come up (packages
    /// installed, daemon started) or gone away since we last looked.
    fn reselect_audio(&mut self) {
        let pw_socket = std::env::var("XDG_RUNTIME_DIR")
            .map(|d| std::path::Path::new(&d).join("pipewire-0").exists())
            .unwrap_or(false);
        let switch = match &self.audio {
            AudioBackend::PipeWire(p) => !p.alive(),
            AudioBackend::Alsa(_) => pw_socket,
            AudioBackend::None => true,
        };
        if !switch {
            return;
        }
        let next = AudioBackend::select(&self.cfg);
        let changed = std::mem::discriminant(&next) != std::mem::discriminant(&self.audio)
            || matches!(next, AudioBackend::PipeWire(_));
        if changed {
            eprintln!("roostbar: audio via {}", next.name());
            self.audio = next;
            self.install_wake_source();
            self.dirty = true;
        }
    }

    fn refresh_fast(&mut self) {
        let now = chrono::Local::now();
        let clock = now.format(&self.cfg.clock_format).to_string();
        let date = now.format(&self.cfg.date_format).to_string();
        let long_date = now.format("%A %-d %B").to_string();
        if clock != self.clock || long_date != self.long_date {
            // The panel's clock, and each notification's "5 min ago".
            if let Some(panel) = &mut self.panel {
                panel.dirty = true;
            }
            self.long_date = long_date;
        }
        let volume = self.audio.get();
        if clock != self.clock || date != self.date || volume != self.volume {
            if self.debug && volume != self.volume {
                let sink = match &self.audio {
                    AudioBackend::PipeWire(p) => p.sink_description().unwrap_or_default(),
                    _ => String::new(),
                };
                eprintln!("roostbar: volume {:?} via {} {sink}", volume, self.audio.name());
            }
            self.clock = clock;
            self.date = date;
            self.volume = volume;
            self.dirty = true;
        }
    }

    /// Pick up an edit to `~/.config/roostbar/config.toml`.
    ///
    /// Raven Settings rewrites that file whenever the theme, the accent, the
    /// clock format or the bar's edge changes, and until this the bar wore
    /// the version it was started with until someone restarted it by hand.
    /// A stat every slow poll is cheaper than a watch and cannot miss a
    /// rename, which is how the file is written.
    fn reload_config(&mut self) {
        let mtime = Config::mtime();
        if mtime == self.cfg_mtime {
            return;
        }
        self.cfg_mtime = mtime;
        let new = Config::load();

        // The font is the one thing that can fail; a bad path should leave
        // the bar readable in the face it already has rather than blank it.
        if new.ui_font != self.cfg.ui_font
            || new.font != self.cfg.font
            || new.font_size != self.cfg.font_size
        {
            match Text::load(&new.ui_font, &new.font, new.font_size) {
                Ok(t) => self.text = t,
                Err(e) => eprintln!("roostbar: font: {e}; keeping the one in use"),
            }
        }

        // Geometry goes back to the compositor; the height we draw at
        // arrives with the configure this provokes.
        if new.position != self.cfg.position
            || new.height != self.cfg.height
            || new.exclusive != self.cfg.exclusive
        {
            let edge = if new.position == "bottom" { Anchor::BOTTOM } else { Anchor::TOP };
            self.layer.set_anchor(edge | Anchor::LEFT | Anchor::RIGHT);
            self.layer.set_size(0, new.height);
            self.layer.set_exclusive_zone(if new.exclusive { new.height as i32 } else { 0 });
            self.layer.commit();
        }

        if new.alsa_card != self.cfg.alsa_card || new.alsa_mixer != self.cfg.alsa_mixer {
            self.audio = AudioBackend::select(&new);
            eprintln!("roostbar: audio via {}", self.audio.name());
            self.install_wake_source();
        }

        self.colors = Colors::from_config(&new);
        self.cfg = new;
        self.dirty = true;
        eprintln!("roostbar: reloaded {}", Config::path().display());
        // The clock and date are cached strings; re-format them now so a
        // changed clock_format shows on this frame and not the next second's.
        self.refresh_fast();
    }

    fn refresh_slow(&mut self) {
        self.last_slow_poll = Instant::now();
        self.reload_config();
        self.reselect_audio();
        let battery = system::battery(&self.cfg.battery);
        let wifi = system::wifi(&self.cfg.wifi_interface);
        let bt = self.bt.state();
        let eco = system::eco_mode();
        if battery != self.battery || wifi != self.wifi || bt != self.bt_state || eco != self.eco {
            self.battery = battery;
            self.wifi = wifi;
            self.bt_state = bt;
            self.eco = eco;
            self.dirty = true;
        }
    }

    fn build_segments(&mut self) {
        let c = &self.colors;
        let mut right: Vec<(Module, String, [u8; 4])> = Vec::new();

        match &self.wifi {
            Wifi::Connected(ssid) => right.push((Module::Wifi, format!("󰤨 {ssid}"), c.fg)),
            Wifi::Disconnected => right.push((Module::Wifi, "󰤭".into(), c.muted)),
            Wifi::Unavailable => {}
        }
        match &self.bt_state {
            BtState::Connected(name) => right.push((Module::Bluetooth, format!("󰂱 {name}"), c.accent)),
            BtState::Idle => right.push((Module::Bluetooth, "󰂯".into(), c.fg)),
            BtState::Off => right.push((Module::Bluetooth, "󰂲".into(), c.muted)),
            BtState::NoStack => right.push((Module::Bluetooth, "󰂲 —".into(), c.muted)),
            BtState::Busy(what) => right.push((Module::Bluetooth, format!("󰂯 {what}…"), c.accent)),
        }
        match self.volume {
            Some(Volume { muted: true, .. }) => right.push((Module::Volume, "󰝟 mute".into(), c.muted)),
            Some(Volume { percent, .. }) => {
                let icon = if percent == 0 { "󰕿" } else if percent < 50 { "󰖀" } else { "󰕾" };
                right.push((Module::Volume, format!("{icon} {percent}%"), c.fg));
            }
            None => right.push((Module::Volume, "󰝟 —".into(), c.muted)),
        }
        if let Some(b) = &self.battery {
            let icons = ["󰂎", "󰁺", "󰁻", "󰁼", "󰁽", "󰁾", "󰁿", "󰂀", "󰂁", "󰂂", "󰁹"];
            let idx = ((b.percent as usize) * 10 / 100).min(10);
            // Three plugged-in shapes: filling (bolt), full and still on the
            // charger (plug), full and off it. The last one is the moment
            // after the adapter is pulled, while the kernel still says Full
            // but no supply reports `online`; the plain battery icon is the
            // honest one there.
            let (icon, col) = if b.charging {
                ("󰂄", c.charging)
            } else if b.full && b.plugged {
                ("󰚥", c.charging)
            } else if b.full {
                ("󰁹", c.fg)
            } else if b.percent <= self.cfg.battery_low {
                (icons[idx], c.warning)
            } else {
                (icons[idx], c.fg)
            };
            // Eco: a leaf ahead of the battery, so the reason the machine is
            // being frugal is visible where the battery is read.
            let text = if self.eco {
                format!("󰌪 {icon} {}%", b.percent)
            } else {
                format!("{icon} {}%", b.percent)
            };
            right.push((Module::Battery, text, col));
        }
        right.push((Module::Clock, self.clock.clone(), c.fg));

        let scale = self.text.with_scale(self.scale as f32);
        let pad = self.cfg.padding as f32 * self.scale as f32;
        let gap = self.cfg.gap as f32 * self.scale as f32;
        let w = (self.width * self.scale as u32) as f32;

        let mut segs = Vec::new();
        let mut x = w - pad;
        for (module, text, color) in right.into_iter().rev() {
            let tw = self.text.width(&text, scale);
            x -= tw;
            segs.push(Segment { module, text, color, x0: x, x1: x + tw });
            x -= gap;
        }
        if self.cfg.show_date {
            let tw = self.text.width(&self.date, scale);
            segs.push(Segment { module: Module::Date, text: self.date.clone(), color: c.muted, x0: pad, x1: pad + tw });
        }
        self.segments = segs;
    }

    fn draw(&mut self) {
        if !self.configured || !self.dirty || self.width == 0 {
            return;
        }
        self.dirty = false;
        self.build_segments();

        let pw = self.width * self.scale as u32;
        let ph = self.height * self.scale as u32;
        let stride = pw as i32 * 4;
        let (buffer, canvas_buf) = match self.pool.create_buffer(pw as i32, ph as i32, stride, wl_shm::Format::Argb8888) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("roostbar: buffer: {e}");
                return;
            }
        };
        let mut canvas = Canvas::new(canvas_buf, pw, ph);
        canvas.fill(self.colors.bg);

        // The hairline on the edge that faces the desktop: the bar is a
        // strip of the same glass as the dock, and a strip needs an edge
        // where it stops or it bleeds into whatever is under it.
        let hairline_y = if self.cfg.position == "bottom" { 0 } else { ph as i32 - 1 };
        canvas.hline(hairline_y, HAIRLINE);

        let scale = self.text.with_scale(self.scale as f32);
        let s = self.scale as f32;
        for seg in &self.segments {
            let hovered = self.hover == Some(seg.module);
            if hovered && seg.module.clickable() {
                // A pill behind the module, inset from the bar's edges so it
                // reads as a highlight on the strip rather than a cut in it.
                let pad = 8.0 * s;
                let inset = 4.0 * s;
                let h = ph as f32 - inset * 2.0;
                canvas.fill_rounded(seg.x0 - pad, inset, seg.x1 - seg.x0 + pad * 2.0, h, h / 2.0, HOVER_PILL);
            }
            // A muted module brightens under the pointer, so a hover says
            // "this responds" even where there is no pill.
            let color = if hovered && seg.module.clickable() && seg.color == self.colors.muted {
                self.colors.fg
            } else {
                seg.color
            };
            self.text.draw(&mut canvas, &seg.text, seg.x0, scale, color);
        }

        let surface = self.layer.wl_surface();
        surface.set_buffer_scale(self.scale);
        surface.damage_buffer(0, 0, pw as i32, ph as i32);
        if let Err(e) = buffer.attach_to(surface) {
            eprintln!("roostbar: attach: {e}");
        }
        self.layer.commit();
    }

    fn module_at(&self, x: f64) -> Option<Module> {
        let x = x as f32 * self.scale as f32;
        let slop = 6.0 * self.scale as f32;
        self.segments.iter().find(|s| x >= s.x0 - slop && x <= s.x1 + slop).map(|s| s.module)
    }

    fn click(&mut self, module: Module, button: u32) {
        // The bar is outside the panel too. The clock opens and closes it
        // itself, below.
        if module != Module::Clock {
            self.panel = None;
        }
        match (module, button) {
            (Module::Volume, BTN_LEFT) | (Module::Volume, BTN_MIDDLE) => self.audio.toggle_mute(),
            (Module::Wifi, BTN_LEFT) => self.open_settings("network"),
            (Module::Bluetooth, BTN_LEFT) => self.open_settings("bluetooth"),
            (Module::Bluetooth, BTN_MIDDLE) => {
                // Nothing paired and no MAC configured: the bar has no list
                // to offer, but the Bluetooth page does.
                if !self.bt.primary_action(self.cfg.bluetooth_device.clone()) {
                    self.open_settings("bluetooth");
                }
            }
            (Module::Bluetooth, BTN_RIGHT) => self.bt.toggle_power(),
            (Module::Date, BTN_LEFT) => self.open_settings("datetime"),
            (Module::Clock, BTN_LEFT) => self.toggle_panel(),
            (Module::Battery, BTN_LEFT) => {
                if !launch("raven-power", &[]) {
                    self.open_quick_settings();
                }
            }
            _ => {}
        }
        // Actions are async on both backends; poll soon so the bar catches up.
        self.last_slow_poll = Instant::now() - Duration::from_secs(60);
        self.refresh_fast();
        self.dirty = true;
        self.draw();
    }

    /// Raven Settings on one page; Huginn's quick settings if Settings is
    /// not installed.
    fn open_settings(&self, page: &str) {
        if !launch("raven-settings", &["--page", page]) {
            self.open_quick_settings();
        }
    }

    fn open_quick_settings(&self) {
        if let Some(shell) = &self.shell {
            shell.open_quick_settings();
        }
    }

    /// Height, logical, the clock panel needs for what it lists now.
    fn panel_height(&self) -> u32 {
        PanelMetrics::new(self.cfg.font_size).height(self.notes.len())
    }

    /// Open the clock panel, or close it if it is open.
    fn toggle_panel(&mut self) {
        // Dropping the layer surface destroys it.
        if self.panel.take().is_some() {
            return;
        }
        let height = self.panel_height();
        let pools = SlotPool::new((PANEL_WIDTH * height * 4) as usize, &self.shm)
            .and_then(|pool| Ok((pool, SlotPool::new(4096, &self.shm)?)));
        let (pool, catcher_pool) = match pools {
            Ok(pools) => pools,
            Err(e) => {
                eprintln!("roostbar: panel pool: {e}");
                return;
            }
        };

        // The catcher first, and the panel a layer above it, so the panel
        // is never under the surface that closes it.
        let surface = self.compositor.create_surface(&self.qh);
        let catcher = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Top, Some("roostbar-panel-outside"), None);
        catcher.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        catcher.set_size(0, 0);
        catcher.set_exclusive_zone(0);
        catcher.set_keyboard_interactivity(KeyboardInteractivity::None);
        catcher.commit();

        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Overlay, Some("roostbar-panel"), None);
        // Under the clock: the bar's corner, on whichever edge it is on.
        // Exclusive zone 0 keeps the panel out of the bar's own zone.
        let edge = if self.cfg.position == "bottom" { Anchor::BOTTOM } else { Anchor::TOP };
        layer.set_anchor(edge | Anchor::RIGHT);
        layer.set_margin(PANEL_MARGIN, PANEL_MARGIN, PANEL_MARGIN, PANEL_MARGIN);
        layer.set_size(PANEL_WIDTH, height);
        layer.set_exclusive_zone(0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        self.panel = Some(Panel {
            layer,
            pool,
            catcher,
            catcher_pool,
            requested: height,
            width: 0,
            height: 0,
            scale: self.scale,
            configured: false,
            dirty: true,
            pointer: None,
            rows: Vec::new(),
            hits: Vec::new(),
            scroll: 0.0,
        });
        // What the thread last sent may predate a Huginn restart.
        self.centre.refresh();
    }

    /// The centre sent a new list.
    fn notes_changed(&mut self, notes: Vec<notifications::Entry>) {
        if notes == self.notes {
            return;
        }
        self.notes = notes;
        let max = PanelMetrics::new(self.cfg.font_size).max_scroll(self.notes.len());
        if let Some(panel) = &mut self.panel {
            panel.scroll = panel.scroll.min(max);
            panel.dirty = true;
        }
        self.draw_panel();
    }

    /// Resize the panel if what it lists needs a different height, else draw
    /// it if anything changed. A resize is drawn when the compositor answers.
    fn draw_panel(&mut self) {
        let want = self.panel_height();
        let Some(mut panel) = self.panel.take() else { return };
        if panel.requested != want {
            panel.requested = want;
            panel.layer.set_size(PANEL_WIDTH, want);
            panel.layer.commit();
        } else if panel.configured && panel.dirty && panel.width != 0 {
            panel.dirty = false;
            self.paint_panel(&mut panel);
        }
        self.panel = Some(panel);
    }

    fn paint_panel(&self, panel: &mut Panel) {
        let s = panel.scale as f32;
        let pw = panel.width * panel.scale as u32;
        let ph = panel.height * panel.scale as u32;
        let (buffer, buf) = match panel.pool.create_buffer(pw as i32, ph as i32, pw as i32 * 4, wl_shm::Format::Argb8888) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("roostbar: panel buffer: {e}");
                return;
            }
        };
        let c = &self.colors;
        let m = PanelMetrics::new(self.cfg.font_size);
        let pointer = panel.pointer.map(|(x, y)| (x as f32 * s, y as f32 * s));
        let over = |r: [f32; 4]| pointer.is_some_and(|(x, y)| x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3]);

        let mut canvas = Canvas::new(buf, pw, ph);
        canvas.fill([0, 0, 0, 0]);
        canvas.fill_rounded(0.0, 0.0, pw as f32, ph as f32, PANEL_RADIUS * s, c.bg);

        let base = self.text.with_scale(s);
        let big = self.text.with_scale(s * 2.4);
        let small = self.text.with_scale(s * 0.88);
        let pad = PANEL_PAD * s;
        let right = pw as f32 - pad;
        let mut rows = Vec::new();
        let mut hits = Vec::new();

        // The time, and the date in words.
        let mut y = PANEL_TOP * s;
        self.text.draw_line(&mut canvas, &self.clock, pad, y, m.clock * s, big, c.fg);
        y += m.clock * s;
        self.text.draw_line(&mut canvas, &self.long_date, pad, y, m.line * s, base, c.muted);
        y += (m.line + PANEL_GAP) * s;
        canvas.fill_rect(pad as i32, y as i32, (right - pad) as i32, s as i32, HAIRLINE);
        y += (1.0 + PANEL_GAP_SMALL) * s;

        // "Notifications", and "Clear all" when there is anything to clear.
        self.text.draw_line(&mut canvas, "Notifications", pad, y, m.header * s, base, c.fg);
        if !self.notes.is_empty() {
            let label = "Clear all";
            let w = self.text.width(label, small);
            let inset = 8.0 * s;
            let rect = [right - w - inset, y + 3.0 * s, w + inset * 2.0, (m.header - 6.0) * s];
            let hovered = over(rect);
            if hovered {
                canvas.fill_rounded(rect[0], rect[1], rect[2], rect[3], rect[3] / 2.0, HOVER_PILL);
            }
            self.text.draw_line(&mut canvas, label, right - w, y, m.header * s, small, if hovered { c.fg } else { c.muted });
            hits.push((rect, PanelHit::Clear));
        }
        y += (m.header + PANEL_GAP_SMALL) * s;

        let list_top = y;
        let list_bottom = list_top + m.list_height(self.notes.len()) * s;
        if self.notes.is_empty() {
            let w = self.text.width(EMPTY_TEXT, base);
            self.text.draw_line(&mut canvas, EMPTY_TEXT, (pw as f32 - w) / 2.0, list_top, list_bottom - list_top, base, c.muted);
        } else {
            canvas.clip = Some((list_top as i32, list_bottom.ceil() as i32));
            let now = chrono::Local::now().timestamp();
            let close = 24.0 * s;
            let text_x = pad + 14.0 * s;
            let text_w = right - text_x - close - 10.0 * s;
            for (i, note) in self.notes.iter().enumerate() {
                let top = list_top + (i as f32 * (m.row + ROW_GAP) - panel.scroll) * s;
                let h = m.row * s;
                if top + h <= list_top || top >= list_bottom {
                    continue;
                }
                let shown_top = top.max(list_top);
                let shown = [pad, shown_top, right - pad, (top + h).min(list_bottom) - shown_top];
                let row_hovered = over(shown);
                canvas.fill_rounded(pad, top, right - pad, h, ROW_RADIUS * s, if row_hovered { ROW_HOVER } else { ROW_FILL });
                rows.push(shown);

                let mut ly = top + ROW_PAD * s;
                if note.open {
                    // Still open in Huginn, not yet only history.
                    let d = 6.0 * s;
                    canvas.fill_rounded(pad + 5.0 * s, ly + (m.small * s - d) / 2.0, d, d, d / 2.0, c.accent);
                }
                let when = ago(note.arrived, now);
                let head = if note.app_name.is_empty() { when } else { format!("{} · {when}", note.app_name) };
                self.text.draw_line(&mut canvas, &self.text.ellipsize(&head, text_w, small), text_x, ly, m.small * s, small, c.muted);
                ly += m.small * s;
                let summary = self.text.ellipsize(&flatten(&note.summary), text_w, base);
                self.text.draw_line(&mut canvas, &summary, text_x, ly, m.line * s, base, c.fg);
                ly += m.line * s;
                let body = flatten(&note.body);
                if !body.is_empty() {
                    let body = self.text.ellipsize(&body, text_w, small);
                    self.text.draw_line(&mut canvas, &body, text_x, ly, m.small * s, small, c.muted);
                }

                // The remove control, top right of the row.
                let rect = [right - close - 6.0 * s, top + (ROW_PAD - 2.0) * s, close, close];
                let clickable = rect[1] >= list_top && rect[1] + rect[3] <= list_bottom;
                let hovered = clickable && over(rect);
                if hovered {
                    canvas.fill_rounded(rect[0], rect[1], rect[2], rect[3], close / 2.0, HOVER_PILL);
                }
                let glyph = "󰅖";
                let gw = self.text.width(glyph, base);
                let color = if hovered { c.fg } else { c.muted };
                self.text.draw_line(&mut canvas, glyph, rect[0] + (close - gw) / 2.0, rect[1], close, base, color);
                if clickable {
                    hits.push((rect, PanelHit::Remove(note.id)));
                }
            }
        }
        panel.rows = rows;
        panel.hits = hits;

        let surface = panel.layer.wl_surface();
        surface.set_buffer_scale(panel.scale);
        surface.damage_buffer(0, 0, pw as i32, ph as i32);
        if let Err(e) = buffer.attach_to(surface) {
            eprintln!("roostbar: panel attach: {e}");
        }
        panel.layer.commit();
    }

    /// A pointer event on the clock panel.
    fn panel_pointer(&mut self, ev: &PointerEvent) {
        let m = PanelMetrics::new(self.cfg.font_size);
        let Some(panel) = &mut self.panel else { return };
        let before = panel.hover();
        match ev.kind {
            PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => panel.pointer = Some(ev.position),
            PointerEventKind::Leave { .. } => panel.pointer = None,
            PointerEventKind::Press { button: BTN_LEFT, .. } => {
                // Gone from the panel at once; the centre's next list agrees.
                match before.1.map(|i| panel.hits[i].1) {
                    Some(PanelHit::Remove(id)) => {
                        self.centre.remove(id);
                        self.notes.retain(|n| n.id != id);
                    }
                    Some(PanelHit::Clear) => {
                        self.centre.clear();
                        self.notes.clear();
                    }
                    None => {}
                }
                panel.scroll = panel.scroll.min(m.max_scroll(self.notes.len()));
                panel.dirty = true;
            }
            PointerEventKind::Axis { vertical, .. } => {
                let delta = if vertical.value120 != 0 {
                    vertical.value120 as f32 / 120.0 * SCROLL_NOTCH
                } else if vertical.discrete != 0 {
                    vertical.discrete as f32 * SCROLL_NOTCH
                } else {
                    vertical.absolute as f32
                };
                let next = (panel.scroll + delta).clamp(0.0, m.max_scroll(self.notes.len()));
                if next != panel.scroll {
                    panel.scroll = next;
                    panel.dirty = true;
                }
            }
            _ => {}
        }
        if panel.hover() != before {
            panel.dirty = true;
        }
        self.draw_panel();
    }

    fn scroll(&mut self, module: Module, notches: i64) {
        if module == Module::Volume && notches != 0 {
            self.audio.adjust(-notches * self.cfg.volume_step);
            self.refresh_fast();
            self.dirty = true;
            self.draw();
        }
    }
}

impl CompositorHandler for Bar {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &wl_surface::WlSurface, new_factor: i32) {
        // Nothing to see on the catcher, so its buffer stays at scale 1.
        if self.panel.as_ref().is_some_and(|p| p.catcher.wl_surface() == surface) {
            return;
        }
        if let Some(panel) = self.panel.as_mut().filter(|p| p.layer.wl_surface() == surface) {
            if panel.scale != new_factor {
                panel.scale = new_factor;
                panel.dirty = true;
                self.draw_panel();
            }
            return;
        }
        if new_factor != self.scale {
            self.scale = new_factor;
            self.dirty = true;
            self.draw();
        }
    }
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for Bar {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for Bar {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        let s = layer.wl_surface();
        if self.panel.as_ref().is_some_and(|p| p.layer.wl_surface() == s || p.catcher.wl_surface() == s) {
            self.panel = None;
            return;
        }
        self.exit = true;
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface, configure: LayerSurfaceConfigure, _: u32) {
        if let Some(panel) = self.panel.as_mut().filter(|p| p.catcher.wl_surface() == layer.wl_surface()) {
            // Fully transparent, but a buffer the size of the screen: a
            // surface only takes input where it has one.
            let (w, h) = configure.new_size;
            let (w, h) = (w.max(1) as i32, h.max(1) as i32);
            match panel.catcher_pool.create_buffer(w, h, w * 4, wl_shm::Format::Argb8888) {
                Ok((buffer, buf)) => {
                    buf.fill(0);
                    let surface = panel.catcher.wl_surface();
                    surface.set_buffer_scale(1);
                    surface.damage_buffer(0, 0, w, h);
                    if let Err(e) = buffer.attach_to(surface) {
                        eprintln!("roostbar: catcher attach: {e}");
                    }
                    panel.catcher.commit();
                }
                Err(e) => eprintln!("roostbar: catcher buffer: {e}"),
            }
            return;
        }
        if let Some(panel) = self.panel.as_mut().filter(|p| p.layer.wl_surface() == layer.wl_surface()) {
            let (w, h) = configure.new_size;
            panel.width = if w == 0 { PANEL_WIDTH } else { w };
            panel.height = if h == 0 { panel.requested } else { h };
            panel.configured = true;
            panel.dirty = true;
            self.draw_panel();
            return;
        }
        let (w, h) = configure.new_size;
        self.width = if w == 0 { 1920 } else { w };
        if h != 0 {
            self.height = h;
        }
        if self.debug {
            eprintln!("roostbar: configured {}x{} scale {}", self.width, self.height, self.scale);
        }
        self.configured = true;
        self.dirty = true;
        self.draw();
    }
}

impl SeatHandler for Bar {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Pointer {
            if let Some(p) = self.pointer.take() {
                p.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for Bar {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for ev in events {
            if self.panel.as_ref().is_some_and(|p| ev.surface == *p.layer.wl_surface()) {
                self.panel_pointer(ev);
                continue;
            }
            if self.panel.as_ref().is_some_and(|p| ev.surface == *p.catcher.wl_surface()) {
                // A press outside the panel closes it, and goes no further.
                if matches!(ev.kind, PointerEventKind::Press { .. }) {
                    self.panel = None;
                }
                continue;
            }
            if ev.surface != *self.layer.wl_surface() {
                continue;
            }
            match ev.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.pointer_x = ev.position.0;
                    let h = self.module_at(ev.position.0);
                    if h != self.hover {
                        self.hover = h;
                        self.dirty = true;
                        self.draw();
                    }
                }
                PointerEventKind::Leave { .. } => {
                    if self.hover.is_some() {
                        self.hover = None;
                        self.dirty = true;
                        self.draw();
                    }
                }
                PointerEventKind::Press { button, .. } => {
                    if let Some(m) = self.module_at(self.pointer_x) {
                        self.click(m, button);
                    }
                }
                PointerEventKind::Release { .. } => {}
                PointerEventKind::Axis { vertical, .. } => {
                    let Some(m) = self.module_at(self.pointer_x) else { continue };
                    let notches: i64 = if vertical.value120 != 0 {
                        self.scroll_acc += vertical.value120 as f64;
                        let n = (self.scroll_acc / 120.0).trunc();
                        self.scroll_acc -= n * 120.0;
                        n as i64
                    } else if vertical.discrete != 0 {
                        vertical.discrete as i64
                    } else {
                        // Smooth scrolling: ~15 logical px per notch.
                        self.scroll_acc += vertical.absolute;
                        let n = (self.scroll_acc / 15.0).trunc();
                        self.scroll_acc -= n * 15.0;
                        n as i64
                    };
                    self.scroll(m, notches);
                }
            }
        }
    }
}

impl ShmHandler for Bar {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

/// The manager has no events, so there is nothing to handle; the impl exists
/// because binding a global needs a Dispatch target.
impl Dispatch<RavenShellManagerV1, ()> for Bar {
    fn event(
        _: &mut Self,
        _: &RavenShellManagerV1,
        _: raven_shell::raven_shell_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl ProvidesRegistryState for Bar {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(Bar);
delegate_dispatch2!(Bar);
