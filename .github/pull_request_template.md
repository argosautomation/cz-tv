<!-- Adding a channel? Keep the change to channels/*.m3u and fill this in. -->

**What this adds or changes**

**If it adds a channel**

- [ ] `tools/lint-channels.sh` is clean
- [ ] `tools/check-channels.sh` decoded a frame and I **looked at the contact
      sheet** - the picture was the programme, not a slate or a colour bar
- [ ] It is the publisher's own free feed: no account, no key, no DRM, nothing
      rebroadcast from a pay service, and no credential in the URL
- [ ] Where you are (a channel can be fine for you and geo-blocked for the
      next person): 

Contact sheet / screenshot:

**If it changes the app**

- [ ] `cargo test` passes
- [ ] If it touches the guide, the keypad or playback: `tests/cz_tv_hw_test.sh`
      run on a card, and here is what the frames showed
