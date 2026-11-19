# IPC protocol

The daemon listens on `$XDG_RUNTIME_DIR/quikscope-$WAYLAND_DISPLAY.sock`
(mode 0600); on X11 the name is `quikscope-x11-<display>.sock`, e.g.
`quikscope-x11-0.sock` for `DISPLAY=:0`. On Windows it is an AF_UNIX
socket at `%LOCALAPPDATA%\quikscope\ipc.sock`. `QUIKSCOPE_SOCKET`
overrides the path for both sides. Each connection carries exactly one request line and one response line, UTF-8,
`\n`-terminated.
Requests are limited to 64 KiB including the line terminator. With no
`XDG_RUNTIME_DIR`, Unix sockets use a `quikscope-<uid>` directory under the
temporary directory, created with mode 0700; foreign-owned, symlinked or
permissive pre-existing directories are refused.

For `open`, exactly one space separates the command from the path; subsequent
spaces, including trailing spaces, belong to the path. The line protocol cannot
represent paths containing a newline or non-UTF-8 names. Rename such files before
opening them. Same-user clients are trusted; the socket is not an authorization
boundary against other processes running as your user.

```sh
echo 'capture screen no-notify' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/quikscope-$WAYLAND_DISPLAY.sock
```

## Requests

```text
capture <mode> [flag]... [output=NAME] [geometry=X,Y,WxH]
open-last
pick                # pick a pixel's color anywhere on the desktop
open <path>          # rest of the line, spaces allowed
record <video|animation|anim|gif> [mode] [flag]... [fps=N] [format=F] [audio=ID,ID|no-audio]
       [output=NAME] [geometry=X,Y,WxH]
record <stop|pause|resume|toggle-pause|cancel|status>
audio-devices        # recordable audio devices
settings            # open or focus the settings window; replies `ok pong`
notifications <on|off|toggle|status>   # alias: notify
version             # the daemon's version
ping
quit
```

`mode`: `region`, `output` (alias `monitor`), `window` (alias `active`),
`screen` (aliases `all`, `fullscreen`).

Flags override the configuration for one request: `edit`, `notify`,
`clipboard`, `cursor`, `freeze`, each optionally prefixed with `no-`, and `save`.
Omitting a boolean flag uses the config value; repeated flags use the last
value. `freeze` defaults to true. `capture region no-freeze` selects over the
live desktop, then captures pixels after the selection overlay closes;
`capture region freeze` retains the frozen snapshot behavior. These flags
affect interactive region selection only, not fixed geometry or other modes.
`text` extracts the capture's text instead of copying the image (see
[text extraction](text-extraction.md)).

`open` probes the file: videos (H.264 in MP4, MOV or MKV) open in the video
editor, animated GIF/WebP/AVIF/JPEG XL/APNG in the annotator's animation
mode, still images in the annotator.

`record video|animation` starts a recording, or, while one is running (of
either kind), stops and saves it, so one key binding toggles. `anim` is
short for `animation`; `gif` is an animation recorded as GIF whatever
`record.animation_format` says. Its mode
defaults to `region` (selection over the live desktop); `output`/`screen`
record a whole output, `window` the active window's rectangle at start.
A recording covers one output: a selection spanning outputs is clipped to
the output holding most of it. Flags: `edit`, `notify`, `clipboard`,
`cursor` (each with `no-`), `fps=1…240`, `format=` (`mp4`/`mkv`/`mov` for
video, `gif`/`webp`/`avif`/`jxl` for animation), `audio=` comma-separated
device ids or `no-audio`. Animations never record audio.

`record cancel` discards the running recording. While a stopped recording
is still being saved it cancels the save instead: a video is discarded, an
animation keeps its lossless recording, which opens in the annotator from
the notification and is offered again after the daemon restarts.

Requests are rejected with an `err` response when malformed: unknown
commands, modes or flags, extra arguments, or invalid geometry.

## Responses

```text
ok <shot-id> <width>x<height>          # capture/open succeeded
ok <shot-id> <width>x<height> <path>   # … and `save` wrote <path>
ok pong                                # ping, quit, settings
ok version <version>                  # version: the daemon's version, e.g. 0.3.0
ok text <shot-id> <text>               # capture … text: the text read (empty: none)
ok notifications <on|off>             # notifications: the setting now in effect
ok color <#rrggbb>                    # pick: the color clicked
ok recording <kind> <width>x<height>  # record: started (frame size in pixels)
ok recorded <id> <width>x<height> <duration-ms> <path>   # record/record stop: saved
ok record idle                        # record status/cancel: nothing recording
ok record <kind> <elapsed-ms> <running|paused>           # record status/pause/resume
ok record saving [percent]            # record status/cancel: still being saved
ok audio <json>                       # audio-devices: [{"id","name","output"}]
cancelled                              # selection cancelled, or the save was cancelled
err <message>                          # single line
```

A `record` request that starts answers once recording has begun (after the
selection); one that stops answers after the file is finalized, which for a
long GIF can take a while: clients should not apply a read timeout to either.
`<id>` is what a notification click opens (`ok recorded` ids share the shot
numbering). `<path>` is the rest of the line. `ok record saving` carries
the percentage written (0–100) while a stopped recording is encoded, and
no number while a discarded one is torn down; a new recording cannot start
until it reads `idle`.

`capture region` without `geometry=` and `pick` answer only after the user
finishes selecting; clients should not apply a read timeout to them. A second
interactive selection (either kind) while one is open is refused with
`err a selection is already in progress`.

`pick` freezes every output like a frozen region selection, without the
pointer, and magnifies the pixels at the pointer; a click or Enter picks the
physical pixel under it, Esc or a right click cancels. The color also opens
in the color window, reused if one is open.

In `ok text` replies `\`, line feeds and carriage returns in the text are
escaped as `\\`, `\n` and `\r`, so the reply stays on one line.

## Lifecycle

- `quikscope` commands other than `daemon`, `version` and `quit` start a detached daemon
  (`setsid`) when nothing listens on the socket, logging to
  `$XDG_CACHE_HOME/quikscope/daemon.log`, and wait up to 5 s for it. The log
  is appended to; above 1 MiB it moves to `daemon.log.1` first.
- A daemon refuses to start while another one answers on the socket; a stale
  socket file left by a crash is replaced.
- `quit` stops and saves a running recording, waits for every file still
  being written (recordings being saved, exports and saves started from an
  editor, even one already closed) and refuses new ones meanwhile, then
  replies, exits the daemon and removes the socket.
- At start-up the daemon recovers what a daemon that died left behind (see
  [crash recovery](configuration.md#crash-recovery)).
