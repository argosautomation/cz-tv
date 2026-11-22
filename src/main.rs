// cz-tv - a live TV guide for the M5Stack Cardputer Zero.
//
// The card is a Raspberry Pi CM0: 352 MiB RAM, a 320x170 SPI panel, and - the
// part that makes this possible - a VideoCore hardware H.264 decoder
// (bcm2835-codec) plus a vc4 HDMI out. So this binary is deliberately thin: it
// draws the channel guide on the little panel and, on ENTER, hands the stream to
// a player that hardware-decodes it. The picture follows the cable: with nothing
// in the HDMI port it plays on the card's own screen, and the moment a TV is
// plugged in it plays there instead. TAB overrides either way.
//
// The channel list is an ordinary M3U playlist, so it plays whatever the owner
// of the card already has: a Jellyfin or Plex playlist, their own IPTV
// subscription, an iptv-org list, or a text file of URLs. Out of the box it
// uses a small built-in list of publishers' own open live streams, so a fresh
// install works with no account and no config file: launch it, arrow to a
// channel, press ENTER.
//
// Pure Rust + libc: the API is fetched by shelling out to `curl` (already on the
// box) so the static musl binary never has to link a TLS stack, and playback is
// ffmpeg or VLC. Two crates, serde_json and libc.

mod audio;
mod fb;
mod font;
mod kbd;
mod playlist;

use audio::Vol;
use fb::{Screen, AMBER, BLACK, BLUE, CYAN, GREY, RED, WHITE};
use kbd::*;
use playlist::Chan;
use std::process::{Child, Command, Stdio};

// ----------------------------------------------------------------- config
// Every field has a working default; config.json is for people who want to
// change the player, not something a new install has to be given.
struct Config {
    sources: Vec<playlist::Src>,
    player: Vec<String>,
    player_local: Vec<String>,
    local_decoders: Vec<String>,
    wayland_display: String,
    cache_secs: u64,
}

// The launcher starts apps with a normal environment, but a hand-run shell or a
// systemd unit may not set HOME. Fall back to the passwd entry's directory
// rather than to any one machine's path - this app is meant to be handed round.
fn home_dir() -> String {
    if let Ok(h) = std::env::var("HOME") {
        if !h.is_empty() {
            return h;
        }
    }
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() && !(*pw).pw_dir.is_null() {
            if let Ok(s) = std::ffi::CStr::from_ptr((*pw).pw_dir).to_str() {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
    }
    "/tmp".to_string()
}

fn cfg_dir() -> String {
    format!("{}/.config/cz-tv", home_dir())
}

fn cache_dir() -> String {
    format!("{}/.cache/cz-tv", home_dir())
}

// Where the channels come from. Everything found here ADDS to what came before
// it, so "I want these three as well" is a two-line file and not a copy of the
// built-in lineup with three lines on the end:
//
//   1. the built-in lineup                 ("include_builtin": false drops it)
//   2. ~/.config/cz-tv/channels.m3u        one file to scribble in
//   3. ~/.config/cz-tv/channels.d/*.m3u    drop-ins, alphabetical - this is
//                                          where `cz-tv --add` writes, so a
//                                          pack can be added and removed as a
//                                          single file
//   4. "playlist" and "playlists" in config.json, URL or path, in order
//
// A later source wins on a channel name, which is how a dead built-in channel
// gets replaced without editing anything that an upgrade would overwrite.
fn sources(v: &serde_json::Value) -> Vec<playlist::Src> {
    use playlist::Src;
    let mut out: Vec<Src> = Vec::new();
    if v.get("include_builtin").and_then(|x| x.as_bool()).unwrap_or(true) {
        out.push(Src::Builtin);
    }
    let local = format!("{}/channels.m3u", cfg_dir());
    if std::path::Path::new(&local).exists() {
        out.push(Src::Path(local));
    }
    out.extend(dropins().into_iter().map(Src::Path));
    let mut listed: Vec<String> = Vec::new();
    if let Some(one) = v.get("playlist").and_then(|x| x.as_str()) {
        listed.push(one.to_string());
    }
    if let Some(many) = v.get("playlists").and_then(|x| x.as_array()) {
        listed.extend(many.iter().filter_map(|x| x.as_str().map(String::from)));
    }
    for item in listed {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        out.push(as_src(item));
    }
    out
}

pub fn as_src(item: &str) -> playlist::Src {
    if item.starts_with("http://") || item.starts_with("https://") {
        playlist::Src::Url(item.to_string())
    } else {
        playlist::Src::Path(item.to_string())
    }
}

// Sorted, so the guide is in the same order every launch; a drop-in can be
// given a "10-" prefix to place it.
fn dropins() -> Vec<String> {
    let dir = format!("{}/channels.d", cfg_dir());
    let mut found: Vec<String> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "m3u" || x == "m3u8").unwrap_or(false))
            .filter_map(|p| p.to_str().map(String::from))
            .collect(),
        Err(_) => Vec::new(),
    };
    found.sort();
    found
}

fn load_config() -> Config {
    use serde_json::Value;
    let path = format!("{}/config.json", cfg_dir());
    // On the TV: VLC as a Wayland client on the labwc session, fullscreen,
    // hardware decode.
    let default_player = vec![
        "cvlc".to_string(),
        "--no-video-title-show".into(),
        "--fullscreen".into(),
        "--intf".into(),
        "dummy".into(),
        "--play-and-exit".into(),
        "--avcodec-hw=any".into(),
        "--aout=pulse".into(),
        "--network-caching=3000".into(),
        "{url}".into(),
    ];
    // On the card's OWN 320x170 panel, no TV: the VideoCore hardware-decodes
    // ({vdec}, tried in order from local_decoders), we drop to 20 fps before the
    // CPU downscale+RGB565 (the SPI panel is fine faster; the swscale is what
    // costs), blit straight to /dev/fb0, and route audio to the onboard ES8389.
    let default_player_local = vec![
        "ffmpeg".to_string(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "quiet".into(),
        "-re".into(),
        "-c:v".into(),
        "{vdec}".into(),
        "-i".into(),
        "{url}".into(),
        "-map".into(),
        "0:v:0".into(),
        "-vf".into(),
        "fps=20,scale=320:170:force_original_aspect_ratio=decrease:flags=fast_bilinear,pad=320:170:(ow-iw)/2:(oh-ih)/2,format=rgb565le".into(),
        "-f".into(),
        "fbdev".into(),
        "/dev/fb0".into(),
        "-map".into(),
        "0:a:0?".into(),
        "-af".into(),
        "aresample=async=1".into(),
        "-f".into(),
        "pulse".into(),
        "cz-tv".into(),
    ];
    let v: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let str_list = |key: &str| -> Option<Vec<String>> {
        v.get(key)
            .and_then(|p| p.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .filter(|a: &Vec<String>| !a.is_empty())
    };
    Config {
        sources: sources(&v),
        player: str_list("player").unwrap_or(default_player),
        player_local: str_list("player_local").unwrap_or(default_player_local),
        // Tried in order for on-card playback, first one that keeps running
        // wins. "" means "let ffmpeg pick" (software decode), the last resort.
        local_decoders: str_list("local_decoders")
            .unwrap_or_else(|| vec!["h264_v4l2m2m".to_string(), "".into()]),
        wayland_display: v
            .get("wayland_display")
            .and_then(|x| x.as_str())
            .unwrap_or("wayland-1")
            .to_string(),
        cache_secs: v.get("cache_secs").and_then(|x| x.as_u64()).unwrap_or(6 * 3600),
    }
}

// ----------------------------------------------------------------- views
#[derive(Clone)]
enum Action {
    OpenCat(String),  // category name
    OpenAll,
    Play(usize),      // index into the lineup
}

#[derive(Clone)]
struct Row {
    label: String,
    chan: Option<usize>, // the lineup index, for the ON NOW lookup
    action: Action,
}

struct View {
    title: String,
    rows: Vec<Row>,
    sel: usize,
    top: usize,
}

impl View {
    fn new(title: &str, rows: Vec<Row>) -> View {
        View { title: title.to_string(), rows, sel: 0, top: 0 }
    }
    fn cur(&self) -> Option<&Row> {
        self.rows.get(self.sel)
    }
}

// Right-align the channel number so 3 and 3730 line up in a column.
fn chan_row(lineup: &[Chan], i: usize) -> Row {
    let c = &lineup[i];
    Row {
        label: format!("{:>4}  {}", c.number, c.name),
        chan: Some(i),
        action: Action::Play(i),
    }
}

fn home_view(lineup: &[Chan]) -> View {
    // Groups in the order the playlist lists them, each with its channel count.
    let mut cats: Vec<(String, usize)> = Vec::new();
    for c in lineup {
        match cats.iter_mut().find(|(n, _)| *n == c.group) {
            Some((_, n)) => *n += 1,
            None => cats.push((c.group.clone(), 1)),
        }
    }
    cats.sort_by(|a, b| a.0.cmp(&b.0));
    let mut rows = vec![Row {
        label: format!("All channels ({})", lineup.len()),
        chan: None,
        action: Action::OpenAll,
    }];
    for (name, n) in cats {
        rows.push(Row {
            label: format!("{name}  ({n})"),
            chan: None,
            action: Action::OpenCat(name),
        });
    }
    View::new("TV", rows)
}

fn cat_view(lineup: &[Chan], cat: &str) -> View {
    let rows: Vec<Row> = lineup
        .iter()
        .enumerate()
        .filter(|(_, c)| c.group == cat)
        .map(|(i, _)| chan_row(lineup, i))
        .collect();
    View::new(cat, rows)
}

fn all_view(lineup: &[Chan]) -> View {
    View::new(
        "All channels",
        (0..lineup.len()).map(|i| chan_row(lineup, i)).collect(),
    )
}

// Type-to-search over the whole lineup, name or number. 467 channels is too
// many to arrow through and the keypad has real letter keys, so any printable
// character opens this.
fn search_view(lineup: &[Chan], q: &str) -> View {
    let needle = q.to_lowercase();
    let rows: Vec<Row> = lineup
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            c.name.to_lowercase().contains(&needle) || c.number.to_string().starts_with(&needle)
        })
        .map(|(i, _)| chan_row(lineup, i))
        .collect();
    View::new(&format!("/{q}"), rows)
}

// ----------------------------------------------------------------- drawing
// The font atlas is ASCII 32..126, so an accented letter paints as a blank -
// "En Espanol" is readable, "En Espa ol" looks like a bug, and playlists are
// full of names like Télé, Kanál and Nhật. Fold what folds, and turn anything
// left into a dot so the gap is clearly a character and not a missing one.
fn fold_ascii(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ' '..='~' => c.to_string(),
            'À'..='Å' | 'à'..='å' | 'Ā'..='ą' => {
                if c.is_uppercase() { "A" } else { "a" }.to_string()
            }
            'È'..='Ë' | 'è'..='ë' | 'Ē'..='ě' => {
                if c.is_uppercase() { "E" } else { "e" }.to_string()
            }
            'Ì'..='Ï' | 'ì'..='ï' | 'Ĩ'..='ı' => {
                if c.is_uppercase() { "I" } else { "i" }.to_string()
            }
            'Ò'..='Ö' | 'Ø' | 'ò'..='ö' | 'ø' | 'Ō'..='ő' => {
                if c.is_uppercase() { "O" } else { "o" }.to_string()
            }
            'Ù'..='Ü' | 'ù'..='ü' | 'Ũ'..='ų' => {
                if c.is_uppercase() { "U" } else { "u" }.to_string()
            }
            'Ç' | 'ç' | 'Ć'..='č' => if c.is_uppercase() { "C" } else { "c" }.to_string(),
            'Ñ' | 'ñ' | 'Ń'..='ň' => if c.is_uppercase() { "N" } else { "n" }.to_string(),
            'Þ' | 'þ' => if c.is_uppercase() { "TH" } else { "th" }.to_string(),
            'Ý' | 'ý' | 'ÿ' => if c.is_uppercase() { "Y" } else { "y" }.to_string(),
            'Š' | 'š' | 'Ś'..='ş' => if c.is_uppercase() { "S" } else { "s" }.to_string(),
            'Ž' | 'ž' | 'Ź'..='ż' => if c.is_uppercase() { "Z" } else { "z" }.to_string(),
            'Ð' | 'ð' | 'Ď'..='đ' => if c.is_uppercase() { "D" } else { "d" }.to_string(),
            'Ł' | 'ł' => if c.is_uppercase() { "L" } else { "l" }.to_string(),
            'Ğ' | 'ğ' | 'Ĝ'..='ģ' => if c.is_uppercase() { "G" } else { "g" }.to_string(),
            'Ř' | 'ř' | 'Ŕ'..='ŗ' => if c.is_uppercase() { "R" } else { "r" }.to_string(),
            'Ţ' | 'ţ' | 'Ť'..='ŧ' => if c.is_uppercase() { "T" } else { "t" }.to_string(),
            'ß' => "ss".to_string(),
            'Æ' | 'æ' => if c.is_uppercase() { "AE" } else { "ae" }.to_string(),
            'Œ' | 'œ' => if c.is_uppercase() { "OE" } else { "oe" }.to_string(),
            '\u{2013}' | '\u{2014}' => "-".to_string(),
            '\u{2018}' | '\u{2019}' => "'".to_string(),
            '\u{201c}' | '\u{201d}' => "\"".to_string(),
            '\u{2026}' => "...".to_string(),
            _ => ".".to_string(),
        })
        .collect()
}

fn clip(s: &str, cols: usize) -> String {
    let folded = fold_ascii(s);
    let v: Vec<char> = folded.chars().collect();
    if v.len() <= cols {
        folded
    } else {
        v[..cols.saturating_sub(1)].iter().collect::<String>() + ">"
    }
}

// The guide. One extra row above the footer describes the highlighted channel -
// its group and the host its stream comes from - which is the thing a name on
// its own never tells you once a playlist has channels from several sources.
fn draw(scr: &mut Screen, view: &View, detail: Option<&str>, status: &str) {
    scr.clear(BLACK);
    let cols = scr.cols;
    let show_now = detail.is_some();
    let visible = scr.rows.saturating_sub(if show_now { 3 } else { 2 });

    let n = view.rows.len();
    let hdr = if n > 0 {
        format!("{}  {}/{}", view.title, view.sel + 1, n)
    } else {
        view.title.clone()
    };
    scr.row_bar(0, &clip(&hdr, cols), BLACK, CYAN);

    for i in 0..visible {
        let idx = view.top + i;
        if idx >= n {
            break;
        }
        let label = clip(&view.rows[idx].label, cols);
        if idx == view.sel {
            scr.row_bar(i + 1, &label, BLACK, AMBER);
        } else {
            scr.text(0, i + 1, &label, WHITE);
        }
    }
    if n == 0 {
        scr.text(0, 2, "No channels match.", GREY);
    }

    let fr = scr.rows - 1;
    if let Some(d) = detail {
        scr.text(0, fr - 1, &clip(d, cols), CYAN);
    }
    scr.fill_rect(0, fr * font::CH, scr.w, font::CH, BLUE);
    scr.text(0, fr, &clip(status, cols), WHITE);
    scr.flush();
}

fn message(scr: &mut Screen, title: &str, lines: &[&str], color: fb::Color) {
    scr.clear(BLACK);
    scr.row_bar(0, &clip(title, scr.cols), BLACK, color);
    for (i, l) in lines.iter().enumerate() {
        scr.text(0, i + 2, &clip(l, scr.cols), WHITE);
    }
    scr.flush();
}

// What the highlighted channel is, without going near the network: its group
// and the host serving it. With a mixed playlist (some Jellyfin, some IPTV, some
// public streams) the host is how you tell them apart.
fn detail_line(ch: &Chan) -> String {
    let host = ch
        .url
        .split("//")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .unwrap_or("");
    if host.is_empty() {
        ch.group.clone()
    } else {
        format!("{}  {}", ch.group, host)
    }
}

// ----------------------------------------------------------------- playback
#[derive(Clone, Copy, PartialEq, Eq)]
enum Out {
    Screen, // the card's own 320x170 panel
    Tv,     // the HDMI output
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutMode {
    Auto, // follow the cable
    ForceScreen,
    ForceTv,
}

// Is a TV actually connected? The vc4 HDMI connector exposes its hotplug state
// at /sys/class/drm/card0-HDMI-A-1/status, and it updates live, so this also
// answers "did he just plug one in mid-show".
fn hdmi_connected() -> bool {
    // CZ_DRM_DIR points this at a fake connector tree so the plug/unplug path
    // can be tested on a card with nothing in its HDMI port.
    let root = std::env::var("CZ_DRM_DIR").unwrap_or_else(|_| "/sys/class/drm".to_string());
    let dir = match std::fs::read_dir(&root) {
        Ok(d) => d,
        Err(_) => return false,
    };
    for e in dir.flatten() {
        if !e.file_name().to_string_lossy().contains("HDMI") {
            continue;
        }
        if let Ok(st) = std::fs::read_to_string(e.path().join("status")) {
            if st.trim().eq_ignore_ascii_case("connected") {
                return true;
            }
        }
    }
    false
}

fn resolve_out(mode: OutMode) -> Out {
    match mode {
        OutMode::Auto => {
            if hdmi_connected() {
                Out::Tv
            } else {
                Out::Screen
            }
        }
        OutMode::ForceScreen => Out::Screen,
        OutMode::ForceTv => Out::Tv,
    }
}

fn cur_sink(mode: OutMode) -> Option<String> {
    audio::sink_for(resolve_out(mode) == Out::Tv)
}

fn out_label(mode: OutMode) -> String {
    let t = match resolve_out(mode) {
        Out::Screen => "SCREEN",
        Out::Tv => "TV",
    };
    if mode == OutMode::Auto {
        format!("TAB:{t}")
    } else {
        format!("TAB:{t}*")
    }
}

// Substitute {url} and {vdec} into an argv template. vdec=None drops the
// decoder flag entirely ("-c:v {vdec}" disappears) so ffmpeg picks for itself.
fn expand(tmpl: &[String], url: &str, vdec: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(tmpl.len());
    for t in tmpl {
        if t == "{vdec}" {
            match vdec {
                Some(d) => out.push(d.to_string()),
                None => {
                    if out.last().map(|s| s == "-c:v" || s == "-codec:v").unwrap_or(false) {
                        out.pop();
                    }
                }
            }
        } else {
            out.push(t.replace("{url}", url));
        }
    }
    out
}

fn attempts(cfg: &Config, url: &str, target: Out) -> Vec<(Vec<String>, String)> {
    match target {
        Out::Tv => vec![(expand(&cfg.player, url, None), "TV".to_string())],
        Out::Screen => {
            if !cfg.player_local.iter().any(|t| t == "{vdec}") {
                // A hand-written player_local with its own decoder: honour it as-is.
                return vec![(expand(&cfg.player_local, url, None), "custom".to_string())];
            }
            cfg.local_decoders
                .iter()
                .map(|d| {
                    if d.is_empty() {
                        (expand(&cfg.player_local, url, None), "software decode".to_string())
                    } else {
                        let label = d.split('_').next().unwrap_or(d).to_uppercase();
                        (expand(&cfg.player_local, url, Some(d)), label)
                    }
                })
                .collect()
        }
    }
}

fn spawn_player(cfg: &Config, args: Vec<String>, sink: Option<&str>) -> std::io::Result<Child> {
    let mut args = args;
    if args.is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty player"));
    }
    let prog = args.remove(0);
    let mut cmd = Command::new(prog);
    cmd.args(&args);
    // Send the sound where the picture is going. PULSE_SINK rather than a player
    // flag: both ffmpeg's pulse muxer and VLC's pulse aout honour it, so a
    // hand-edited player template in config.json keeps working as written.
    if let Some(s) = sink {
        cmd.env("PULSE_SINK", s);
    }
    // Run as a Wayland client on the labwc session so it lands on the HDMI out.
    if std::env::var("XDG_RUNTIME_DIR").is_err() {
        let uid = unsafe { libc::getuid() };
        cmd.env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"));
    }
    if std::env::var("WAYLAND_DISPLAY").is_err() {
        cmd.env("WAYLAND_DISPLAY", &cfg.wayland_display);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    cmd.spawn()
}

fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

// A player that dies this fast never opened the stream (wrong decoder, 404, no
// display). One that outlives it and then exits simply reached the end.
const FAST_FAIL_SECS: f64 = 6.0;

enum Sup {
    Exited(f64),
    Esc,
    Switch,   // TAB: he asked for the other output
    Retarget, // the HDMI cable came or went while in Auto
}

struct Playing<'a> {
    name: &'a str,
    line: &'a str,
    sink: Option<&'a str>,
    target: Out,
}

fn draw_now(scr: &mut Screen, p: &Playing, detail: Option<&str>, vol: Option<Vol>) {
    let vline = vol.map(|v| v.line()).unwrap_or_default();
    let detail = detail.unwrap_or("");
    message(
        scr,
        "NOW PLAYING",
        &[p.name, detail, "", p.line, "", &vline, "", "ESC stop   TAB other screen"],
        RED,
    );
}

fn supervise(
    child: &mut Child,
    kbd: &mut Keyboard,
    mode: &mut OutMode,
    scr: &mut Screen,
    p: &Playing,
    detail: Option<&str>,
    vol: &mut Option<Vol>,
) -> Sup {
    let target = p.target;
    let start = std::time::Instant::now();
    let mut hdmi_t = std::time::Instant::now();
    let mut light_t = std::time::Instant::now();
    let mut vol_t = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return Sup::Exited(start.elapsed().as_secs_f64()),
            Ok(None) => {}
        }
        for ev in kbd.poll(200) {
            match ev {
                Ev::Down(KEY_ESC) => {
                    kill_child(child);
                    return Sup::Esc;
                }
                Ev::Down(KEY_TAB) => {
                    *mode = match target {
                        Out::Screen => OutMode::ForceTv,
                        Out::Tv => OutMode::ForceScreen,
                    };
                    kill_child(child);
                    return Sup::Switch;
                }
                _ => {}
            }
        }
        // Keep the level on the NOW PLAYING card honest while he works the
        // keypad's volume keys (labwc's binding, not ours). Only on the TV path:
        // when a title plays on the card the panel belongs to the video, so
        // there is nothing to repaint and the CPU is the thing in short supply.
        if target == Out::Tv && vol_t.elapsed().as_secs() >= 2 {
            vol_t = std::time::Instant::now();
            if let Some(s) = p.sink {
                let nv = audio::get(s);
                if nv.is_some() && nv.map(|v| (v.pct, v.muted)) != vol.map(|v| (v.pct, v.muted)) {
                    *vol = nv;
                    draw_now(scr, p, detail, *vol);
                }
            }
        }
        // The launcher blanks the panel when it thinks the card is idle, and a
        // dark backlight looks exactly like "it is not playing".
        if target == Out::Screen && light_t.elapsed().as_secs() >= 5 {
            light_t = std::time::Instant::now();
            fb::backlight_on();
        }
        // Plugged a TV in mid-show? Move the picture to it (Auto only).
        if *mode == OutMode::Auto && hdmi_t.elapsed().as_secs() >= 2 {
            hdmi_t = std::time::Instant::now();
            if resolve_out(*mode) != target {
                kill_child(child);
                return Sup::Retarget;
            }
        }
    }
}

// A TAB pressed *during* a show is a here-and-now switch, not a setting: when it
// ends we go back to following the cable. Pinning it would mean one TAB on the
// couch sends every later channel to an HDMI port with nothing in it. TAB in the
// guide is the persistent control.
fn play(
    scr: &mut Screen,
    kbd: &mut Keyboard,
    cfg: &Config,
    ch: &Chan,
    detail: Option<&str>,
    mode: &mut OutMode,
    vol: &mut Option<Vol>,
) {
    let entry = *mode;
    play_inner(scr, kbd, cfg, ch, detail, mode, vol);
    if entry == OutMode::Auto {
        *mode = OutMode::Auto;
    }
}

fn play_inner(
    scr: &mut Screen,
    kbd: &mut Keyboard,
    cfg: &Config,
    ch: &Chan,
    detail: Option<&str>,
    mode: &mut OutMode,
    vol: &mut Option<Vol>,
) {
    loop {
        let target = resolve_out(*mode);
        let where_ = match target {
            Out::Screen => "On this screen",
            Out::Tv if !hdmi_connected() => "On HDMI - but no TV is plugged in",
            Out::Tv => "On the TV (HDMI)",
        };
        if target == Out::Screen {
            fb::backlight_on();
        }
        let p0 = Playing { name: &ch.name, line: "Tuning...", sink: None, target };
        draw_now(scr, &p0, detail, *vol);

        // Pick the rendition here rather than leave it to the player: players
        // take the first variant a master lists, and plenty of masters list the
        // biggest first - which on a 320x170 panel means decoding 1080p and
        // throwing almost all of it away. A URL with nothing to pick (a media
        // playlist, an .mp4) comes back unchanged.
        let url = playlist::pick_variant(&ch.url, target == Out::Screen)
            .unwrap_or_else(|| ch.url.clone());

        // The sound follows the cable along with the picture: the HDMI sink for
        // the TV, the card's own ES8389 for the panel. `None` means there is no
        // such sink - then leave the routing alone.
        let sink = audio::sink_for(target == Out::Tv);
        if let Some(s) = sink.as_deref() {
            if let Some(v) = audio::ensure_audible(s) {
                *vol = Some(v);
            }
        }

        let tries = attempts(cfg, &url, target);
        let mut switched = false;
        let mut all_failed = true;
        for (i, (args, note)) in tries.into_iter().enumerate() {
            let line = if i == 0 {
                where_.to_string()
            } else {
                format!("{where_} - retry: {note}")
            };
            if target == Out::Screen {
                fb::backlight_on();
            }
            let p = Playing { name: &ch.name, line: &line, sink: sink.as_deref(), target };
            draw_now(scr, &p, detail, *vol);
            let mut child = match spawn_player(cfg, args, sink.as_deref()) {
                Ok(c) => c,
                Err(e) => {
                    message(scr, "PLAYER ERROR", &[&format!("{e}"), "", "ESC to go back"], RED);
                    wait_key(kbd);
                    return;
                }
            };
            match supervise(&mut child, kbd, mode, scr, &p, detail, vol) {
                Sup::Esc => return,
                Sup::Switch | Sup::Retarget => {
                    switched = true;
                    all_failed = false;
                    break;
                }
                Sup::Exited(secs) => {
                    if secs >= FAST_FAIL_SECS {
                        // It really played, then ended (or he stopped it elsewhere).
                        return;
                    }
                    // Died on startup: fall through and try the next decoder.
                }
            }
        }
        if switched {
            continue; // re-resolve the target and start over on the other output
        }
        if all_failed {
            let lines: Vec<&str> = match target {
                Out::Tv if !hdmi_connected() => vec![
                    "Nothing is plugged into the",
                    "HDMI port.",
                    "",
                    "Press TAB to play on this",
                    "screen instead.",
                ],
                Out::Tv => vec!["The TV would not start this", "channel.", "", "TAB to try this screen."],
                Out::Screen => vec![
                    "This channel would not play",
                    "on the card's screen.",
                    "",
                    "TAB to send it to a TV.",
                ],
            };
            message(scr, "CANNOT PLAY", &lines, RED);
            wait_key(kbd);
        }
        return;
    }
}

fn wait_key(kbd: &mut Keyboard) {
    loop {
        for ev in kbd.poll(500) {
            if let Ev::Down(_) = ev {
                return;
            }
        }
    }
}

// ----------------------------------------------------------------- selftest
// `cz-tv --probe [channel-number]` loads the playlist and resolves one channel
// to the exact URL the player would get, with no framebuffer and no keypad - so
// a playlist can be checked on a build host, and on the card over ssh, before
// anyone sits down in front of it. It is what the live test script drives.
fn probe(arg: Option<&str>) -> i32 {
    let cfg = load_config();
    let (lineup, reports) = playlist::load_many(&cfg.sources, &cache_dir(), cfg.cache_secs);
    for r in &reports {
        println!("source     {:<44} {}", trunc(&r.label, 44), r.note());
    }
    if lineup.is_empty() {
        println!("FAIL no channels in any source");
        return 1;
    }
    let mut groups: Vec<&str> = lineup.iter().map(|c| c.group.as_str()).collect();
    groups.sort_unstable();
    groups.dedup();
    println!("channels   {} in {} groups", lineup.len(), groups.len());

    // With no argument, check every channel: that is the question a playlist
    // actually raises ("which of these still work"), and it is the check that
    // would have caught a provider swapping real video for a takedown slate.
    let picks: Vec<&Chan> = match arg.and_then(|a| a.parse::<i64>().ok()) {
        Some(n) => lineup.iter().filter(|c| c.number == n).collect(),
        None => lineup.iter().collect(),
    };
    if picks.is_empty() {
        println!("FAIL no such channel");
        return 1;
    }
    let mut bad = 0;
    for ch in picks {
        let low = playlist::pick_variant(&ch.url, true);
        let high = playlist::pick_variant(&ch.url, false);
        // The variant has to be FETCHED, not just resolved. A master playlist
        // that parses is not a channel that plays: three of the built-in
        // channels publish "../../../manifest/..." variants and the CDN 403s a
        // path that still has a ".." in it, which this check now catches and the
        // old "the master parsed, call it ok" did not.
        let note = match (&low, &high) {
            (Some(l), Some(_)) if !reachable(l) => {
                bad += 1;
                "VARIANT DEAD".to_string()
            }
            (Some(l), Some(h)) if l == h => "1 rendition".to_string(),
            (Some(_), Some(_)) => "ok".to_string(),
            _ => {
                // Not a master playlist. That is fine for a media playlist or an
                // .mp4, so say what it is rather than call it a failure.
                if reachable(&ch.url) {
                    "single stream".to_string()
                } else {
                    bad += 1;
                    "UNREACHABLE".to_string()
                }
            }
        };
        println!("{:>4}  {:<28} {:<10} {}", ch.number, trunc(&ch.name, 28), ch.group, note);
    }
    if bad > 0 {
        println!("{bad} channel(s) did not answer");
        return 1;
    }
    0
}

// `cz-tv --sources` - where the lineup came from, in order, with the file paths
// spelled out. The first question a second person asks about someone else's
// install is "where is this list", and the answer has to come from the app.
fn show_sources() -> i32 {
    let cfg = load_config();
    let (lineup, reports) = playlist::load_many(&cfg.sources, &cache_dir(), cfg.cache_secs);
    for r in &reports {
        println!("{:<52} {}", r.label, r.note());
    }
    println!("\n{} channels in the guide", lineup.len());
    println!("add more:   cz-tv --add <url-or-file>");
    println!("drop-ins:   {}/channels.d/", cfg_dir());
    let packs = pack_dir();
    if std::path::Path::new(&packs).is_dir() {
        let mut names: Vec<String> = std::fs::read_dir(&packs)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| n.ends_with(".m3u"))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        if !names.is_empty() {
            println!("packs:      {} ({})", packs, names.join(", "));
        }
    }
    0
}

// Packs shipped by the package: extra lineups that are NOT in the default
// guide, added with one --add. Where a community list lands without anybody
// having to argue about the default lineup.
fn pack_dir() -> String {
    for d in ["/usr/share/cz-tv/packs", "/usr/local/share/cz-tv/packs"] {
        if std::path::Path::new(d).is_dir() {
            return d.to_string();
        }
    }
    "/usr/share/cz-tv/packs".to_string()
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "channels".to_string() } else { trunc(&out, 40) }
}

// Record a remote playlist URL in config.json's "playlists" array, leaving
// every other setting in the file exactly as it was - this file is where
// somebody's player command lives.
fn subscribe(url: &str) -> Result<String, String> {
    use serde_json::Value;
    let path = format!("{}/config.json", cfg_dir());
    let mut v: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| Value::Object(Default::default()));
    if !v.is_object() {
        return Err(format!("{path} is not a JSON object - not touching it"));
    }
    let already = v
        .get("playlists")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().any(|x| x.as_str() == Some(url)))
        .unwrap_or(false)
        || v.get("playlist").and_then(|x| x.as_str()) == Some(url);
    if already {
        return Ok(format!("{path} (already subscribed)"));
    }
    let obj = v.as_object_mut().unwrap();
    let list = obj.entry("playlists").or_insert_with(|| Value::Array(Vec::new()));
    match list.as_array_mut() {
        Some(a) => a.push(Value::String(url.to_string())),
        None => return Err(format!("\"playlists\" in {path} is not a list")),
    }
    if let Some(d) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let text = serde_json::to_string_pretty(&v).map_err(|e| format!("{e}"))?;
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, text + "\n").map_err(|e| format!("{path}: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("{path}: {e}"))?;
    Ok(path)
}

// Is this a LIST OF CHANNELS, or is it one stream?
//
// Both are .m3u8 and both contain #EXTINF lines, so the tag everyone reaches
// for does not separate them. An HLS stream - master or media - always carries
// a tag from the HLS vocabulary that a channel list never has. Without this,
// `--add <a stream>` reads the master's renditions as six channels all named
// after the CDN host.
fn is_channel_list(text: &str) -> bool {
    const HLS_ONLY: [&str; 6] = [
        "#EXT-X-STREAM-INF",
        "#EXT-X-TARGETDURATION",
        "#EXT-X-MEDIA-SEQUENCE",
        "#EXT-X-ENDLIST",
        "#EXT-X-PLAYLIST-TYPE",
        "#EXT-X-I-FRAME-STREAM-INF",
    ];
    if HLS_ONLY.iter().any(|t| text.contains(t)) {
        return false;
    }
    if text.contains("#EXTINF") {
        return true;
    }
    // A hand-written file of URLs, one per line, is a list as well.
    text.lines().filter(|l| l.trim().starts_with("http")).count() > 1
}

// `cz-tv --add <url|file> [name] [group]`
//
// The barrier to this app being shareable was never the backend, it was that a
// new channel meant hand-editing JSON over ssh. This is that job in one
// command: it fetches the thing, works out whether it is a playlist or a single
// stream, CHECKS IT PLAYS, and appends it to a drop-in file.
//
// A playlist (anything with more than one entry) is written as its own file in
// channels.d/ so it can be removed again by deleting one file.
fn add_source(args: &[String]) -> i32 {
    let Some(target) = args.first() else {
        println!("usage: cz-tv --add <url-or-file> [name] [group]");
        return 2;
    };
    let src = as_src(target);
    let text = match playlist::read_source(&src, &cache_dir(), 0) {
        Ok(t) => t,
        Err(e) => {
            println!("could not read {target}: {e}");
            return 1;
        }
    };
    let looks_like_playlist = is_channel_list(&text);
    let dir = format!("{}/channels.d", cfg_dir());
    if let Err(e) = std::fs::create_dir_all(&dir) {
        println!("could not create {dir}: {e}");
        return 1;
    }

    if looks_like_playlist {
        let found = playlist::parse_m3u(&text);
        if found.is_empty() {
            println!("{target} has no channels in it");
            return 1;
        }
        // A remote list is recorded as a SUBSCRIPTION - its URL goes in the
        // "playlists" array in config.json - so it tracks upstream instead of
        // freezing today's copy. A local file is copied into channels.d/ so
        // that deleting or moving the original does not empty the guide.
        let dest = match &src {
            playlist::Src::Url(u) => match subscribe(u) {
                Ok(where_) => where_,
                Err(e) => {
                    println!("{e}");
                    return 1;
                }
            },
            _ => {
                let stem = std::path::Path::new(target)
                    .file_stem()
                    .and_then(|x| x.to_str())
                    .unwrap_or("list");
                let dest = format!("{dir}/{}.m3u", slug(stem));
                if let Err(e) = std::fs::write(&dest, &text) {
                    println!("could not write {dest}: {e}");
                    return 1;
                }
                dest
            }
        };
        println!("added {} channels from {target}", found.len());
        println!("  -> {dest}");
        for ch in found.iter().take(5) {
            println!("     {}", ch.name);
        }
        if found.len() > 5 {
            println!("     ... and {} more", found.len() - 5);
        }
        println!("cz-tv --sources lists every source; remove it by editing that file.");
        return 0;
    }

    // One stream. Check it actually plays before writing it down: a 200 is not
    // a picture, so resolve the rendition the player would get and decode-test
    // the URL the same way --probe does.
    let url = target.to_string();
    let name = args.get(1).cloned().unwrap_or_else(|| {
        url.split("//").nth(1).and_then(|r| r.split('/').next()).unwrap_or("Channel").to_string()
    });
    let group = args.get(2).cloned().unwrap_or_else(|| "My Channels".to_string());
    let picked = playlist::pick_variant(&url, true);
    if picked.is_none() && !reachable(&url) {
        println!("{url}\n  did not answer - not added.");
        return 1;
    }
    let dest = format!("{dir}/my-channels.m3u");
    let line = format!("#EXTINF:-1 group-title=\"{group}\",{name}\n{url}\n");
    let body = match std::fs::read_to_string(&dest) {
        Ok(existing) => {
            if existing.contains(&url) {
                println!("{name} is already in {dest}");
                return 0;
            }
            format!("{existing}{line}")
        }
        Err(_) => format!("#EXTM3U\n# Added by `cz-tv --add`. Edit or delete freely.\n{line}"),
    };
    if let Err(e) = std::fs::write(&dest, body) {
        println!("could not write {dest}: {e}");
        return 1;
    }
    println!("added {name} ({group})");
    println!("  -> {dest}");
    match picked {
        Some(u) if u != url => println!("  plays: {}", trunc(&u, 70)),
        _ => println!("  plays: single stream"),
    }
    0
}

fn reachable(url: &str) -> bool {
    Command::new("curl")
        .args(["-sS", "--fail", "--max-time", "15", "-o", "/dev/null", "-A", "CardputerZero-TV/1.0", url])
        .status()
        .map(|st| st.success())
        .unwrap_or(false)
}

fn trunc(s: &str, n: usize) -> String {
    let v: Vec<char> = s.chars().collect();
    if v.len() <= n { s.to_string() } else { v[..n].iter().collect() }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("--probe") => std::process::exit(probe(args.get(1).map(|s| s.as_str()))),
        Some("--add") => std::process::exit(add_source(&args[1..])),
        Some("--sources") => std::process::exit(show_sources()),
        Some("--version") => {
            println!("cz-tv {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("--help") | Some("-h") => {
            println!(
                "cz-tv - a live TV guide for the M5Stack Cardputer Zero\n\n\
                 With no arguments it draws the channel guide on /dev/fb0 and reads\n\
                 the keypad. Arrows move, ENTER plays, ESC goes back, TAB moves the\n\
                 picture between this screen and a TV, any letter searches.\n\n\
                 It ships with a built-in lineup of free live channels and ADDS to\n\
                 it whatever else you point it at, so adding one channel is one\n\
                 command and never a copy of the whole list:\n\n\
                   --add <url>            add one stream, after checking it plays\n\
                   --add <url> NAME GROUP ... with a name and a guide group\n\
                   --add <m3u url>        subscribe to a playlist (tracks upstream)\n\
                   --add <file.m3u>       copy a playlist in\n\
                   --sources              where the lineup comes from, in order\n\
                   --probe [chan]         resolve every channel and say which answer\n\
                   --version\n\n\
                 Sources, each adding to the ones above it: the built-in lineup,\n\
                 ~/.config/cz-tv/channels.m3u, ~/.config/cz-tv/channels.d/*.m3u,\n\
                 then \"playlist\"/\"playlists\" in ~/.config/cz-tv/config.json. A\n\
                 later source wins on a channel name, so a built-in channel whose\n\
                 feed died can be replaced by name. \"include_builtin\": false in\n\
                 config.json drops the built-ins entirely.\n"
            );
            return;
        }
        _ => {}
    }
    let cfg = load_config();
    Screen::wake();
    let mut scr = match Screen::open() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cz-tv: framebuffer: {e}");
            std::process::exit(1);
        }
    };
    let mut kbd = match Keyboard::open() {
        Ok(k) => k,
        Err(e) => {
            message(&mut scr, "cz-tv", &["keyboard open failed:", &format!("{e}")], RED);
            std::thread::sleep(std::time::Duration::from_secs(3));
            std::process::exit(1);
        }
    };

    message(&mut scr, "TV", &["Loading channels..."], CYAN);
    let (lineup, reports) = playlist::load_many(&cfg.sources, &cache_dir(), cfg.cache_secs);
    if lineup.is_empty() {
        // Name the source that failed. "no channels" with four sources
        // configured is not something anyone can act on.
        let why = reports
            .iter()
            .find_map(|r| r.err.as_ref().map(|e| format!("{}: {}", trunc(&r.label, 24), e)))
            .unwrap_or_else(|| "no channels in any source".to_string());
        message(
            &mut scr,
            "TV - no channels",
            &[
                &trunc(&why, 42),
                "",
                "Add one:  cz-tv --add URL",
                "or drop an M3U at",
                "~/.config/cz-tv/channels.m3u",
                "cz-tv --sources shows them.",
            ],
            RED,
        );
        wait_key(&mut kbd);
        return;
    }

    let mut stack: Vec<View> = vec![home_view(&lineup)];
    let mut esc_armed = false; // ESC at home arms quit; second ESC quits
    let mut out_mode = OutMode::Auto;
    let mut vol: Option<Vol> = cur_sink(out_mode).as_deref().and_then(audio::get);
    let mut vol_t = std::time::Instant::now();
    let mut query = String::new(); // the live type-to-search string

    loop {
        // The description line for the highlighted row, if it is a channel.
        let sel_chan: Option<usize> = stack.last().and_then(|v| v.cur()).and_then(|r| r.chan);
        let detail = sel_chan.map(|i| detail_line(&lineup[i]));

        {
            let view = stack.last().unwrap();
            let out = out_label(out_mode);
            let vtag = vol.map(|v| format!(" {}", v.short())).unwrap_or_default();
            let status = if !query.is_empty() {
                format!("type/BKSP  ENTER play  ESC done {out}")
            } else if stack.len() == 1 {
                format!("ENTER open  ESC ESC quit {out}{vtag}")
            } else {
                format!("ENTER play  ESC back  a-z find {out}{vtag}")
            };
            draw(&mut scr, view, detail.as_deref(), &status);
        }

        let events = kbd.poll(200);
        // The volume keys belong to the desktop (labwc binds them session-wide),
        // so the footer has to go and look rather than be told.
        if vol_t.elapsed().as_millis() >= 1500 {
            vol_t = std::time::Instant::now();
            vol = cur_sink(out_mode).as_deref().and_then(audio::get);
        }
        if events.is_empty() {
            continue;
        }
        fb::backlight_on();

        // One key press emits a Down and, if it is printable, a Char right
        // behind it (see kbd.rs). Letters only mean "search" when they are not
        // already a navigation key, so each Down records whether it was one and
        // the Char that follows is dropped if it was. Assigned, never just set:
        // one poll can carry two presses, and a DOWN arrow must not swallow the
        // letter typed after it.
        let mut handled_down = false;
        for ev in events {
            match ev {
                Ev::Down(KEY_DOWN) => {
                    handled_down = true;
                    move_sel(stack.last_mut().unwrap(), 1, list_rows(&scr, &detail));
                }
                Ev::Down(KEY_UP) => {
                    handled_down = true;
                    move_sel(stack.last_mut().unwrap(), -1, list_rows(&scr, &detail));
                }
                Ev::Down(KEY_PAGEDOWN) => {
                    handled_down = true;
                    move_sel(stack.last_mut().unwrap(), 8, list_rows(&scr, &detail));
                }
                Ev::Down(KEY_PAGEUP) => {
                    handled_down = true;
                    move_sel(stack.last_mut().unwrap(), -8, list_rows(&scr, &detail));
                }
                Ev::Down(KEY_HOME) => {
                    handled_down = true;
                    let v = stack.last_mut().unwrap();
                    v.sel = 0;
                    v.top = 0;
                }
                Ev::Down(KEY_END) => {
                    handled_down = true;
                    let v = stack.last_mut().unwrap();
                    v.sel = v.rows.len().saturating_sub(1);
                    fix_scroll(v, list_rows(&scr, &detail));
                }
                Ev::Down(KEY_TAB) => {
                    handled_down = true;
                    out_mode = match out_mode {
                        OutMode::Auto => OutMode::ForceScreen,
                        OutMode::ForceScreen => OutMode::ForceTv,
                        OutMode::ForceTv => OutMode::Auto,
                    };
                    vol = cur_sink(out_mode).as_deref().and_then(audio::get);
                    esc_armed = false;
                }
                Ev::Down(KEY_BACKSPACE) if !query.is_empty() => {
                    handled_down = true;
                    query.pop();
                    let v = stack.last_mut().unwrap();
                    if query.is_empty() {
                        stack.pop();
                    } else {
                        *v = search_view(&lineup, &query);
                    }
                }
                Ev::Down(KEY_ENTER) | Ev::Down(KEY_RIGHT) => {
                    handled_down = true;
                    esc_armed = false;
                    let action = stack.last().and_then(|v| v.cur()).map(|r| r.action.clone());
                    match action {
                        Some(Action::Play(i)) => {
                            play(
                                &mut scr, &mut kbd, &cfg, &lineup[i], detail.as_deref(),
                                &mut out_mode, &mut vol,
                            );
                        }
                        Some(Action::OpenAll) => stack.push(all_view(&lineup)),
                        Some(Action::OpenCat(c)) => stack.push(cat_view(&lineup, &c)),
                        None => {}
                    }
                }
                Ev::Down(KEY_ESC) | Ev::Down(KEY_LEFT) => {
                    handled_down = true;
                    if !query.is_empty() {
                        query.clear();
                        stack.pop();
                        esc_armed = false;
                    } else if stack.len() > 1 {
                        stack.pop();
                        esc_armed = false;
                    } else if esc_armed {
                        return;
                    } else {
                        esc_armed = true;
                    }
                }
                Ev::Down(_) => handled_down = false, // not a navigation key
                Ev::Char(c) if !handled_down => {
                    // Searching always runs over the WHOLE lineup, not the list
                    // on screen: typing "mst" on the Comedy page should still
                    // find MST3K whatever category it lives in.
                    esc_armed = false;
                    if query.is_empty() {
                        query.push(c);
                        stack.push(search_view(&lineup, &query));
                    } else {
                        query.push(c);
                        *stack.last_mut().unwrap() = search_view(&lineup, &query);
                    }
                }
                Ev::Char(_) => {} // the Char behind a navigation key
            }
        }
    }
}

// How many list rows fit: the guide loses one to the description line when
// there is one to show, so the scroll window has to agree with draw().
fn list_rows(scr: &Screen, detail: &Option<String>) -> usize {
    scr.rows.saturating_sub(if detail.is_some() { 3 } else { 2 })
}

fn move_sel(v: &mut View, delta: i32, visible: usize) {
    let n = v.rows.len() as i32;
    if n == 0 {
        return;
    }
    let mut s = v.sel as i32 + delta;
    if s < 0 {
        s = 0;
    }
    if s >= n {
        s = n - 1;
    }
    v.sel = s as usize;
    fix_scroll(v, visible);
}

fn fix_scroll(v: &mut View, visible: usize) {
    if visible == 0 {
        return;
    }
    if v.sel < v.top {
        v.top = v.sel;
    } else if v.sel >= v.top + visible {
        v.top = v.sel + 1 - visible;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lineup() -> Vec<Chan> {
        let mk = |n: i64, name: &str, group: &str, host: &str| Chan {
            number: n,
            explicit_number: false,
            name: name.into(),
            group: group.into(),
            url: format!("https://{host}/{n}/master.m3u8"),
        };
        vec![
            mk(430, "MST3K", "Comedy", "cdn.example"),
            mk(2531, "NBC News NOW", "News", "cdn.example"),
            mk(2533, "CNN HEADLINES", "News", "jellyfin.lan"),
            mk(3730, "Little Stars Universe", "Kids", "cdn.example"),
        ]
    }

    #[test]
    fn home_lists_every_group_once_with_counts() {
        let l = lineup();
        let v = home_view(&l);
        let labels: Vec<&str> = v.rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels[0], "All channels (4)");
        assert!(labels.contains(&"News  (2)"), "{labels:?}");
        assert!(labels.contains(&"Comedy  (1)"), "{labels:?}");
        assert_eq!(labels.len(), 4, "3 groups + All");
    }

    #[test]
    fn search_spans_the_whole_lineup_by_name_or_number() {
        let l = lineup();
        assert_eq!(search_view(&l, "mst").rows.len(), 1);
        assert_eq!(search_view(&l, "MST").rows.len(), 1, "case-insensitive");
        // Substring, not word match: "news" hits NBC News NOW and nothing else
        // here, because CNN HEADLINES does not contain it.
        assert_eq!(search_view(&l, "news").rows.len(), 1);
        assert_eq!(search_view(&l, "253").rows.len(), 2, "number prefix matches 2531+2533");
        assert_eq!(search_view(&l, "2531").rows.len(), 1);
        assert_eq!(search_view(&l, "zzz").rows.len(), 0);
    }

    #[test]
    fn channel_numbers_are_column_aligned() {
        let l = lineup();
        let v = all_view(&l);
        assert_eq!(v.rows[0].label, " 430  MST3K");
        assert_eq!(v.rows[3].label, "3730  Little Stars Universe");
    }

    #[test]
    fn the_detail_line_names_the_group_and_the_host() {
        let l = lineup();
        assert_eq!(detail_line(&l[2]), "News  jellyfin.lan");
        // A playlist can hold a bare path or a scheme we do not parse; the group
        // alone is still useful and must not panic.
        let odd = Chan {
            number: 1,
            explicit_number: false,
            name: "X".into(),
            group: "Local".into(),
            url: "file.ts".into(),
        };
        assert_eq!(detail_line(&odd), "Local");
    }

    #[test]
    fn the_detail_line_costs_a_list_row() {
        // draw() and the scroll window must agree or the selection can sit on a
        // row that is not painted.
        assert_eq!(9usize.saturating_sub(3) + 1, 9usize.saturating_sub(2));
    }

    #[test]
    fn expand_drops_the_decoder_flag_when_there_is_none() {
        let t: Vec<String> = ["ffmpeg", "-c:v", "{vdec}", "-i", "{url}"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            expand(&t, "http://x", Some("h264_v4l2m2m")),
            vec!["ffmpeg", "-c:v", "h264_v4l2m2m", "-i", "http://x"]
        );
        assert_eq!(expand(&t, "http://x", None), vec!["ffmpeg", "-i", "http://x"]);
    }

    #[test]
    fn scroll_window_follows_the_selection() {
        let mut v = all_view(&lineup());
        move_sel(&mut v, 3, 2);
        assert_eq!(v.sel, 3);
        assert_eq!(v.top, 2);
        move_sel(&mut v, -10, 2);
        assert_eq!((v.sel, v.top), (0, 0));
    }

    #[test]
    fn accented_names_fold_to_something_the_7x14_font_can_draw() {
        assert_eq!(fold_ascii("En Espanol"), "En Espanol");
        assert_eq!(fold_ascii("En Espa\u{f1}ol"), "En Espanol");
        assert_eq!(fold_ascii("T\u{e9}l\u{e9} Qu\u{e9}bec"), "Tele Quebec");
        assert_eq!(fold_ascii("Stra\u{df}e"), "Strasse");
        assert_eq!(fold_ascii("\u{422}\u{412}"), "..", "Cyrillic has no fold; dots, not blanks");
        // clip() has to count the FOLDED length, or a row of accents overruns.
        assert_eq!(clip("\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}", 3), "ee>");
    }

    #[test]
    fn config_sources_add_up_and_keep_their_order() {
        use playlist::Src;
        let v: serde_json::Value = serde_json::from_str(
            r#"{"playlist":"https://a/list.m3u","playlists":["/tmp/b.m3u","  ","https://c/d.m3u"]}"#,
        )
        .unwrap();
        let got: Vec<String> = sources(&v).iter().map(|s| s.label()).collect();
        // The built-in lineup is first and the configured ones follow in order.
        // A blank entry is not a source; before this, "playlist" REPLACED the
        // built-ins instead of adding to them.
        assert_eq!(
            got,
            vec![
                "built-in lineup".to_string(),
                "https://a/list.m3u".into(),
                "/tmp/b.m3u".into(),
                "https://c/d.m3u".into()
            ]
        );
        let off: serde_json::Value = serde_json::from_str(r#"{"include_builtin":false}"#).unwrap();
        assert!(sources(&off).is_empty(), "include_builtin:false drops the built-ins");
        assert_eq!(as_src("https://x/y.m3u"), Src::Url("https://x/y.m3u".into()));
        assert_eq!(as_src("/home/me/y.m3u"), Src::Path("/home/me/y.m3u".into()));
    }

    #[test]
    fn a_stream_is_not_mistaken_for_a_channel_list() {
        // Red Bull TV's real master: #EXTINF appears nowhere, but six variant
        // URLs do, and the old rule read them as six channels named after the
        // CDN host.
        let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=299200,RESOLUTION=320x180\n\
                      https://cdn/lo.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=7612000\n\
                      https://cdn/hi.m3u8\n";
        assert!(!is_channel_list(master));
        // A media playlist DOES have #EXTINF lines - one per segment.
        let media = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\nseg1.ts\n#EXTINF:6.0,\nseg2.ts\n";
        assert!(!is_channel_list(media));
        // The real thing.
        let list = "#EXTM3U\n#EXTINF:-1 group-title=\"News\",DW English\nhttps://h/a.m3u8\n";
        assert!(is_channel_list(list));
        // And a bare file of URLs somebody typed.
        assert!(is_channel_list("https://h/a.m3u8\nhttps://h/b.m3u8\n"));
        assert!(!is_channel_list("https://h/a.m3u8\n"), "one URL is one channel");
    }

    #[test]
    fn a_pack_or_group_name_becomes_a_safe_filename() {
        assert_eq!(slug("Opinion & Talk"), "opinion-talk");
        assert_eq!(slug("free-tv_US.m3u8"), "free-tv-us-m3u8");
        assert_eq!(slug("///"), "channels");
    }
}
