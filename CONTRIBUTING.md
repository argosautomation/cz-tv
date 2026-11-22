# Contributing

Two kinds of contribution, and the first one needs no Rust at all.

## Adding a channel

The lineup is a data file: [`channels/builtin.m3u`](channels/builtin.m3u). A
channel is two lines in it.

```
#EXTINF:-1 group-title="World",DW English
https://dwamdstream102.akamaized.net/hls/live/2015525/dwstream102/index.m3u8
```

Open a pull request that touches only that file (or a file under
[`channels/packs/`](channels/packs/README.md)). CI lints it in a few seconds
and a maintainer plays it before merging.

### What gets merged

- **The publisher's own free stream**, or a feed the publisher distributes
  through a free ad-supported platform. The test is: would the broadcaster
  recognise this URL as theirs?
- **No account, no key, no DRM.** The card has no Widevine CDM, so anything
  behind one cannot play here at all.
- **`https`**, and **no credential anywhere in the URL**. A stream path with a
  username or a password in it is somebody's subscription; publishing it hands
  out their login, and it will be rejected.
- **H.264 + AAC over HLS.** The Cardputer Zero hardware-decodes H.264. HEVC is
  software-only on this hardware and will not keep up.
- **A name that fits**: about 28 characters is what the 320x170 guide shows.
- **A `group-title`** that already exists, unless the channel really starts a
  new group.

### What does not

- **Anything rebroadcast from a pay service.** The big community lists are full
  of pay TV re-streamed off a box at a bare IP address. That is the one thing
  that could get this project itself taken down, and it is an automatic no.
- **Geo-locked feeds that only work where you live.** Say so if you are not
  sure; a channel that answers for you and 403s elsewhere belongs in a regional
  pack, with the country in its name.
- **Partisan and advocacy channels in the default lineup.** They are welcome in
  [`channels/packs/opinion.m3u`](channels/packs/opinion.m3u), which anybody can
  add with one command. The lineup a stranger gets on first install should not
  pick a side for them.

### Check it yourself first

```sh
tools/lint-channels.sh                 # policy, no network, 2 seconds
tools/check-channels.sh -o out         # fetches, decodes a frame, builds a
                                       # contact sheet - then LOOK AT IT
```

**Looking at the picture is the whole check, and skipping it is how dead
channels get merged.** This app began as a Pluto TV client. Pluto now answers
every client profile with a *"Pluto TV is no longer available on this device"*
slate - and that slate is clean H.264 + AAC, delivered with an HTTP 200. A
reachability check passes on it. A codec probe passes on it. A playlist parse
passes on it. The same sweep that found that also found four channels serving
`200` on the master playlist and `404` on every variant inside it, and two BBC
channels whose "live stream" is now a Pluto *"That's a wrap!"* card. Every one
of those looks healthy in a terminal.

So: run the checker, open `out/contact-sheet.jpg`, and paste it into the pull
request. If a frame shows a slate, a colour bar or a "not available" card, that
channel is dead no matter what the status code said.

## Changing the app

It is a single static Rust binary, two crates (`serde_json`, `libc`), and it
cross-compiles from a normal x86-64 Linux box:

```sh
rustup target add aarch64-unknown-linux-musl
cargo test                     # no hardware needed
packaging/build.sh             # the aarch64 binary
```

`tests/cz_tv_hw_test.sh` runs **on the card**: it drives the real app through a
uinput keyboard and grabs `/dev/fb0` on a timeline, so a screenshot is read off
the hardware rather than reasoned about. If a change touches the guide, the
keypad or playback, run it and say in the pull request what the frames showed.

Things worth knowing before changing the player paths:

- The guide is the SPI panel (`/dev/fb0`, fbdev) and video on a TV is the vc4
  HDMI out (labwc/Wayland). The app polls `/sys/class/drm/*HDMI*/status`, so
  plugging a TV in mid-show moves the picture.
- Variant URIs **must** have their dot segments removed (RFC 3986). Several
  CDNs publish `../../../manifest/...` and answer `403` to a path that still
  contains a `..`.
- Masters are served with CRLF often enough that a stray `\r` on the end of a
  variant URI is a real 404.
- The card has 352 MiB of RAM. Do not build on it.
