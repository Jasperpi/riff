# riff
spotify but cooler

Spotify in your terminal. Plays music itself (it is its own Spotify Connect
device), shows album art as half-blocks, ASCII or braille, follows along with
synced lyrics, and draws a live spectrum of what you're hearing.

Needs Spotify Premium.

## Run

```
riff            # first run signs you in
riff --demo     # look around offline, no account needed
riff doctor     # check what's working (--play, --mix, --dj test audio paths)
```

Press `?` inside riff for every key. The short version:

| | |
|---|---|
| `space` `n` `p` | play/pause, next, previous |
| `,` `.` / `<` `>` | seek 5 s / 30 s (hold to scrub) |
| `-` `+` | volume |
| `enter` | play the selected track, or open the album/artist/playlist |
| `/` | search Spotify |
| `ctrl-f` | filter the current list |
| `x` | select songs to build a queue (see below) |
| `a` `f` | add to queue, like |
| `o` `A` | go to the track's album / artist |
| `v` | now playing: big cover, lyrics, spectrum |
| `c` | cover style: blocks, ascii, braille, pulse, depth |
| `D` | DJ decks: two songs at once, with a crossfader |
| `4` | four squares: four songs at once, arrows to switch |
| `m` `M` | crossfade between songs, change its length |
| `z` | party mode |
| `d` `u` | devices, up next |
| `s` `r` | shuffle, repeat |
| `esc` | back |

The mouse works too: click to select, double-click to play, click the progress
bar to seek, wheel to scroll. Keyboard media keys and the desktop's media
widget control riff as well (MPRIS).

### Building a queue

Mark songs with `x` (or ctrl-click, or right-click), the way you'd pick several
files. For a run of songs press `V`, move, and everything between where you
started and where you are is selected; `V` again to finish. The selection stays
with you as you move around: open another playlist, search for something, mark
more. Then:

- `a` queues everything selected, in the order you picked it
- `enter` plays the selection right now
- `X` clears it

### DJ decks

`D` switches on two decks, A and B, that play at the same time through a
crossfader. Whatever was playing carries on from deck A.

1. Browse or search as usual and press `1` or `2` on a song to load it onto
   deck A or B (`enter` loads whichever deck is free). The first song plays;
   after that a new one waits, cued.
2. Press `D` to see the decks. `m` mixes the other deck in: it starts silently,
   riff listens for its beat, bends its tempo to match the deck that's playing,
   lines the beats up, and then glides the crossfader across. If the two songs
   are too far apart in tempo, or one has no clear beat, you get a plain blend.
3. Or do it by hand:

| | |
|---|---|
| `a` `b` | play / pause deck A / B |
| `←` `→` | crossfader (`↓` centres it, shift moves faster) |
| `tab` | choose which deck the keys below act on |
| `,` `.` | seek |
| `[` `]` `0` | tempo down / up / back to normal (up to ±12%) |
| `s` | match this deck's tempo and beat to the other |
| `u` `i` `o` | EQ: low / mid / high up a step |
| `j` `k` `l` | EQ: low / mid / high down a step |
| `J` `K` `L` | kill that band (again, or `U` `I` `O`, puts it back to flat) |
| `x` | bass swap: this deck gets the lows, the other loses them |
| `{` `}` | nudge this deck a touch earlier / later |
| `M` | how long `m` takes: 3, 6, 9 or 12 seconds |
| `esc` | back to your songs, decks still playing |
| `D` | decks off |

Each deck has a three-band EQ, from a full kill up to +6 dB, with a meter
beside each knob showing what that band is putting out. The usual way to
bring a song in: kill its lows (`J`), mix it in, and press `x` on it when you
want its bass line to take over from the other deck's.

What you do is heard about 30 ms later: riff feeds the sound card directly
through a buffer a few milliseconds long.

Each deck shows its tempo in BPM and a light that flashes on its beat. Beat
matching works best on music with a steady pulse; a band with a live drummer
wanders, and riff follows it as well as it can. The decks are local to riff:
other Spotify devices won't show what they are playing.

### Four squares

`4` and four songs start at once, one in each quarter of the screen, each
printed like a pop-art poster in its own loud colours. You hear one of
them. The arrow keys pick another: you hear it start to happen straight
away, and it lands on the beat.

- All four play the whole time. The three you can't hear run underneath
  the one you can, bent to its tempo and slid onto its beats, and onto its
  bars where riff can tell which beat is the one. So whichever you pick is
  already in time. (It takes ten seconds or so after starting for the
  squares to find their beat; a square that is in time shows ⇄.)
- Settle on a song for a few beats and the next switch is a blend: the new
  song's top end creeps in under the old one, then on the next half-bar the
  bass lines swap and the old song bows out over a beat.
- Flick between squares and each switch is a cut on the next beat, with
  the old song lifting into it and an echo of it trailing off underneath.
- A song too far apart in tempo to match is brought in with an echo-out.
- The square coming in shows **NEXT** and a dot for each beat to go.

| | |
|---|---|
| `←` `→` `↑` `↓` | the next song (click a square too) |
| shift+arrow | drop it: the old song stutters for a beat, then the new one |
| `enter` | stutter the song playing for a beat |
| `tab` | a square chosen for you |
| `n` `p` | round the squares |
| `space` | pause all four, together |
| `esc` | back to your songs, squares still playing |
| `4` | squares off |

The squares start with what's playing, then songs you marked with `x`,
then the list you're in from the cursor down. When a song in a quiet
square finishes, the next one from that list takes its place; a square
near the end of its song hands over by itself. Back in your songs,
`enter` puts the song under the cursor into a square.

The squares, like the decks, are local to riff, and they take over from
the decks (and the decks from them).

### Crossfade

Outside the decks, `m` blends the end of each song into the start of the next
(6 seconds by default; `M` cycles 3/6/9/12). Skips and seeks get a very short
blend so they never click. Pause, skip and volume stay instant throughout.

### To the beat

riff listens to what it is playing and finds the beat.

- `c` until the cover style says **pulse**: the album cover punches in,
  brightens and ripples on each beat.
- `c` once more for **depth**: riff works out how near each part of the
  cover is and turns it into a scene with a camera moving through it. The
  foreground and background drift apart as if you were moving your head,
  and on every bar the camera swings round to a new viewpoint; over eight
  bars it dollies in and out, keeping the subject's size while the
  background breathes behind it. A coloured light circles the scene and
  falls on it by the way each surface faces, catching the edges on the
  beat. The focus rests on the subject, with the distance soft and hazy,
  and pulls back to the distance at the end of each phrase. On each
  downbeat a line of light sweeps through from front to back, and dust
  drifts at every depth, passing in front of and behind things. Whatever is
  nearest jumps forward on the beat. This style is optional (see Build)
  and fetches a 27 MB model the first time.
  In a terminal that can show pictures (kitty, Konsole, Ghostty, WezTerm) it
  is drawn as one, at the screen's own resolution, instead of in half-blocks;
  `pictures = false` in the config keeps it to half-blocks.
- `z` for **party mode**: the whole interface joins in. Behind it all runs a
  scene that changes every four bars: the spectrum closing in from every
  edge, a tunnel of rings leaping outward on the beat, a slow plasma, laser
  beams sweeping up from the floor, rain of glyphs falling at the pace of
  their part of the spectrum, stars streaking past. Colours wheel and jump
  on the beat, the text sways, and covers get a frame of light. When the
  music drops back in after a breakdown, the screen flashes once, shakes,
  glitches, and jumps to a hotter scene. `z` again to calm it down.

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
| `~/.cache/riff/` | library cache, cover images and their depth, audio cache, `riff.log` |

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

The depth cover style is left out unless asked for, because it adds about
25 MB to the program:

```
cargo build --release --features depth
```

It runs a small depth-estimation network (Depth Anything V2 Small, Apache-2.0)
on your own machine with ONNX Runtime; nothing about your covers leaves it.
The model is kept in `~/.cache/riff/models/`.
