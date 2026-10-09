# riff

Spotify in your terminal. Plays music itself (it is its own Spotify Connect
device), shows album art as half-blocks, ASCII or braille, follows along with
synced lyrics, and draws a live spectrum of what you're hearing.

Needs Spotify Premium.

## Run

```
riff            # first run signs you in
riff --demo     # look around offline, no account needed
riff doctor     # check what's working (add --play for a 15-second audio test)
```

Press `?` inside riff for every key. The short version:

| | |
|---|---|
| `space` `n` `p` | play/pause, next, previous |
| `,` `.` / `<` `>` | seek 5 s / 30 s |
| `-` `+` | volume |
| `enter` | play the selected track, or open the album/artist/playlist |
| `/` | search Spotify |
| `ctrl-f` | filter the current list |
| `a` `f` | add to queue, like |
| `o` `A` | go to the track's album / artist |
| `v` | now playing: big cover, lyrics, spectrum |
| `c` | cover style: blocks, ascii, braille, off |
| `d` `u` | devices, up next |
| `s` `r` | shuffle, repeat |
| `esc` | back |

The mouse works too: click to select, double-click to play, click the progress
bar to seek, wheel to scroll. Keyboard media keys and the desktop's media
widget control riff as well (MPRIS).

## Why it doesn't stall

Most terminal Spotify clients route everything, even pausing their own player,
through Spotify's public Web API, using a Client ID shared by all their users.
That shared quota is permanently exhausted, so requests fail with `429 Too Many
Requests` and the interface freezes.

riff doesn't use the Web API for anything it needs:

- **Playback is local.** Play, pause, seek, skip, volume and queueing are calls
  into the embedded player, and playback state arrives as events. Nothing is
  polled.
- **Your library comes over the player's own connection**, the same service
  the official apps use: playlists and folders, Liked Songs, saved albums,
  followed artists, album and artist pages, search, lyrics. A 3,400-song
  Liked Songs list loads in a few seconds the first time; later visits fetch only what
  changed.
- **Everything you've opened is cached on disk** and shown instantly next time.

## Optional: your own Client ID

`riff setup` adds the few things only the Web API offers: playlist results in
search, Recently Played, On Repeat, and controlling other devices from riff.

1. <https://developer.spotify.com/dashboard> → **Create app**
2. Redirect URI: `http://127.0.0.1:8898/callback`, API: **Web API**
3. Copy the **Client ID** from the app's settings and paste it into `riff setup`

Because the app is yours, so is its request quota. If Spotify ever does say
"slow down", riff stops all Web API requests until the time it names instead of
retrying.

## Files

| | |
|---|---|
| `~/.config/riff/config.toml` | settings (volume, cover style, bitrate, …) |
| `~/.cache/riff/session/` | the saved login |
| `~/.cache/riff/` | library cache, cover images, audio cache, `riff.log` |

`riff logout` forgets the login. `kill -USR1 $(pidof riff)` saves the current
screen to `~/.cache/riff/frame.ans` for bug reports.

## Build

```
cargo build --release
cp target/release/riff ~/.cargo/bin/
cargo test
```

Needs the ALSA, OpenSSL and D-Bus development packages (`alsa-lib-devel`,
`openssl-devel`, `dbus-devel` on Fedora).
