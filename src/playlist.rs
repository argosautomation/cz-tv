// The channel source. Everything provider-specific in this app lives here, and
// it is one format: M3U. That is deliberate - an app meant to be handed round a
// group should work with whatever the person already has (a Jellyfin or Plex
// playlist, their own IPTV subscription, an iptv-org list, a text file of URLs
// they wrote by hand) rather than with one service that can withdraw its feed.
//
// It did withdraw it, in fact: this started as a Pluto TV client. Pluto's open
// HLS endpoint returns a "Pluto TV is no longer available on this device" slate
// to every client profile now, so the lineup moved out of the code and into a
// playlist the user owns. BUILTIN below is only a starting point.

use std::process::Command;

const UA: &str = "CardputerZero-TV/1.0";

// The built-in lineup is a DATA file, channels/builtin.m3u, not source code.
// That is the whole point: adding a channel is two lines in an M3U that anyone
// can read and send as a pull request, with no Rust, no rebuild of their own
// and no review of program logic. It is embedded in the binary at compile time
// so a fresh install still works with nothing on disk.
//
// Every channel in it was verified by fetching the stream, decoding a frame and
// LOOKING at the picture - an HTTP 200 and a clean codec probe are not proof a
// feed is alive, a takedown slate passes both. tools/check-channels.sh is that
// check, packaged for contributors.
pub const BUILTIN: &str = include_str!("../channels/builtin.m3u");

#[derive(Clone)]
pub struct Chan {
    pub number: i64,
    pub name: String,
    pub group: String,
    pub url: String,
    // True when the playlist gave this channel a tvg-chno. Merged lineups have
    // to be renumbered, and a number the author wrote down must survive that
    // while a positional one must not.
    pub explicit_number: bool,
}

// ------------------------------------------------------------------ m3u
// One attribute out of an #EXTINF line: key="value". Written by hand rather
// than with a regex crate - the binary links two crates and stays under 500 KB.
fn attr(line: &str, key: &str) -> Option<String> {
    let pat = format!("{key}=\"");
    let i = line.find(&pat)? + pat.len();
    let rest = &line[i..];
    let j = rest.find('"')?;
    Some(rest[..j].to_string())
}

pub fn parse_m3u(text: &str) -> Vec<Chan> {
    let mut out: Vec<Chan> = Vec::new();
    let mut name = String::new();
    let mut group = String::new();
    let mut number: Option<i64> = None;
    let mut have_inf = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            // "#EXTINF:-1 tvg-id="x" group-title="News",DW English"
            name = match rest.find(',') {
                Some(i) => rest[i + 1..].trim().to_string(),
                None => String::new(),
            };
            if name.is_empty() {
                name = attr(rest, "tvg-name").unwrap_or_default();
            }
            group = attr(rest, "group-title").unwrap_or_default();
            number = attr(rest, "tvg-chno").and_then(|s| s.trim().parse().ok());
            have_inf = true;
            continue;
        }
        // Some playlists put the group on its own line instead.
        if let Some(g) = line.strip_prefix("#EXTGRP:") {
            group = g.trim().to_string();
            continue;
        }
        if line.starts_with('#') {
            continue; // #EXTM3U, #EXTVLCOPT, #PLAYLIST, comments
        }
        // A bare URL with no #EXTINF above it is still a channel; name it after
        // its own host so a hand-written list of URLs works.
        let label = if have_inf && !name.is_empty() {
            name.clone()
        } else {
            host_of(line)
        };
        out.push(Chan {
            number: number.unwrap_or(out.len() as i64 + 1),
            name: label,
            group: if group.is_empty() { "Channels".to_string() } else { group.clone() },
            url: line.to_string(),
            explicit_number: number.is_some(),
        });
        name.clear();
        group.clear();
        number = None;
        have_inf = false;
    }
    out
}

fn host_of(url: &str) -> String {
    url.split("//")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .unwrap_or(url)
        .to_string()
}

// ------------------------------------------------------------------ loading
fn curl(url: &str) -> Result<String, String> {
    let out = Command::new("curl")
        .args(["-sS", "--fail", "--location", "--max-time", "30", "-A", UA, url])
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err("could not fetch the playlist".into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ------------------------------------------------------------------ sources
// Channels come from a LIST of sources, not one, and they ADD UP. That is the
// difference between "here is my playlist instead of yours" and "I found three
// more streams": before this, dropping a channels.m3u in replaced the built-in
// lineup, so adding one channel meant copying all 27 first.
#[derive(Clone, PartialEq, Debug)]
pub enum Src {
    Builtin,
    Path(String),
    Url(String),
}

impl Src {
    pub fn label(&self) -> String {
        match self {
            Src::Builtin => "built-in lineup".to_string(),
            Src::Path(p) => p.clone(),
            Src::Url(u) => u.clone(),
        }
    }
}

// What one source contributed, for `--sources` and for the probe. A source that
// fails is reported, never fatal: one unreachable URL must not take the guide
// down when the other sources are fine.
pub struct Loaded {
    pub label: String,
    pub count: usize,
    // Channels this source carried that were already in the guide under the
    // same URL. Without this, adding a list you already have reports "0
    // channels" and reads like a failure.
    pub dupes: usize,
    pub err: Option<String>,
}

impl Loaded {
    pub fn note(&self) -> String {
        match (&self.err, self.dupes) {
            (Some(e), _) => format!("FAILED: {e}"),
            (None, 0) => format!("{} channels", self.count),
            (None, d) => format!("{} channels ({d} already listed)", self.count),
        }
    }
}

pub fn read_source(src: &Src, cache_dir: &str, max_age_secs: u64) -> Result<String, String> {
    match src {
        Src::Builtin => Ok(BUILTIN.to_string()),
        Src::Path(p) => std::fs::read_to_string(p).map_err(|e| format!("{e}")),
        Src::Url(u) => {
            let cache = cache_path(cache_dir, u);
            if let Some(t) = fresh_cache(&cache, max_age_secs) {
                return Ok(t);
            }
            match curl(u) {
                Ok(t) => {
                    write_cache(&cache, &t);
                    Ok(t)
                }
                // Offline with a stale copy on disk is still a working guide.
                Err(e) => std::fs::read_to_string(&cache).map_err(|_| e),
            }
        }
    }
}

// Load every source in order and merge them. Two rules, both of which fall out
// of "the later source is the one the owner of the card wrote":
//   * same channel NAME  -> the later source wins, in place. Replacing a
//     built-in channel whose feed died needs no new syntax, just its name.
//   * same channel URL   -> kept once. Subscribing to two overlapping
//     community lists is normal and should not double the guide.
pub fn load_many(srcs: &[Src], cache_dir: &str, max_age_secs: u64) -> (Vec<Chan>, Vec<Loaded>) {
    let mut out: Vec<Chan> = Vec::new();
    let mut reports: Vec<Loaded> = Vec::new();
    for src in srcs {
        let label = src.label();
        match read_source(src, cache_dir, max_age_secs) {
            Err(e) => reports.push(Loaded { label, count: 0, dupes: 0, err: Some(e) }),
            Ok(text) => {
                let chans = parse_m3u(&text);
                let mut added = 0usize;
                let mut dupes = 0usize;
                for ch in chans {
                    let same_name = out
                        .iter()
                        .position(|o| o.name.trim().eq_ignore_ascii_case(ch.name.trim()));
                    if let Some(i) = same_name {
                        let keep_number = out[i].number;
                        let keep_explicit = out[i].explicit_number;
                        out[i] = ch;
                        if !out[i].explicit_number {
                            out[i].number = keep_number;
                            out[i].explicit_number = keep_explicit;
                        }
                        added += 1;
                        continue;
                    }
                    if out.iter().any(|o| o.url == ch.url) {
                        dupes += 1;
                        continue;
                    }
                    out.push(ch);
                    added += 1;
                }
                reports.push(Loaded { label, count: added, dupes, err: None });
            }
        }
    }
    renumber(&mut out);
    (out, reports)
}

// Each source numbered its own channels from 1, so a merged lineup has
// duplicates. Numbers the playlist author wrote down (tvg-chno) are kept;
// everything else takes the lowest number nobody claimed, in order.
fn renumber(chans: &mut [Chan]) {
    let mut taken: Vec<i64> = chans.iter().filter(|c| c.explicit_number).map(|c| c.number).collect();
    taken.sort_unstable();
    let mut next = 1i64;
    for ch in chans.iter_mut() {
        if ch.explicit_number {
            continue;
        }
        while taken.binary_search(&next).is_ok() {
            next += 1;
        }
        ch.number = next;
        next += 1;
    }
}

// A cache file per URL, named after the URL so two subscriptions do not
// overwrite each other. FNV-1a rather than a hash crate: it is 6 lines and this
// binary links two dependencies on purpose.
pub fn cache_path(dir: &str, url: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in url.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("{dir}/playlist-{h:016x}.m3u")
}

fn fresh_cache(path: &str, max_age_secs: u64) -> Option<String> {
    let md = std::fs::metadata(path).ok()?;
    if md.modified().ok()?.elapsed().ok()?.as_secs() > max_age_secs {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

fn write_cache(path: &str, text: &str) {
    if let Some(dir) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = format!("{path}.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

// ------------------------------------------------------------------ renditions
// Pick a rendition out of a master playlist ourselves rather than let the player
// do it. Players take the first variant listed, and plenty of masters list the
// biggest one first - which on a 320x170 panel means decoding 1080p and then
// throwing almost all of it away. `lowest` is true for the panel, false for a TV.
//
// Returns None when the URL is not a master playlist (a media playlist, an .mpd,
// a plain .ts or .mp4); the caller then hands the player the original URL.
pub fn pick_variant(url: &str, lowest: bool) -> Option<String> {
    if !url.contains(".m3u") {
        return None;
    }
    let body = curl(url).ok()?;
    pick_variant_from(&body, url, lowest)
}

pub fn pick_variant_from(master: &str, base: &str, lowest: bool) -> Option<String> {
    let mut best: Option<(u64, String)> = None;
    let mut pending: Option<u64> = None;
    for line in master.lines() {
        let line = line.trim();
        // The attributes start AFTER the tag, and BANDWIDTH is often the very
        // first one ("#EXT-X-STREAM-INF:BANDWIDTH=299200,RESOLUTION=320x180").
        // Splitting the whole line on ',' leaves the tag glued to that first
        // attribute, so the prefix has to come off first - without this every
        // variant scored 0 and the lowest and the highest were the same stream.
        if let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(rank(attrs));
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(score) = pending.take() {
            let better = match &best {
                None => true,
                Some((b, _)) => {
                    if lowest {
                        score < *b
                    } else {
                        score > *b
                    }
                }
            };
            if better {
                best = Some((score, resolve(base, line)));
            }
        }
    }
    best.map(|(_, u)| u)
}

// How big a rendition is. RESOLUTION is the honest answer for "will this fit a
// 320x170 panel" and most masters carry it; BANDWIDTH is the fallback. Mixing
// the two scales inside one master would misrank, but in practice a master
// either labels every variant with a resolution or none of them.
fn rank(attrs: &str) -> u64 {
    let field = |key: &str| -> Option<&str> {
        attrs.split(',').find_map(|kv| kv.trim().strip_prefix(key))
    };
    if let Some(res) = field("RESOLUTION=") {
        let mut it = res.split(['x', 'X']);
        if let (Some(w), Some(h)) = (it.next(), it.next()) {
            if let (Ok(w), Ok(h)) = (w.trim().parse::<u64>(), h.trim().parse::<u64>()) {
                return w * h;
            }
        }
    }
    field("BANDWIDTH=")
        .or_else(|| field("AVERAGE-BANDWIDTH="))
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

// Variant URIs are usually relative ("1042180/playlist.m3u8?..."), so they need
// the master's directory, and the master's own query string must not come
// along. Dot segments have to be removed the way RFC 3986 says, not left in the
// path: three of the channels in the built-in lineup publish their variants as
// "../../../manifest/..." and the CDN answers 403 to a path that still contains
// a "..". Players get this right, which is why the bug only showed up once we
// started picking the rendition ourselves - the master fetch and the codec
// probe both looked fine and the picture never came.
fn resolve(base: &str, rel: &str) -> String {
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return rel.to_string();
    }
    let no_q = base.split('?').next().unwrap_or(base);
    // scheme://host, and the path after it
    let after_scheme = match no_q.find("//") {
        Some(i) => i + 2,
        None => return rel.to_string(),
    };
    let (origin, base_path) = match no_q[after_scheme..].find('/') {
        Some(j) => no_q.split_at(after_scheme + j),
        None => (no_q, "/"),
    };
    // The relative reference keeps its own query string; only the path part of
    // it takes place in the merge.
    let (rel_path, rel_q) = match rel.find('?') {
        Some(i) => (&rel[..i], &rel[i..]),
        None => (rel, ""),
    };
    let merged = if let Some(r) = rel_path.strip_prefix('/') {
        format!("/{r}")
    } else {
        let dir = match base_path.rfind('/') {
            Some(i) => &base_path[..=i],
            None => "/",
        };
        format!("{dir}{rel_path}")
    };
    format!("{origin}{}{rel_q}", remove_dot_segments(&merged))
}

// RFC 3986 section 5.2.4, the short version: "." drops out, ".." pops the
// segment before it, and a leading ".." at the root is discarded rather than
// escaping above it.
fn remove_dot_segments(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "." => continue,
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let mut joined = out.join("/");
    if !joined.starts_with('/') {
        joined.insert(0, '/');
    }
    // A trailing "." or ".." means the result names a directory.
    if (path.ends_with("/.") || path.ends_with("/..")) && !joined.ends_with('/') {
        joined.push('/');
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_list_parses_and_is_all_https() {
        let c = parse_m3u(BUILTIN);
        assert_eq!(c.len(), 27);
        assert_eq!(c[0].name, "CBS News 24/7");
        assert_eq!(c[0].group, "News");
        assert_eq!(c[0].number, 1);
        assert_eq!(c[26].group, "Test");
        // Every channel is reachable without an account, so none of them may
        // carry credentials in the URL - that is how an IPTV path leaks a login.
        for ch in &c {
            assert!(!ch.url.contains("password"), "{} has a credential in the URL", ch.name);
        }
        for ch in &c {
            assert!(ch.url.starts_with("https://"), "{} is not https", ch.name);
            assert!(!ch.name.is_empty());
        }
    }

    // Write a playlist to a unique temp path. No tempfile crate: this binary
    // links two dependencies on purpose, and the test only needs a path.
    fn tmp_m3u(tag: &str, body: &str) -> String {
        let p = format!(
            "{}/cz-tv-test-{}-{}.m3u",
            std::env::temp_dir().display(),
            tag,
            std::process::id()
        );
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn sources_add_up_instead_of_replacing_each_other() {
        let mine = tmp_m3u(
            "add",
            "#EXTM3U\n#EXTINF:-1 group-title=\"Mine\",My Camera\nhttps://cam.lan/s.m3u8\n",
        );
        let (lineup, reports) =
            load_many(&[Src::Builtin, Src::Path(mine.clone())], "/nonexistent", 0);
        let builtin = parse_m3u(BUILTIN).len();
        assert_eq!(lineup.len(), builtin + 1, "the drop-in ADDS, it does not replace");
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].count, builtin);
        assert_eq!(reports[1].count, 1);
        assert!(lineup.iter().any(|c| c.name == "My Camera"));
        assert!(lineup.iter().any(|c| c.name == "DW English"));
        let _ = std::fs::remove_file(mine);
    }

    #[test]
    fn a_later_source_replaces_a_built_in_channel_by_name() {
        // How a dead built-in channel gets fixed locally: same name, new URL,
        // in a file an upgrade will not overwrite.
        let fix = tmp_m3u(
            "fix",
            "#EXTM3U\n#EXTINF:-1 group-title=\"News\",DW English\nhttps://new.example/dw.m3u8\n",
        );
        let (lineup, _) = load_many(&[Src::Builtin, Src::Path(fix.clone())], "/nonexistent", 0);
        assert_eq!(lineup.len(), parse_m3u(BUILTIN).len(), "a replacement is not an addition");
        let dw: Vec<&Chan> = lineup.iter().filter(|c| c.name == "DW English").collect();
        assert_eq!(dw.len(), 1);
        assert_eq!(dw[0].url, "https://new.example/dw.m3u8");
        let _ = std::fs::remove_file(fix);
    }

    #[test]
    fn the_same_url_from_two_lists_appears_once() {
        let body = "#EXTM3U\n#EXTINF:-1,Alpha\nhttps://h/a.m3u8\n#EXTINF:-1,Beta\nhttps://h/b.m3u8\n";
        // Two community lists that overlap: same stream, different label.
        let a = tmp_m3u("dup-a", body);
        let b = tmp_m3u("dup-b", "#EXTM3U\n#EXTINF:-1,Alpha HD\nhttps://h/a.m3u8\n");
        let (lineup, _) = load_many(&[Src::Path(a.clone()), Src::Path(b.clone())], "/nonexistent", 0);
        assert_eq!(lineup.len(), 2, "the duplicate URL is dropped, not renamed in twice");
        // And the source says so, rather than reporting a bare "0 channels".
        let (_, reports) = load_many(&[Src::Path(a.clone()), Src::Path(b.clone())], "/nonexistent", 0);
        assert_eq!(reports[1].note(), "0 channels (1 already listed)");
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }

    #[test]
    fn merging_renumbers_but_keeps_the_numbers_someone_wrote_down() {
        // Each file numbered its own channels from 1. Without renumbering, four
        // channels share numbers 1 and 2 and typing a number finds the wrong one.
        let a = tmp_m3u("num-a", "#EXTM3U\n#EXTINF:-1,A\nhttps://h/1\n#EXTINF:-1,B\nhttps://h/2\n");
        let b = tmp_m3u(
            "num-b",
            "#EXTM3U\n#EXTINF:-1 tvg-chno=\"2\",C\nhttps://h/3\n#EXTINF:-1,D\nhttps://h/4\n",
        );
        let (lineup, _) = load_many(&[Src::Path(a.clone()), Src::Path(b.clone())], "/nonexistent", 0);
        let got: Vec<(i64, &str)> = lineup.iter().map(|c| (c.number, c.name.as_str())).collect();
        assert_eq!(got, vec![(1, "A"), (3, "B"), (2, "C"), (4, "D")]);
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }

    #[test]
    fn one_broken_source_does_not_take_the_guide_down() {
        let (lineup, reports) = load_many(
            &[Src::Builtin, Src::Path("/no/such/list.m3u".into())],
            "/nonexistent",
            0,
        );
        assert_eq!(lineup.len(), parse_m3u(BUILTIN).len());
        assert!(reports[1].err.is_some(), "and it is reported, by name");
        assert_eq!(reports[1].label, "/no/such/list.m3u");
    }

    #[test]
    fn a_cache_file_is_named_after_its_url() {
        let a = cache_path("/c", "https://x/list.m3u");
        let b = cache_path("/c", "https://y/list.m3u");
        assert_ne!(a, b, "two subscriptions must not share one cache file");
        assert_eq!(a, cache_path("/c", "https://x/list.m3u"), "and it is stable");
        assert!(a.starts_with("/c/playlist-") && a.ends_with(".m3u"));
    }

    #[test]
    fn real_world_m3u_shapes() {
        let m = r#"#EXTM3U x-tvg-url="http://e.xml"
#EXTINF:-1 tvg-id="a.us" tvg-logo="http://l/a.png" group-title="News",DW English
#EXTVLCOPT:network-caching=1000
https://h/a/master.m3u8
#EXTINF:-1 tvg-chno="430" tvg-name="MST3K" group-title="Comedy",
https://h/b/index.m3u8
#EXTINF:-1,No Group Here
https://h/c.ts
https://h/bare-url/stream.m3u8
"#;
        let c = parse_m3u(m);
        assert_eq!(c.len(), 4);
        assert_eq!((c[0].name.as_str(), c[0].group.as_str()), ("DW English", "News"));
        assert_eq!(c[0].url, "https://h/a/master.m3u8", "#EXTVLCOPT is not a URL");
        // Empty name after the comma falls back to tvg-name, and tvg-chno wins
        // over the positional number.
        assert_eq!((c[1].name.as_str(), c[1].number), ("MST3K", 430));
        assert_eq!(c[2].group, "Channels", "no group-title gets a default");
        // A bare URL with no #EXTINF is still a channel, named after its host.
        assert_eq!(c[3].name, "h");
    }

    #[test]
    fn extgrp_is_honoured() {
        let m = "#EXTM3U\n#EXTINF:-1,Chan A\n#EXTGRP:Movies\nhttps://h/a.m3u8\n";
        let c = parse_m3u(m);
        assert_eq!(c[0].group, "Movies");
    }

    #[test]
    fn a_group_does_not_leak_into_the_next_channel() {
        let m = "#EXTM3U\n#EXTINF:-1 group-title=\"News\",A\nhttps://h/a.m3u8\n\
                 #EXTINF:-1,B\nhttps://h/b.m3u8\n";
        let c = parse_m3u(m);
        assert_eq!(c[0].group, "News");
        assert_eq!(c[1].group, "Channels");
    }

    #[test]
    fn variant_pick_is_by_bandwidth_not_order() {
        let master = "#EXTM3U\n\
            #EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=1042180\n1042180/playlist.m3u8?sid=x\n\
            #EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=3321280\n3321280/playlist.m3u8?sid=x\n\
            #EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=640930\n640930/playlist.m3u8?sid=x\n";
        let base = "https://h.example/hls/channel/abc/master.m3u8?sid=x";
        assert_eq!(
            pick_variant_from(master, base, true),
            Some("https://h.example/hls/channel/abc/640930/playlist.m3u8?sid=x".into())
        );
        assert_eq!(
            pick_variant_from(master, base, false),
            Some("https://h.example/hls/channel/abc/3321280/playlist.m3u8?sid=x".into())
        );
    }

    #[test]
    fn bandwidth_as_the_first_attribute_still_ranks() {
        // Red Bull TV's real master shape. The tag and the first attribute are
        // not separated by a comma, so a naive split scored every variant 0 and
        // handed the panel the 1080p stream.
        let master = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=299200,RESOLUTION=320x180\nlo.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=3836800,RESOLUTION=1280x720\nmid.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=7612000,RESOLUTION=1920x1080\nhi.m3u8\n";
        let base = "https://h/live/master.m3u8";
        assert_eq!(pick_variant_from(master, base, true), Some("https://h/live/lo.m3u8".into()));
        assert_eq!(pick_variant_from(master, base, false), Some("https://h/live/hi.m3u8".into()));
    }

    #[test]
    fn resolution_outranks_bandwidth_when_both_are_given() {
        // A high-bitrate small rendition must not be sent to the TV over a
        // bigger picture that happens to encode more efficiently.
        let master = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=9000000,RESOLUTION=320x180\nsmall.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1920x1080\nbig.m3u8\n";
        let base = "https://h/live/master.m3u8";
        assert_eq!(pick_variant_from(master, base, false), Some("https://h/live/big.m3u8".into()));
        assert_eq!(pick_variant_from(master, base, true), Some("https://h/live/small.m3u8".into()));
    }

    #[test]
    fn variant_uris_resolve_absolute_root_relative_and_relative() {
        let base = "https://h.example/hls/live/1/master.m3u8";
        let m = |uri: &str| {
            format!("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n{uri}\n")
        };
        assert_eq!(
            pick_variant_from(&m("sub/v.m3u8"), base, true),
            Some("https://h.example/hls/live/1/sub/v.m3u8".into())
        );
        assert_eq!(
            pick_variant_from(&m("/other/v.m3u8"), base, true),
            Some("https://h.example/other/v.m3u8".into())
        );
        assert_eq!(
            pick_variant_from(&m("https://cdn2/v.m3u8"), base, true),
            Some("https://cdn2/v.m3u8".into())
        );
    }

    #[test]
    fn dot_segments_are_removed_the_way_a_player_removes_them() {
        // Real shapes from the built-in lineup. The CDN answers 403 to a path
        // that still contains "..", so leaving them in is a dead channel that
        // every other check calls healthy.
        let base = "https://cdn.example/v1/master/abc123/retro/index.m3u8";
        let m = |uri: &str| format!("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n{uri}\n");
        assert_eq!(
            pick_variant_from(&m("../../../manifest/abc123/retro/0.m3u8"), base, true),
            Some("https://cdn.example/v1/manifest/abc123/retro/0.m3u8".into())
        );
        // One level up, and a "." that is simply noise.
        assert_eq!(
            pick_variant_from(&m("../hls/./lo.m3u8"), base, true),
            Some("https://cdn.example/v1/master/abc123/hls/lo.m3u8".into())
        );
        // A variant's own query string survives; the master's does not.
        assert_eq!(
            pick_variant_from(&m("../v.m3u8?sid=9"), "https://cdn.example/a/b/m.m3u8?sid=1", true),
            Some("https://cdn.example/a/v.m3u8?sid=9".into())
        );
        // ".." must not climb above the root.
        assert_eq!(
            pick_variant_from(&m("../../../../../x.m3u8"), "https://cdn.example/a/m.m3u8", true),
            Some("https://cdn.example/x.m3u8".into())
        );
    }

    #[test]
    fn a_media_playlist_has_no_variants_to_pick() {
        // The caller must fall back to the original URL, not refuse to tune.
        let media = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\nseg1.ts\n";
        assert_eq!(pick_variant_from(media, "https://h/a.m3u8", true), None);
        // And a non-playlist URL never even gets fetched.
        assert_eq!(pick_variant("https://h/movie.mp4", true), None);
    }
}
