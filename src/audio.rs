use alsa::mixer::{Mixer, SelemChannelId, SelemId};

pub struct Audio {
    mixer: Mixer,
    sid: SelemId,
    pub card: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Volume {
    pub percent: i64,
    pub muted: bool,
}

impl Audio {
    /// Open the configured card, or scan hw:0..hw:7 for the first card that
    /// has the mixer element -- preferring one that also has a Speaker or
    /// Headphone element, which is the laptop codec rather than HDMI.
    pub fn open(card: &str, element: &str) -> Option<Self> {
        if !card.is_empty() {
            return Self::try_open(card, element);
        }
        let mut fallback = None;
        for i in 0..8 {
            let name = format!("hw:{i}");
            if let Some(a) = Self::try_open(&name, element) {
                let analog = ["Speaker", "Headphone"]
                    .iter()
                    .any(|e| a.mixer.find_selem(&SelemId::new(e, 0)).is_some());
                if analog {
                    return Some(a);
                }
                fallback.get_or_insert(a);
            }
        }
        fallback
    }

    fn try_open(card: &str, element: &str) -> Option<Self> {
        let mixer = Mixer::new(card, false).ok()?;
        let sid = SelemId::new(element, 0);
        let selem = mixer.find_selem(&sid)?;
        if !selem.has_playback_volume() {
            return None;
        }
        drop(selem);
        Some(Self { mixer, sid, card: card.to_string() })
    }

    pub fn get(&self) -> Option<Volume> {
        let _ = self.mixer.handle_events();
        let s = self.mixer.find_selem(&self.sid)?;
        let (lo, hi) = s.get_playback_volume_range();
        let v = s.get_playback_volume(SelemChannelId::FrontLeft).ok()?;
        let percent = if hi > lo { ((v - lo) * 100 + (hi - lo) / 2) / (hi - lo) } else { 0 };
        let muted = s.has_playback_switch()
            && s.get_playback_switch(SelemChannelId::FrontLeft).map(|x| x == 0).unwrap_or(false);
        Some(Volume { percent, muted })
    }

    pub fn adjust(&self, delta_percent: i64) {
        let Some(s) = self.mixer.find_selem(&self.sid) else { return };
        let (lo, hi) = s.get_playback_volume_range();
        let cur = s.get_playback_volume(SelemChannelId::FrontLeft).unwrap_or(lo);
        let cur_pct = if hi > lo { ((cur - lo) * 100 + (hi - lo) / 2) / (hi - lo) } else { 0 };
        let new_pct = (cur_pct + delta_percent).clamp(0, 100);
        let new = lo + ((hi - lo) * new_pct + 50) / 100;
        let _ = s.set_playback_volume_all(new);
        // Turning the volume up while muted unmutes -- that is what people mean.
        if delta_percent > 0 && s.has_playback_switch() {
            let _ = s.set_playback_switch_all(1);
        }
    }

    pub fn toggle_mute(&self) {
        let Some(s) = self.mixer.find_selem(&self.sid) else { return };
        if !s.has_playback_switch() {
            return;
        }
        let on = s.get_playback_switch(SelemChannelId::FrontLeft).unwrap_or(1);
        let _ = s.set_playback_switch_all(if on == 0 { 1 } else { 0 });
    }
}
