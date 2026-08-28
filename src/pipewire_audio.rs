//! Native PipeWire volume control. A dedicated thread runs a PipeWire main
//! loop, tracks every Audio/Sink node and the `default.audio.sink` metadata,
//! and mirrors the default sink's Props (channelVolumes + mute) into shared
//! state. Commands come in over a pipewire channel so set_param runs on the
//! loop thread. A self-pipe wakes the bar the moment something changes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use pipewire as pw;
use pw::spa;
use spa::param::ParamType;
use spa::pod::deserialize::PodDeserializer;
use spa::pod::serialize::PodSerializer;
use spa::pod::{Object, Pod, Property, PropertyFlags, Value, ValueArray};
use spa::utils::SpaTypes;

use crate::audio::Volume;

#[derive(Clone, Debug, Default)]
pub struct Sink {
    pub name: String,
    pub description: String,
    pub volumes: Vec<f32>,
    pub muted: bool,
}

#[derive(Default)]
pub struct State {
    pub connected: bool,
    pub default_sink: String,
    pub sinks: HashMap<u32, Sink>,
}

impl State {
    fn default_id(&self) -> Option<u32> {
        self.sinks
            .iter()
            .find(|(_, s)| !self.default_sink.is_empty() && s.name == self.default_sink)
            .map(|(id, _)| *id)
            .or_else(|| self.sinks.keys().min().copied())
    }
}

enum Cmd {
    Adjust(i64),
    ToggleMute,
}

pub struct PwClient {
    state: Arc<Mutex<State>>,
    tx: pw::channel::Sender<Cmd>,
    pub wake_rx: UnixStream,
}

/// PipeWire volumes are linear; every mixer in the world shows the cube root.
fn to_percent(lin: f32) -> i64 {
    (lin.max(0.0).cbrt() * 100.0).round() as i64
}
fn to_linear(pct: i64) -> f32 {
    let p = pct.clamp(0, 100) as f32 / 100.0;
    p * p * p
}

impl PwClient {
    pub fn start() -> Option<Self> {
        // Don't even try if there's no daemon socket -- a connect attempt
        // could otherwise autospawn nothing and just log noise.
        let rt = std::env::var("XDG_RUNTIME_DIR").ok()?;
        if !std::path::Path::new(&rt).join("pipewire-0").exists() {
            return None;
        }
        let (wake_tx, wake_rx) = UnixStream::pair().ok()?;
        wake_rx.set_nonblocking(true).ok()?;
        let state = Arc::new(Mutex::new(State::default()));
        let (tx, rx) = pw::channel::channel::<Cmd>();
        let st = state.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<bool>();
        std::thread::Builder::new()
            .name("pipewire".into())
            .spawn(move || run_loop(st, rx, wake_tx, ready_tx))
            .ok()?;
        match ready_rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(true) => Some(Self { state, tx, wake_rx }),
            _ => None,
        }
    }

    pub fn get(&self) -> Option<Volume> {
        let st = self.state.lock().ok()?;
        if !st.connected {
            return None;
        }
        let id = st.default_id()?;
        let s = st.sinks.get(&id)?;
        let lin = s.volumes.iter().cloned().fold(0.0f32, f32::max);
        Some(Volume { percent: to_percent(lin), muted: s.muted })
    }

    pub fn sink_description(&self) -> Option<String> {
        let st = self.state.lock().ok()?;
        let id = st.default_id()?;
        st.sinks.get(&id).map(|s| if s.description.is_empty() { s.name.clone() } else { s.description.clone() })
    }

    pub fn adjust(&self, delta: i64) {
        let _ = self.tx.send(Cmd::Adjust(delta));
    }
    pub fn toggle_mute(&self) {
        let _ = self.tx.send(Cmd::ToggleMute);
    }
}

struct Tracked {
    _node: pw::node::Node,
    _listener: pw::node::NodeListener,
}

fn parse_props(pod: &Pod) -> Option<(Vec<f32>, Option<bool>)> {
    let (_, value) = PodDeserializer::deserialize_from::<Value>(pod.as_bytes()).ok()?;
    let Value::Object(obj) = value else { return None };
    let mut vols = Vec::new();
    let mut mute = None;
    for p in obj.properties {
        if p.key == spa::sys::SPA_PROP_channelVolumes {
            if let Value::ValueArray(ValueArray::Float(v)) = p.value {
                vols = v;
            }
        } else if p.key == spa::sys::SPA_PROP_mute {
            if let Value::Bool(b) = p.value {
                mute = Some(b);
            }
        }
    }
    Some((vols, mute))
}

fn props_pod(volumes: Vec<f32>, mute: bool) -> Option<Vec<u8>> {
    let value = Value::Object(Object {
        type_: SpaTypes::ObjectParamProps.as_raw(),
        id: ParamType::Props.as_raw(),
        properties: vec![
            Property {
                key: spa::sys::SPA_PROP_channelVolumes,
                flags: PropertyFlags::empty(),
                value: Value::ValueArray(ValueArray::Float(volumes)),
            },
            Property { key: spa::sys::SPA_PROP_mute, flags: PropertyFlags::empty(), value: Value::Bool(mute) },
        ],
    });
    let (cursor, _) = PodSerializer::serialize(Cursor::new(Vec::new()), &value).ok()?;
    Some(cursor.into_inner())
}

/// `{ "name": "alsa_output.pci-…" }` -> the name. Tiny hand parser; the value
/// is one flat JSON object and pulling in serde_json for it is silly.
fn json_name(v: &str) -> Option<String> {
    let i = v.find("\"name\"")?;
    let rest = &v[i + 6..];
    let q1 = rest.find('"')?;
    let rest = &rest[q1 + 1..];
    let q2 = rest.find('"')?;
    Some(rest[..q2].to_string())
}

fn run_loop(
    state: Arc<Mutex<State>>,
    rx: pw::channel::Receiver<Cmd>,
    wake: UnixStream,
    ready: std::sync::mpsc::Sender<bool>,
) {
    pw::init();
    let Ok(mainloop) = pw::main_loop::MainLoop::new(None) else { let _ = ready.send(false); return };
    let Ok(context) = pw::context::Context::new(&mainloop) else { let _ = ready.send(false); return };
    let Ok(core) = context.connect(None) else { let _ = ready.send(false); return };
    let Ok(registry) = core.get_registry() else { let _ = ready.send(false); return };
    let registry = Rc::new(registry);
    let wake = Rc::new(RefCell::new(wake));
    let nodes: Rc<RefCell<HashMap<u32, Tracked>>> = Rc::new(RefCell::new(HashMap::new()));
    let metadata: Rc<RefCell<Option<(pw::metadata::Metadata, pw::metadata::MetadataListener)>>> =
        Rc::new(RefCell::new(None));

    let poke = {
        let wake = wake.clone();
        move || {
            let _ = wake.borrow_mut().write(&[1]);
        }
    };

    {
        let mut st = state.lock().unwrap();
        st.connected = true;
    }

    // Commands from the bar.
    let _rx = {
        let state = state.clone();
        let nodes = nodes.clone();
        rx.attach(mainloop.loop_(), move |cmd| {
            let (id, sink) = {
                let st = state.lock().unwrap();
                let Some(id) = st.default_id() else { return };
                let Some(s) = st.sinks.get(&id) else { return };
                (id, s.clone())
            };
            let nodes = nodes.borrow();
            let Some(t) = nodes.get(&id) else { return };
            let n = sink.volumes.len().max(1);
            let (vols, mute) = match cmd {
                Cmd::Adjust(d) => {
                    let cur = sink.volumes.iter().cloned().fold(0.0f32, f32::max);
                    let lin = to_linear(to_percent(cur) + d);
                    (vec![lin; n], if d > 0 { false } else { sink.muted })
                }
                Cmd::ToggleMute => (if sink.volumes.is_empty() { vec![1.0; n] } else { sink.volumes.clone() }, !sink.muted),
            };
            if let Some(bytes) = props_pod(vols, mute) {
                if let Some(pod) = Pod::from_bytes(&bytes) {
                    t._node.set_param(ParamType::Props, 0, pod);
                }
            }
        })
    };

    let _reg_listener = {
        let registry_weak = Rc::downgrade(&registry);
        let state_g = state.clone();
        let state_r = state.clone();
        let nodes_g = nodes.clone();
        let nodes_r = nodes.clone();
        let metadata = metadata.clone();
        let poke_g = poke.clone();
        let poke_r = poke.clone();
        registry
            .add_listener_local()
            .global(move |global| {
                let Some(registry) = registry_weak.upgrade() else { return };
                let Some(props) = global.props else { return };
                match global.type_ {
                    pw::types::ObjectType::Node => {
                        if props.get("media.class") != Some("Audio/Sink") {
                            return;
                        }
                        let Ok(node) = registry.bind::<pw::node::Node, _>(global) else { return };
                        let id = global.id;
                        {
                            let mut st = state_g.lock().unwrap();
                            st.sinks.insert(
                                id,
                                Sink {
                                    name: props.get("node.name").unwrap_or("").to_string(),
                                    description: props
                                        .get("node.description")
                                        .or(props.get("node.nick"))
                                        .unwrap_or("")
                                        .to_string(),
                                    ..Default::default()
                                },
                            );
                        }
                        let st = state_g.clone();
                        let poke = poke_g.clone();
                        let listener = node
                            .add_listener_local()
                            .param(move |_seq, param_id, _index, _next, param| {
                                if param_id != ParamType::Props {
                                    return;
                                }
                                let Some(pod) = param else { return };
                                let Some((vols, mute)) = parse_props(pod) else { return };
                                let mut st = st.lock().unwrap();
                                if let Some(s) = st.sinks.get_mut(&id) {
                                    if !vols.is_empty() {
                                        s.volumes = vols;
                                    }
                                    if let Some(m) = mute {
                                        s.muted = m;
                                    }
                                }
                                drop(st);
                                poke();
                            })
                            .register();
                        node.subscribe_params(&[ParamType::Props]);
                        nodes_g.borrow_mut().insert(id, Tracked { _node: node, _listener: listener });
                        poke_g();
                    }
                    pw::types::ObjectType::Metadata => {
                        if props.get("metadata.name") != Some("default") {
                            return;
                        }
                        let Ok(md) = registry.bind::<pw::metadata::Metadata, _>(global) else { return };
                        let st = state_g.clone();
                        let poke = poke_g.clone();
                        let listener = md
                            .add_listener_local()
                            .property(move |_subject, key, _type, value| {
                                if key == Some("default.audio.sink") {
                                    let name = value.and_then(json_name).unwrap_or_default();
                                    st.lock().unwrap().default_sink = name;
                                    poke();
                                }
                                0
                            })
                            .register();
                        *metadata.borrow_mut() = Some((md, listener));
                    }
                    _ => {}
                }
            })
            .global_remove(move |id| {
                if nodes_r.borrow_mut().remove(&id).is_some() {
                    state_r.lock().unwrap().sinks.remove(&id);
                    poke_r();
                }
            })
            .register()
    };

    let _core_listener = {
        let state = state.clone();
        let ml = mainloop.clone();
        let poke = poke.clone();
        core.add_listener_local()
            .error(move |_id, _seq, _res, message| {
                if message.contains("connection error") {
                    state.lock().unwrap().connected = false;
                    poke();
                    ml.quit();
                }
            })
            .register()
    };

    let _ = ready.send(true);
    mainloop.run();
    state.lock().unwrap().connected = false;
    poke();
}
