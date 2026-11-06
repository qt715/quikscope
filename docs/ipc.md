# IPC protocol

The daemon listens on `$XDG_RUNTIME_DIR/moment-$WAYLAND_DISPLAY.sock`
(mode 0600). `MOMENT_SOCKET` overrides the path for both sides. Each
connection carries exactly one request line and one response line, UTF-8,
`\n`-terminated.

```sh
echo 'capture screen no-notify' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/moment-$WAYLAND_DISPLAY.sock
```

## Requests

```text
capture <mode> [flag]... [output=NAME] [geometry=X,Y,WxH]
open-last
open <path>          # rest of the line, spaces allowed
notifications <on|off|toggle|status>   # alias: notify
ping
quit
```

`mode`: `region`, `output` (alias `monitor`), `window` (alias `active`),
`screen` (aliases `all`, `fullscreen`).

Flags override the configuration for one request: `edit`, `notify`,
`clipboard`, `cursor`, each optionally prefixed with `no-`, and `save`.
`text` extracts the capture's text instead of copying the image (see
[text extraction](text-extraction.md)).

Requests are rejected with an `err` response when malformed: unknown
commands, modes or flags, extra arguments, or invalid geometry.

## Responses

```text
ok <shot-id> <width>x<height>          # capture/open succeeded
ok <shot-id> <width>x<height> <path>   # … and `save` wrote <path>
ok pong                                # ping, quit
ok text <shot-id> <text>               # capture … text: the text read (empty: none)
ok notifications <on|off>             # notifications: the setting now in effect
cancelled                              # the user cancelled the selection
err <message>                          # single line
```

`capture region` without `geometry=` answers only after the user finishes
selecting; clients should not apply a read timeout to it. A second
interactive selection while one is open is refused with
`err a selection is already in progress`.

In `ok text` replies `\`, line feeds and carriage returns in the text are
escaped as `\\`, `\n` and `\r`, so the reply stays on one line.

## Lifecycle

- `moment` commands other than `daemon` and `quit` start a detached daemon
  (`setsid`) when nothing listens on the socket, logging to
  `$XDG_CACHE_HOME/moment/daemon.log`, and wait up to 5 s for it.
- A daemon refuses to start while another one answers on the socket; a stale
  socket file left by a crash is replaced.
- `quit` replies, then exits the daemon and removes the socket.
