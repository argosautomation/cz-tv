// cz-tv audio: which sink the sound goes to, and how loud it is.
//
// The card has two possible audio outputs and they are NOT the same device:
// the onboard ES8389 (a 1 W speaker, PipeWire sink `alsa_output.platform-sound.*`)
// and the vc4 HDMI, whose sink only EXISTS while a TV is actually plugged in.
// Making the *picture* follow the cable is not enough - the sound has to follow
// it too, or a movie on the TV comes out of the card's own tiny speaker.
//
// Routing is done with the `PULSE_SINK` environment variable on the player
// process rather than a player flag, because it is honoured by both ffmpeg's
// pulse muxer and VLC's pulse aout (verified on the hardware by routing each
// into a temporary null sink), so a hand-edited `player`/`player_local` in
// config.json keeps working untouched.
//
// Everything here shells out to `pactl` for the same reason the API shells out
// to `curl`: nothing extra gets linked into a static musl binary. It is all
// best effort - if pactl is missing or the sink has gone, we leave the audio
// wherever the server put it, which is exactly what the app did before.

use std::process::{Command, Stdio};

fn pactl(args: &[&str]) -> Option<String> {
    let out = Command::new("pactl")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The sink that belongs to a picture target. `None` means "no opinion, let the
/// server pick" - which is what happens when asked for the TV with nothing
/// plugged in, because there is no vc4hdmi sink until a cable is there. Naming
/// a sink that does not exist would be harmless anyway (pipewire-pulse quietly
/// falls back to the default) but there is no reason to.
pub fn sink_for(tv: bool) -> Option<String> {
    let txt = pactl(&["list", "sinks", "short"])?;
    txt.lines()
        .filter_map(|l| l.split('\t').nth(1))
        .find(|n| n.to_lowercase().contains("hdmi") == tv)
        .map(str::to_string)
}

#[derive(Clone, Copy)]
pub struct Vol {
    pub pct: u32,
    pub muted: bool,
}

impl Vol {
    /// Short form for the menu footer, where there are only a few columns to
    /// spare: `V75`, or `VMUTE`.
    pub fn short(&self) -> String {
        if self.muted {
            "VMUTE".to_string()
        } else {
            format!("V{}", self.pct)
        }
    }

    /// The NOW PLAYING line, which has room to name the keys. They are the
    /// keypad's own Fn layer, handled by the desktop, so they also work from
    /// the launcher and from every other app on the card.
    pub fn line(&self) -> String {
        if self.muted {
            "MUTED - Fn+A unmutes".to_string()
        } else {
            format!("VOL {}   Fn+S quieter  Fn+D louder", self.pct)
        }
    }
}

// `pactl get-sink-volume` prints
//   Volume: front-left: 16384 /  25% / -36.12 dB,   front-right: ...
// and the percentage is the same scale `wpctl` and every desktop mixer show.
// It is NOT linear in amplitude: 25% is -36 dB, which on a 1 W speaker is
// silence with the lights on. That is exactly how this app shipped mute.
fn first_pct(s: &str) -> Option<u32> {
    let b = s.as_bytes();
    for i in 0..b.len() {
        if b[i] != b'%' {
            continue;
        }
        let mut j = i;
        while j > 0 && b[j - 1].is_ascii_digit() {
            j -= 1;
        }
        if j < i {
            return std::str::from_utf8(&b[j..i]).ok()?.parse().ok();
        }
    }
    None
}

pub fn get(sink: &str) -> Option<Vol> {
    let pct = first_pct(&pactl(&["get-sink-volume", sink])?)?;
    let muted = pactl(&["get-sink-mute", sink])
        .map(|s| s.to_lowercase().contains("yes"))
        .unwrap_or(false);
    Some(Vol { pct, muted })
}

fn set_pct(sink: &str, pct: u32) {
    let _ = pactl(&["set-sink-volume", sink, &format!("{}%", pct.min(MAX))]);
}

fn set_mute(sink: &str, on: bool) {
    let _ = pactl(&["set-sink-mute", sink, if on { "1" } else { "0" }]);
}

const MAX: u32 = 100;
// Below this the speaker is not quiet, it is off: the scale is cubic, so 40%
// is already -24 dB. Anything under it on a movie reads as "the app is broken".
const FLOOR: u32 = 40;
const RESCUE: u32 = 70;

/// Called once when a title starts: a movie must never begin inaudible. Unmutes
/// and lifts a floor-scraping volume, but leaves any level he could actually
/// hear exactly where he set it.
pub fn ensure_audible(sink: &str) -> Option<Vol> {
    let mut v = get(sink)?;
    if v.muted {
        set_mute(sink, false);
        v.muted = false;
    }
    if v.pct < FLOOR {
        set_pct(sink, RESCUE);
        v.pct = RESCUE;
    }
    Some(v)
}

// The volume KEYS are deliberately not handled here. labwc already binds them
// session-wide in /etc/xdg/labwc/rc.xml (XF86AudioRaiseVolume / LowerVolume /
// Mute -> `wfpanelctl volumepulse`), and that binding fires while this app is
// in the foreground because the app reads /dev/input directly and is not a
// Wayland client at all. Handling them here as well made the app and the
// compositor each read-modify-write the same sink: measured on the hardware,
// seven presses of Fn+S took the volume from 100% to 15% instead of 30%, and
// two presses of Fn+A left it muted because the toggles raced. So the keys
// belong to the desktop, and this app's job is to SHOW the level - which
// nothing else on the card does - and to never start a title inaudible.
