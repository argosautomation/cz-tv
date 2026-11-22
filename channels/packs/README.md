# Packs

A pack is a playlist that is **installed but not switched on**. It ships in the
`.deb` at `/usr/share/cz-tv/packs/`, nothing in it appears in the guide, and one
command adds it:

```sh
cz-tv --add /usr/share/cz-tv/packs/opinion.m3u
cz-tv --sources          # shows every pack the package installed
```

Removing it again is deleting one file from `~/.config/cz-tv/channels.d/`.

Packs exist so that the lineup a stranger gets on first install can stay small
and uncontroversial while still letting the project carry lists that plenty of
people want. A channel belongs in a pack rather than in `channels/builtin.m3u`
when it is:

- **partisan or advocacy television** - it plays, it is free, it is the
  publisher's own feed, and the default lineup still should not choose a side
  for somebody who just installed a TV guide (`opinion.m3u`)
- **regional**, where the audience is a country or a language rather than
  everyone
- **a genre somebody specifically wants** - music video, religious, shopping,
  24/7 single-show channels

Everything in `../../CONTRIBUTING.md` still applies to a pack: the publisher's
own free feed, https, no credential in the URL, nothing rebroadcast from a pay
service, and a frame decoded and looked at.
