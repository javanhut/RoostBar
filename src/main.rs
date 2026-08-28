mod audio;
mod bluetooth;
mod config;
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
use std::time::{Duration, Instant};

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
    /// version that has `open_quick_settings`. None on an older Huginn, in
    /// which case a click on the battery does nothing.
    shell: Option<RavenShellManagerV1>,
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
    clock: String,
    date: String,
    last_slow_poll: Instant,
}

struct Colors {
    bg: [u8; 4],
    fg: [u8; 4],
    accent: [u8; 4],
    muted: [u8; 4],
    warning: [u8; 4],
    charging: [u8; 4],
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
        eprintln!("roostbar: compositor has no raven_shell_manager_v1 v2; battery click will not open quick settings");
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
    let text = match Text::load(&cfg.font, cfg.font_size) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("roostbar: font: {e}");
            std::process::exit(1);
        }
    };
    let colors = Colors {
        bg: parse_color(&cfg.background),
        fg: parse_color(&cfg.foreground),
        accent: parse_color(&cfg.accent),
        muted: parse_color(&cfg.muted),
        warning: parse_color(&cfg.warning),
        charging: parse_color(if cfg.charging.trim().is_empty() { &cfg.accent } else { &cfg.charging }),
    };

    let audio = AudioBackend::select(&cfg);
    match &audio {
        AudioBackend::Alsa(a) => eprintln!("roostbar: audio via ALSA ({}, {})", a.card, cfg.alsa_mixer),
        other => eprintln!("roostbar: audio via {}", other.name()),
    }
    let debug = std::env::var_os("ROOSTBAR_DEBUG").is_some();

    let mut bar = Bar {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        layer,
        shell,
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
        clock: String::new(),
        date: String::new(),
        last_slow_poll: Instant::now() - Duration::from_secs(60),
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
    }
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
                // SAFETY: Generic hands back the same UnixStream we inserted.
                while let Ok(n) = unsafe { stream.get_mut() }.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                }
                bar.refresh_fast();
                bar.draw();
                Ok(PostAction::Continue)
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

    fn refresh_slow(&mut self) {
        self.last_slow_poll = Instant::now();
        self.reselect_audio();
        let battery = system::battery(&self.cfg.battery);
        let wifi = system::wifi(&self.cfg.wifi_interface);
        let bt = self.bt.state();
        if battery != self.battery || wifi != self.wifi || bt != self.bt_state {
            self.battery = battery;
            self.wifi = wifi;
            self.bt_state = bt;
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
            right.push((Module::Battery, format!("{icon} {}%", b.percent), col));
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
        let mut canvas = Canvas { buf: canvas_buf, width: pw, height: ph };
        canvas.fill(self.colors.bg);

        let scale = self.text.with_scale(self.scale as f32);
        let hover_pad = 6.0 * self.scale as f32;
        for seg in &self.segments {
            if self.hover == Some(seg.module) && matches!(seg.module, Module::Volume | Module::Bluetooth) {
                let mut hl = self.colors.fg;
                hl[0] = 24;
                canvas.fill_rect((seg.x0 - hover_pad) as i32, 0, (seg.x1 - seg.x0 + 2.0 * hover_pad) as i32, ph as i32, hl);
            }
            self.text.draw(&mut canvas, &seg.text, seg.x0, scale, seg.color);
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
        match (module, button) {
            (Module::Volume, BTN_LEFT) | (Module::Volume, BTN_MIDDLE) => self.audio.toggle_mute(),
            (Module::Bluetooth, BTN_LEFT) => self.bt.primary_action(self.cfg.bluetooth_device.clone()),
            (Module::Bluetooth, BTN_MIDDLE) | (Module::Bluetooth, BTN_RIGHT) => self.bt.toggle_power(),
            (Module::Battery, BTN_LEFT) => {
                if let Some(shell) = &self.shell {
                    shell.open_quick_settings();
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
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, new_factor: i32) {
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
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface, configure: LayerSurfaceConfigure, _: u32) {
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
