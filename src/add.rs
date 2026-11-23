// cz-tv - adding channels from the app itself.
//
// `cz-tv --add <url>` does the whole job from a shell, and on this device that
// is precisely the problem: the card is a TV box with a thumb keyboard, the
// binary is not on PATH for someone who installed the .deb, and the first
// thing a new owner wants is a channel the built-in lineup does not have. So
// everything --add can do is here as well, in the guide, under one row on the
// home screen.
//
// The order of the menu is the order of effort. A pack or a playlist already
// sitting on the card costs one keypress, and that covers a USB stick, a
// scp'd file and the lists the package ships. Typing a URL is last because it
// is the slow one, and because the keypad has no shift key: letters arrive
// lowercase and TAB is what makes them capitals (see kbd.rs - the hardware has
// a Sym layer for punctuation and an Fn layer for media keys, and no third
// layer to put a shift on).
//
// Nothing here parses or writes a playlist itself. Every path ends in
// crate::add_target or crate::add_stream, the same two functions --add calls,
// so the app and the command line cannot drift apart.

use crate::fb::{self, Screen, AMBER, BLACK, BLUE, CYAN, GREY, RED, WHITE};
use crate::kbd::{Ev, Keyboard, KEY_BACKSPACE, KEY_DOWN, KEY_END, KEY_ENTER, KEY_ESC, KEY_HOME,
                 KEY_LEFT, KEY_PAGEDOWN, KEY_PAGEUP, KEY_RIGHT, KEY_TAB, KEY_UP};
use crate::{cfg_dir, clip, home_dir, pack_dir, slug, trunc, wait_key, Added};

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub enum Outcome {
    /// Nothing was written. The caller can carry on with the lineup it has.
    Nothing,
    /// Channels were added, so the lineup on screen is now out of date.
    Changed,
}

// ------------------------------------------------------------------- menu
// A list that scrolls, drawn like the guide so that the two feel like one app.
// The guide's own View carries playback actions it has no use for here, so this
// is the same shape with the payload left out.
struct Menu {
    title: String,
    rows: Vec<String>,
    sel: usize,
    top: usize,
}

impl Menu {
    fn new(title: &str, rows: Vec<String>) -> Menu {
        Menu { title: title.to_string(), rows, sel: 0, top: 0 }
    }

    fn visible(scr: &Screen) -> usize {
        scr.rows.saturating_sub(2)
    }

    fn draw(&self, scr: &mut Screen, status: &str) {
        scr.clear(BLACK);
        let cols = scr.cols;
        let n = self.rows.len();
        let hdr = if n > 0 {
            format!("{}  {}/{}", self.title, self.sel + 1, n)
        } else {
            self.title.clone()
        };
        scr.row_bar(0, &clip(&hdr, cols), BLACK, CYAN);
        for i in 0..Menu::visible(scr) {
            let idx = self.top + i;
            if idx >= n {
                break;
            }
            let label = clip(&self.rows[idx], cols);
            if idx == self.sel {
                scr.row_bar(i + 1, &label, BLACK, AMBER);
            } else {
                scr.text(0, i + 1, &label, WHITE);
            }
        }
        let fr = scr.rows - 1;
        scr.fill_rect(0, fr * crate::font::CH, scr.w, crate::font::CH, BLUE);
        scr.text(0, fr, &clip(status, cols), WHITE);
        scr.flush();
    }

    fn move_by(&mut self, delta: i32, visible: usize) {
        let n = self.rows.len() as i32;
        if n == 0 {
            return;
        }
        self.sel = (self.sel as i32 + delta).clamp(0, n - 1) as usize;
        if self.sel < self.top {
            self.top = self.sel;
        } else if visible > 0 && self.sel >= self.top + visible {
            self.top = self.sel + 1 - visible;
        }
    }

    // The navigation every screen here shares. Returns Some(index) on ENTER.
    fn key(&mut self, code: u16, scr: &Screen) -> Nav {
        let visible = Menu::visible(scr);
        match code {
            KEY_DOWN => self.move_by(1, visible),
            KEY_UP => self.move_by(-1, visible),
            KEY_PAGEDOWN => self.move_by(visible as i32, visible),
            KEY_PAGEUP => self.move_by(-(visible as i32), visible),
            KEY_HOME => self.move_by(-(self.rows.len() as i32), visible),
            KEY_END => self.move_by(self.rows.len() as i32, visible),
            KEY_ENTER | KEY_RIGHT => return Nav::Pick(self.sel),
            KEY_ESC | KEY_LEFT => return Nav::Back,
            _ => {}
        }
        Nav::Stay
    }
}

enum Nav {
    Stay,
    Back,
    Pick(usize),
}

// ------------------------------------------------------------- what we offer
enum Offer {
    /// Type an address in. Slowest, and the only one that works for a stream
    /// nobody has put on the card yet.
    Url,
    /// A playlist file that is already here: a pack the package installed, a
    /// file on a USB stick, something scp'd into the home directory.
    File { path: String, title: String, count: usize, added: bool },
}

// Everything that can be added without typing. Packs first - they are curated
// and they are the reason the package ships them - then any playlist lying
// around on the card.
fn offers() -> Vec<Offer> {
    let mut out = vec![Offer::Url];
    let mut seen: Vec<String> = Vec::new();
    for dir in search_dirs() {
        for path in playlists_in(&dir) {
            if seen.contains(&path) {
                continue;
            }
            seen.push(path.clone());
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let count = crate::playlist::parse_m3u(&text).len();
            if count == 0 {
                continue;
            }
            let stem = std::path::Path::new(&path)
                .file_stem()
                .and_then(|x| x.to_str())
                .unwrap_or("list")
                .to_string();
            out.push(Offer::File {
                added: already_added(&path, &stem),
                title: title_of(&text).unwrap_or_else(|| stem.replace(['-', '_'], " ")),
                path,
                count,
            });
        }
    }
    out
}

// Where a playlist that is already on the card can plausibly be. A removable
// drive is the interesting one: the card mounts USB storage under /media, so
// "put an m3u on a stick and plug it in" needs no network and no typing at all.
fn search_dirs() -> Vec<String> {
    let home = home_dir();
    let mut dirs = vec![pack_dir()];
    for d in ["", "/Downloads", "/Documents", "/playlists"] {
        dirs.push(format!("{home}{d}"));
    }
    for base in ["/media", "/mnt", "/run/media"] {
        for lvl1 in subdirs(base) {
            for lvl2 in subdirs(&lvl1) {
                dirs.push(lvl2);
            }
            dirs.push(lvl1);
        }
    }
    dirs
}

fn subdirs(base: &str) -> Vec<String> {
    let mut out: Vec<String> = match std::fs::read_dir(base) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.path().to_str().map(String::from))
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out.truncate(8);
    out
}

fn playlists_in(dir: &str) -> Vec<String> {
    // The app's own drop-ins are already in the guide; offering to add the
    // file it is currently reading would be a loop with nothing in it.
    let mine = format!("{}/channels.d", cfg_dir());
    if dir == mine || dir == cfg_dir() {
        return Vec::new();
    }
    let mut out: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .filter(|e| {
                e.path().extension().map(|x| x == "m3u" || x == "m3u8").unwrap_or(false)
            })
            .filter_map(|e| e.path().to_str().map(String::from))
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out.truncate(12);
    out
}

// A pack says what it is on its first comment line, so use that rather than
// the filename: "Opinion and talk" reads better than "opinion". That line is
// written for someone reading the file, so it usually carries a caveat after a
// dash - take the part before it, and if what is left is still a sentence
// rather than a name, fall back to the filename.
fn title_of(text: &str) -> Option<String> {
    let line = text.lines().take(4).find(|l| l.starts_with("# ") && l.len() > 4)?;
    let t = line[2..].split(" - ").next()?.trim().trim_end_matches('.').trim();
    (!t.is_empty() && t.chars().count() <= 32).then(|| t.to_string())
}

// `--add` on a local list writes it to channels.d/<slug>.m3u, so that file
// existing is what "already added" means.
fn already_added(path: &str, stem: &str) -> bool {
    let dest = format!("{}/channels.d/{}.m3u", cfg_dir(), slug(stem));
    if !std::path::Path::new(&dest).is_file() {
        return false;
    }
    match (std::fs::read_to_string(path), std::fs::read_to_string(&dest)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

// ------------------------------------------------------------------- screens
/// The Add channels screen. Returns Changed if the guide needs reloading.
pub fn run(scr: &mut Screen, kbd: &mut Keyboard) -> Outcome {
    let list = offers();
    let rows: Vec<String> = list
        .iter()
        .map(|o| match o {
            Offer::Url => "Type an address  (a stream or a playlist)".to_string(),
            Offer::File { title, count, added, .. } => {
                format!("{}  ({}){}", title, count, if *added { "  added" } else { "" })
            }
        })
        .collect();
    let mut menu = Menu::new("Add channels", rows);
    loop {
        menu.draw(scr, "ENTER pick  ESC back");
        for ev in kbd.poll(200) {
            let Ev::Down(code) = ev else { continue };
            fb::backlight_on();
            match menu.key(code, scr) {
                Nav::Back => return Outcome::Nothing,
                Nav::Stay => {}
                Nav::Pick(i) => {
                    let changed = match &list[i] {
                        Offer::Url => type_url(scr, kbd),
                        Offer::File { path, title, .. } => browse(scr, kbd, path, title),
                    };
                    if changed {
                        return Outcome::Changed;
                    }
                }
            }
        }
    }
}

// One playlist, opened. Everything in it, or one channel out of it - which is
// the point of a pack: a list of 40 regional channels is useful even when only
// one of them is wanted, and picking that one should not mean taking all 40.
fn browse(scr: &mut Screen, kbd: &mut Keyboard, path: &str, title: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        fail(scr, kbd, &format!("could not read {}", trunc(path, 38)));
        return false;
    };
    let chans = crate::playlist::parse_m3u(&text);
    let mut rows = vec![format!("Add all {} channels", chans.len())];
    rows.extend(chans.iter().map(|c| format!("  {}", c.name)));
    let mut menu = Menu::new(title, rows);
    loop {
        menu.draw(scr, "ENTER add  ESC back");
        for ev in kbd.poll(200) {
            let Ev::Down(code) = ev else { continue };
            fb::backlight_on();
            match menu.key(code, scr) {
                Nav::Back => return false,
                Nav::Stay => {}
                Nav::Pick(0) => {
                    working(scr, "Adding", title);
                    return report(scr, kbd, crate::add_target(path, None, None));
                }
                Nav::Pick(i) => {
                    let c = &chans[i - 1];
                    working(scr, "Checking", &c.name);
                    return report(scr, kbd, crate::add_stream(&c.url, &c.name, &c.group));
                }
            }
        }
    }
}

// The address editor.
//
// It starts at "https://" because almost every answer does and that is eight
// keypresses saved on a thumb keyboard, and it is ordinary text, so BKSP takes
// it away for the rare http:// or a local path.
fn type_url(scr: &mut Screen, kbd: &mut Keyboard) -> bool {
    let Some(url) = edit_text(
        scr,
        kbd,
        "type an address",
        "one stream, or a whole .m3u playlist",
        "https://",
        12, // "https://" and something after it
    ) else {
        return false;
    };
    working(scr, "Checking", &url);
    let t = match crate::classify(&url) {
        Ok(t) => t,
        Err(e) => {
            fail(scr, kbd, e.lines().next().unwrap_or("could not read that"));
            return false;
        }
    };
    // A playlist names its own channels. A single stream does not, and the
    // guide would otherwise list it by its CDN hostname forever, so ask now -
    // prefilled, so ENTER is still the whole answer for anyone who does not
    // care.
    let named = match &t {
        crate::Target::Stream { suggested, .. } => {
            let Some(n) = edit_text(
                scr,
                kbd,
                "name this channel",
                "ENTER keeps the name shown",
                &crate::trunc(suggested, 40),
                1,
            ) else {
                return false;
            };
            Some(n)
        }
        crate::Target::Playlist { .. } => None,
    };
    working(scr, "Adding", named.as_deref().unwrap_or(&url));
    report(scr, kbd, crate::add_classified(t, named.as_deref(), None))
}

// One line of text, typed on a keypad that has no shift key: letters arrive
// lowercase and TAB is what makes them capitals. Returns None on ESC.
fn edit_text(
    scr: &mut Screen,
    kbd: &mut Keyboard,
    title: &str,
    hint: &str,
    initial: &str,
    min_len: usize,
) -> Option<String> {
    let mut text = initial.to_string();
    let mut caps = false;
    loop {
        draw_editor(scr, title, hint, &text, caps);
        for ev in kbd.poll(200) {
            fb::backlight_on();
            match ev {
                Ev::Down(KEY_ESC) => return None,
                Ev::Down(KEY_TAB) => caps = !caps,
                Ev::Down(KEY_BACKSPACE) => {
                    text.pop();
                }
                Ev::Down(KEY_ENTER) => {
                    if text.chars().count() >= min_len {
                        return Some(text);
                    }
                }
                Ev::Down(_) => {}
                Ev::Char(c) => {
                    if text.len() < 400 {
                        text.push(if caps { c.to_ascii_uppercase() } else { c });
                    }
                }
            }
        }
    }
}

fn draw_editor(scr: &mut Screen, title: &str, hint: &str, text: &str, caps: bool) {
    scr.clear(BLACK);
    let cols = scr.cols;
    scr.row_bar(0, &clip(&format!("Add channels - {title}"), cols), BLACK, CYAN);

    // Wrapped, with the last line always on screen: a URL is longer than 45
    // columns and the end of it is the part being typed.
    let chars: Vec<char> = crate::fold_ascii(text).chars().collect();
    let (cur_row, cur_col) = (chars.len() / cols, chars.len() % cols);
    let lines: Vec<String> = (0..=cur_row)
        .map(|r| chars[r * cols..chars.len().min((r + 1) * cols)].iter().collect())
        .collect();
    const BODY: usize = 4; // rows 2..5, above the hint and the status bar
    let first = cur_row.saturating_sub(BODY - 1);
    for (i, l) in lines[first..=cur_row].iter().enumerate() {
        scr.text(0, 2 + i, l, AMBER);
    }
    // A block cursor rather than an underscore: on a 7x14 font at arm's length
    // an underscore is indistinguishable from the bottom of the row.
    scr.fill_rect(
        cur_col * crate::font::CW,
        (2 + cur_row - first) * crate::font::CH,
        crate::font::CW,
        crate::font::CH,
        if caps { AMBER } else { GREY },
    );

    let fr = scr.rows - 1;
    let hint = if caps { "TAB: back to small letters" } else { hint };
    scr.text(0, fr - 2, &clip(hint, cols), GREY);
    let status = if caps {
        "ENTER ok  ESC cancel  TAB CAPITALS"
    } else {
        "ENTER ok  ESC cancel  TAB caps"
    };
    scr.fill_rect(0, fr * crate::font::CH, scr.w, crate::font::CH, BLUE);
    scr.text(0, fr, &clip(status, cols), if caps { AMBER } else { WHITE });
    scr.flush();
}

// `--add` fetches and decode-tests before it writes, which takes a few seconds
// on a card; say what is happening rather than appear to have died.
fn working(scr: &mut Screen, verb: &str, what: &str) {
    crate::message(scr, "Add channels", &[&format!("{verb} {}...", trunc(what, 30))], CYAN);
}

fn fail(scr: &mut Screen, kbd: &mut Keyboard, why: &str) {
    crate::message(scr, "Add channels", &[&trunc(why, 44), "", "Nothing was changed."], RED);
    footer(scr, "any key goes back");
    wait_key(kbd);
}

// message() draws no status bar; these screens wait for a keypress, so they
// need one saying so.
fn footer(scr: &mut Screen, text: &str) {
    let fr = scr.rows - 1;
    let cols = scr.cols;
    scr.fill_rect(0, fr * crate::font::CH, scr.w, crate::font::CH, BLUE);
    scr.text(0, fr, &clip(text, cols), WHITE);
    scr.flush();
}

// The receipt. It names the file it wrote, because that file is how somebody
// takes the channels away again - there is no "remove" in here, and a line
// telling them where to look is better than a screen that pretends there is.
fn report(scr: &mut Screen, kbd: &mut Keyboard, r: Result<Added, String>) -> bool {
    let a = match r {
        Ok(a) => a,
        Err(e) => {
            let first = e.lines().next().unwrap_or("could not add that").to_string();
            fail(scr, kbd, &first);
            return false;
        }
    };
    let mut lines: Vec<String> = vec![a.headline.clone()];
    for n in a.names.iter().take(3) {
        lines.push(format!("  {n}"));
    }
    if a.more > 0 {
        lines.push(format!("  ... and {} more", a.more));
    }
    if let Some(n) = &a.note {
        lines.push(n.clone());
    }
    lines.push(String::new());
    lines.push(short_path(&a.dest));
    let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
    crate::message(scr, "Add channels", &refs, if a.already { AMBER } else { CYAN });
    footer(scr, if a.already { "any key goes back" } else { "any key shows the guide" });
    wait_key(kbd);
    !a.already
}

// 45 columns is not enough for /home/somebody/.config/cz-tv/channels.d/x.m3u.
fn short_path(p: &str) -> String {
    let home = home_dir();
    if let Some(rest) = p.strip_prefix(&home) {
        return format!("~{rest}");
    }
    p.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scroll_window_follows_the_selection() {
        let mut m = Menu::new("x", (0..20).map(|i| i.to_string()).collect());
        m.move_by(5, 4);
        assert_eq!((m.sel, m.top), (5, 2));
        m.move_by(-99, 4);
        assert_eq!((m.sel, m.top), (0, 0), "clamped at the top, window follows");
        m.move_by(99, 4);
        assert_eq!((m.sel, m.top), (19, 16), "clamped at the end, last page shown");
    }

    #[test]
    fn an_empty_menu_does_not_panic_or_move() {
        let mut m = Menu::new("x", Vec::new());
        m.move_by(1, 4);
        assert_eq!((m.sel, m.top), (0, 0));
    }

    #[test]
    fn a_pack_is_named_by_its_own_first_comment() {
        let packlike = "#EXTM3U\n# Opinion and talk\n#\n#EXTINF:-1,A\nhttps://h/a\n";
        assert_eq!(title_of(packlike).as_deref(), Some("Opinion and talk"));
        // The real pack header: a name, then a caveat for whoever opens the
        // file. The guide wants the name.
        let real = "#EXTM3U\n# Opinion and talk - NOT in the default lineup.\n";
        assert_eq!(title_of(real).as_deref(), Some("Opinion and talk"));
        // The built-in lineup's banner is a sentence, not a title; too long is
        // better left to the filename than clipped to something misleading.
        let wordy = "#EXTM3U\n# This is the list every fresh install starts with, and more.\n";
        assert_eq!(title_of(wordy), None);
        assert_eq!(title_of("#EXTM3U\nhttps://h/a\n"), None);
    }

    #[test]
    fn a_home_path_is_shortened_but_someone_elses_is_not() {
        let home = home_dir();
        assert_eq!(short_path(&format!("{home}/.config/cz-tv/channels.d/x.m3u")),
                   "~/.config/cz-tv/channels.d/x.m3u");
        assert_eq!(short_path("/usr/share/cz-tv/packs/opinion.m3u"),
                   "/usr/share/cz-tv/packs/opinion.m3u");
    }

    #[test]
    fn the_apps_own_dropins_are_never_offered_back_to_it() {
        // Reading the file the guide is already reading, and writing it back
        // into the same directory, is a loop with nothing in it.
        assert!(playlists_in(&format!("{}/channels.d", cfg_dir())).is_empty());
    }
}
