//! PipeWire volume control through PipeWire's own tools: `pw-dump -m`
//! streams every object (and every change) as JSON, and `pw-cli set-param`
//! writes Props. No libpipewire bindings at build time, so it builds anywhere
//! the `pipewire` package is installed.
//!
//! State shape mirrors the native backend so the bar can't tell them apart.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::Value;

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
    pub sinks: HashMap<u64, Sink>,
}

impl State {
    fn default_id(&self) -> Option<u64> {
        self.sinks
            .iter()
            .find(|(_, s)| !self.default_sink.is_empty() && s.name == self.default_sink)
            .map(|(id, _)| *id)
            .or_else(|| self.sinks.keys().min().copied())
    }
}

pub struct PwClient {
    state: Arc<Mutex<State>>,
    pub wake_rx: UnixStream,
    child: Mutex<Option<Child>>,
}

impl Drop for PwClient {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.lock().ok().and_then(|mut c| c.take()) {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn to_percent(lin: f32) -> i64 {
    (lin.max(0.0).cbrt() * 100.0).round() as i64
}
fn to_linear(pct: i64) -> f32 {
    let p = pct.clamp(0, 100) as f32 / 100.0;
    p * p * p
}

fn apply(state: &Mutex<State>, obj: &Value) -> bool {
    let Some(id) = obj.get("id").and_then(Value::as_u64) else { return false };
    let info = obj.get("info");
    let mut st = state.lock().unwrap();
    // `"info": null` is pw-dump's removal notice.
    if info.map(Value::is_null).unwrap_or(true) {
        return st.sinks.remove(&id).is_some();
    }
    let info = info.unwrap();
    let ty = obj.get("type").and_then(Value::as_str).unwrap_or("");
    let props = info.get("props");
    let pget = |k: &str| props.and_then(|p| p.get(k)).and_then(Value::as_str).unwrap_or("").to_string();
    match ty {
        "PipeWire:Interface:Node" => {
            let is_sink = st.sinks.contains_key(&id) || pget("media.class") == "Audio/Sink";
            if !is_sink {
                return false;
            }
            let sink = st.sinks.entry(id).or_default();
            if props.is_some() {
                sink.name = pget("node.name");
                let d = pget("node.description");
                sink.description = if d.is_empty() { pget("node.nick") } else { d };
            }
            if let Some(p) = info.get("params").and_then(|p| p.get("Props")).and_then(Value::as_array) {
                for entry in p {
                    if let Some(v) = entry.get("channelVolumes").and_then(Value::as_array) {
                        sink.volumes = v.iter().filter_map(Value::as_f64).map(|f| f as f32).collect();
                    } else if let Some(v) = entry.get("volume").and_then(Value::as_f64) {
                        if sink.volumes.is_empty() {
                            sink.volumes = vec![v as f32];
                        }
                    }
                    if let Some(m) = entry.get("mute").and_then(Value::as_bool) {
                        sink.muted = m;
                    }
                }
            }
            true
        }
        "PipeWire:Interface:Metadata" => {
            if pget("metadata.name") != "default" {
                return false;
            }
            let mut changed = false;
            if let Some(items) = obj.get("metadata").and_then(Value::as_array) {
                for it in items {
                    if it.get("key").and_then(Value::as_str) == Some("default.audio.sink") {
                        let name = it
                            .get("value")
                            .and_then(|v| v.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        if name != st.default_sink {
                            st.default_sink = name;
                            changed = true;
                        }
                    }
                }
            }
            changed
        }
        _ => false,
    }
}

impl PwClient {
    pub fn start() -> Option<Self> {
        let rt = std::env::var("XDG_RUNTIME_DIR").ok()?;
        if !std::path::Path::new(&rt).join("pipewire-0").exists() {
            return None;
        }
        let mut child = Command::new("pw-dump")
            .arg("-m")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdout = child.stdout.take()?;
        let (mut wake_tx, wake_rx) = UnixStream::pair().ok()?;
        wake_rx.set_nonblocking(true).ok()?;
        let state = Arc::new(Mutex::new(State { connected: true, ..Default::default() }));
        let st = state.clone();
        std::thread::Builder::new()
            .name("pw-dump".into())
            .spawn(move || {
                let reader = std::io::BufReader::new(stdout);
                let stream = serde_json::Deserializer::from_reader(reader).into_iter::<Value>();
                for item in stream {
                    let Ok(v) = item else { break };
                    let objs: Vec<&Value> = match &v {
                        Value::Array(a) => a.iter().collect(),
                        other => vec![other],
                    };
                    let mut changed = false;
                    for o in objs {
                        changed |= apply(&st, o);
                    }
                    if changed {
                        let _ = wake_tx.write(&[1]);
                    }
                }
                st.lock().unwrap().connected = false;
                let _ = wake_tx.write(&[1]);
            })
            .ok()?;
        // Give the initial dump a moment so the first frame shows real numbers.
        std::thread::sleep(std::time::Duration::from_millis(150));
        Some(Self { state, wake_rx, child: Mutex::new(Some(child)) })
    }

    pub fn has_sinks(&self) -> bool {
        self.state.lock().map(|s| !s.sinks.is_empty()).unwrap_or(false)
    }

    pub fn alive(&self) -> bool {
        self.state.lock().map(|s| s.connected).unwrap_or(false)
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

    fn set(&self, id: u64, volumes: &[f32], mute: bool) {
        let vols = volumes.iter().map(|v| format!("{v:.6}")).collect::<Vec<_>>().join(", ");
        let param = format!("{{ channelVolumes: [ {vols} ], mute: {mute} }}");
        let _ = Command::new("pw-cli")
            .args(["set-param", &id.to_string(), "Props", &param])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|mut c| std::thread::spawn(move || c.wait()));
    }

    pub fn adjust(&self, delta: i64) {
        let (id, sink) = {
            let st = self.state.lock().unwrap();
            let Some(id) = st.default_id() else { return };
            let Some(s) = st.sinks.get(&id) else { return };
            (id, s.clone())
        };
        let n = sink.volumes.len().max(1);
        let cur = sink.volumes.iter().cloned().fold(0.0f32, f32::max);
        let lin = to_linear(to_percent(cur) + delta);
        let mute = if delta > 0 { false } else { sink.muted };
        // Optimistic local update so the bar reacts before pw-dump echoes it.
        {
            let mut st = self.state.lock().unwrap();
            if let Some(s) = st.sinks.get_mut(&id) {
                s.volumes = vec![lin; n];
                s.muted = mute;
            }
        }
        self.set(id, &vec![lin; n], mute);
    }

    pub fn toggle_mute(&self) {
        let (id, sink) = {
            let st = self.state.lock().unwrap();
            let Some(id) = st.default_id() else { return };
            let Some(s) = st.sinks.get(&id) else { return };
            (id, s.clone())
        };
        let vols = if sink.volumes.is_empty() { vec![1.0] } else { sink.volumes.clone() };
        let mute = !sink.muted;
        {
            let mut st = self.state.lock().unwrap();
            if let Some(s) = st.sinks.get_mut(&id) {
                s.muted = mute;
            }
        }
        self.set(id, &vols, mute);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dump_and_updates() {
        let state = Mutex::new(State { connected: true, ..Default::default() });
        let dump: Value = serde_json::from_str(r#"[
          { "id": 40, "type": "PipeWire:Interface:Node", "info": { "props": {
              "media.class": "Audio/Sink", "node.name": "alsa_output.pci-0000_05_00.6.analog-stereo",
              "node.description": "Family 17h HD Audio Controller Speaker" },
              "params": { "Props": [ { "volume": 1.0, "mute": false, "channelVolumes": [ 0.125, 0.125 ] } ] } } },
          { "id": 41, "type": "PipeWire:Interface:Node", "info": { "props": { "media.class": "Audio/Source", "node.name": "mic" } } },
          { "id": 3, "type": "PipeWire:Interface:Metadata", "info": { "props": { "metadata.name": "default" } },
            "metadata": [ { "subject": 0, "key": "default.audio.sink", "type": "Spa:String:JSON",
                            "value": { "name": "alsa_output.pci-0000_05_00.6.analog-stereo" } } ] }
        ]"#).unwrap();
        for o in dump.as_array().unwrap() {
            apply(&state, o);
        }
        {
            let st = state.lock().unwrap();
            assert_eq!(st.sinks.len(), 1);
            assert_eq!(st.default_id(), Some(40));
            assert_eq!(st.default_sink, "alsa_output.pci-0000_05_00.6.analog-stereo");
            let s = &st.sinks[&40];
            assert_eq!(s.description, "Family 17h HD Audio Controller Speaker");
            assert_eq!(to_percent(s.volumes[0]), 50);
            assert!(!s.muted);
        }
        // A param-only update (no props) must keep the name and change mute.
        let upd: Value = serde_json::from_str(r#"{ "id": 40, "type": "PipeWire:Interface:Node",
            "info": { "params": { "Props": [ { "mute": true, "channelVolumes": [ 1.0, 1.0 ] } ] } } }"#).unwrap();
        assert!(apply(&state, &upd));
        {
            let st = state.lock().unwrap();
            let s = &st.sinks[&40];
            assert!(s.muted);
            assert_eq!(s.name, "alsa_output.pci-0000_05_00.6.analog-stereo");
            assert_eq!(to_percent(s.volumes[0]), 100);
        }
        // Removal.
        let rm: Value = serde_json::from_str(r#"{ "id": 40, "info": null }"#).unwrap();
        assert!(apply(&state, &rm));
        assert!(state.lock().unwrap().sinks.is_empty());
    }

    #[test]
    fn cubic_mapping_round_trips() {
        for p in [0, 5, 25, 50, 75, 100] {
            assert_eq!(to_percent(to_linear(p)), p);
        }
    }
}
