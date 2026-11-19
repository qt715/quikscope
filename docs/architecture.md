# Architecture

One binary, two roles: a thin **client** (`quikscope capture …`) that sends one
line over a Unix socket, and a resident **daemon** (`quikscope daemon`) that does
the work. Keeping the daemon alive is the main latency win: the Wayland
connections, bound globals, shared-memory buffers, GPU device and shader
pipelines all exist before the keybind is pressed.

```mermaid
flowchart LR
    key[keybind] --> client[quikscope client]
    client -- "capture region\n(Unix socket)" --> ipc[IPC thread]
    ipc --> main[GPUI main thread]
    main -- job --> cap[capture thread\nwlr-screencopy]
    cap -- Snapshot --> main
    main --> overlay[layer-shell overlays\none per output]
    overlay -- rectangle --> main
    main --> clip[clipboard thread\nwlr-data-control]
    main --> notify[notification\nD-Bus]
    notify -- click --> main
    main --> editor[editor window]
    editor -- Ctrl+S --> export[CPU exporter\ntiny-skia] --> png[(PNG)]
```

The diagram shows the default frozen region path. Live region selection opens
overlays from output geometry first and requests pixels only after selection
and synchronized overlay teardown.

## Threads

| Thread | Owns | Notes |
| --- | --- | --- |
| GPUI main | windows, shots, documents | UI state; short atomic config edits and teardown barriers; capture/encoding stay on workers. |
| `quikscope-ipc` | the listening socket | One short-lived thread per connection, so an interactive selection never blocks `open-last` or `ping`. |
| `quikscope-capture` | its own Wayland connection, outputs, `wl_shm` buffers | Captures every requested output concurrently; buffers are reused while an output's mode is unchanged. Waits are bounded (3 s) so a wedged compositor cannot hang it. |
| `quikscope-clipboard` | a second Wayland connection, the data-control selection | Claims the selection immediately; a paste blocks only until the background PNG encoder finishes. |
| `quikscope-notify` | D-Bus signal subscriptions | Maps notification ids to shots; a click opens the editor. |
| `quikscope-ocr-<lang>` | one Tesseract instance (`libtesseract`, opened with `dlopen`) | One thread per language model; the primary stays loaded, others unload after 2 idle minutes. See [text extraction](text-extraction.md). |
| GPUI background pool | PNG encoding, tile building, composition, export | |

The capture and clipboard threads use their own connections rather than
GPUI's: GPUI's event loop is not exposed, and data-control needs no focused
surface (the daemon usually has none), unlike `wl_data_device`.

## Platform backends

Everything that differs between desktops sits behind the traits in
`src/platform/mod.rs`: `Capture` (screen pixels), `Desktop` (focused output,
active window, windows to snap to), `Clipboard`, `Notifier`, and how the
selection overlays become windows (`Surfaces`). `platform::connect` picks the
backends once at daemon start and logs the choice (`platform backend: …`).
Desktop-originated actions (notification clicks, and on platforms without
compositor keybinds, tray menus and global hotkeys) reach the daemon as
`platform::Event`s and run through the same request handler as the IPC
socket.

| Backend | Capture | Clipboard | Overlay | Window knowledge |
| --- | --- | --- | --- | --- |
| `wlroots` | `wlr-screencopy` | `wlr-data-control` | layer-shell | Hyprland IPC when present |
| `x11` | MIT-SHM GetImage, core GetImage fallback | CLIPBOARD selection, PNG/text, INCR | override-redirect PopUp | EWMH |
| `windows` | resident DXGI Desktop Duplication, reusable GDI fallback | delayed PNG + DIBV5/DIB/Unicode | borderless topmost GPUI popup | DWM foreground bounds and filtered top-level Z order |
| `portal` | Screenshot portal → PNG | data-control, Xwayland CLIPBOARD fallback | layer-shell when offered; otherwise one fullscreen xdg-toplevel | none |

X11 keeps separate persistent connections for capture, EWMH queries and
clipboard ownership. The capture thread reuses a SysV shared-memory segment;
RandR monitor rectangles use physical root coordinates, with no output scale.
TrueColor pixels become opaque BGRA, and XFixes optionally supplies the cursor.
The selection owner claims CLIPBOARD before the PNG encoder finishes and serves
the same encoded bytes; large transfers advance only when the requestor deletes
the previous INCR property. Popups bypass the window manager, so region selection
sets native keyboard focus after they map and restores the prior focus on exit.
There is no X11 GPU prewarm window because it could flash on the desktop.

### Screenshot portal

This integration is experimental: automated checks use a simulated portal in
headless labwc, not a real GNOME or KDE desktop.

On Wayland, native screencopy is attempted first; if it is unavailable, use
the Screenshot portal. `QUIKSCOPE_BACKEND=portal|wlroots` forces the choice.
Both the session-bus connection and a separate Wayland output-layout
connection are kept alive. A Request/Response subscription is installed
before `Screenshot("", {interactive: false, handle_token: …})`; filtering
uses the returned request handle, including older non-token-derived handles.
Codes 1/2 report cancellation/denial/failure. Local `file://` URIs are
percent-decoded using `url`, then decoded into BGRA.

The desktop raster is split using wl_output + xdg-output logical bounds;
negative origins, gaps and uniform fractional scaling are supported.
Output changes during the portal dialog produce an error instead of using
stale geometry. The API has no per-output scale metadata: nonuniform
mixed-scale rasters whose aspect ratio differs from the logical desktop
are rejected. The desktop controls pointer inclusion (the Screenshot
interface has no cursor option), permission dialogs and capture flashes.

Overlay protocol selection is independent of capture: use layer-shell when
the compositor advertises `zwlr_layer_shell_v1` (including supported KDE
versions). Each output then gets its own overlay, with cross-output dragging.
Without layer-shell, open just one fullscreen xdg-toplevel.
GPUI 0.3.10 does not forward the requested output to `set_fullscreen`, so
the compositor chooses its placement. `Window::display()` tracks the
actual surface-enter output; the overlay maps input in that output's
coordinates and, in frozen mode, switches to its already-prepared frame.
Selection is confined to that output. This is the GNOME multi-monitor
limitation, not a silent crop of the first output.

Compositor animations and fullscreen focus policy cannot be overridden.
Esc/Enter use the same selection handlers as native capture. No GPU
prewarm window is opened on the fullscreen path, avoiding a visible flash.
`QUIKSCOPE_DEBUG_NO_LAYER_SHELL=1` forces this path for diagnostics/tests.
Screen/geometry capture still covers every output, independent of overlay
placement.

Clipboard prefers Wayland data-control. When it is unavailable but DISPLAY
is set, the existing X11 SelectionOwner serves CLIPBOARD over Xwayland;
GNOME normally bridges this selection to Wayland clients. With neither
protocol, clipboard is unavailable and the reason is logged.

Files are deliberately retained, even under `/tmp`: the portal does not
grant ownership or specify cleanup. [KDE's implementation][kde-screenshot]
saves `Pictures/Screenshot_*.png`; [GNOME's implementation][gnome-screenshot]
asks Shell to create a named screenshot. Document-portal paths are not
necessarily disposable files either.

The faster KWin ScreenShot2 API was considered, not selected: [Plasma 6.5
checks restricted-interface authorization][kwin-screenshot] for the caller
PID. Newer KWin development sources remove that check, but that is not a
portable contract across deployed desktops. No permission bypass or
desktop-specific window-query service is required; `Desktop` returns
"unknown", so there is no active-window capture or snapping.

[portal-screenshot]: https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Screenshot.html
[kde-screenshot]: https://github.com/KDE/xdg-desktop-portal-kde/blob/master/src/screenshot.cpp
[gnome-screenshot]: https://github.com/GNOME/xdg-desktop-portal-gnome/blob/main/src/screenshotdialog.c
[kwin-screenshot]: https://github.com/KDE/kwin/blob/Plasma/6.5/src/plugins/screenshot/screenshotdbusinterface2.cpp

See the [Screenshot portal specification][portal-screenshot] for its
permission and cursor limitations.


GPUI's executable manifest requests per-monitor-v2 DPI awareness; quikscope
verifies it (and sets it if absent) before creating any HWND. Its
snapshot coordinate unit is a physical desktop pixel (scale = 1), preserving
the virtual desktop lattice at mixed DPI, including negative origins.
GPUI 0.3.10 instead divides each monitor's global origin by its own scale;
using those origins for composition would introduce gaps/overlaps. The
Windows monitor placement maps physical bounds to GPUI DIPs; the shared overlay
viewport mapping converts pointer positions and selection highlights between
physical pixels and surface DIPs for both X11 and Windows.
Thus cross-monitor crops and DWM window bounds retain native pixels.
This coordinate mapping is implemented but has not yet been verified with
multiple monitors or mixed DPI on a native Windows desktop.


All DXGI COM/GDI resources stay on one capture thread. Duplication, device,
staging texture, DC and DIB allocations survive requests; timeout reuses a
valid last frame, while access loss discards duplication resources for
recreation. Unsupported duplication falls back to GDI. Cursor-free captures
use GDI when DXGI reports an already-embedded pointer; requested cursors are
drawn with their actual hotspot. The hidden shell window owns the tray,
registered hotkeys and delayed clipboard formats. PNG encoding starts once
with `PendingPng`, and WM_RENDERFORMAT serves those same encoded bytes.
The Windows IPC transport is AF_UNIX (Windows 10 1803+), under the current
user's LocalAppData directory and its inherited ACL, retaining the same
one-line protocol, liveness probe and second-daemon refusal.

## Capture path

1. `wlr-screencopy` frames for all outputs are requested at once. The
   compositor fills reused memfd buffers.
2. Each buffer becomes a BGRA `Image`: `XRGB8888`/`ARGB8888` are already BGRA
   in memory on little-endian machines, so conversion is a copy that forces
   alpha to 255. `XBGR`, and 10-bit formats are converted. `y_invert` and the
   output transform are undone so the image matches what the user sees
   (verified against `grim` on rotated and scaled outputs in the e2e suite).
3. A `Snapshot` holds every output as `Arc<Image>` plus its logical
   rectangle. `Snapshot::compose(rect)` builds any logical rectangle at the
   highest scale among the outputs it touches; a whole-output request returns
   the same `Arc` without copying.

## Region selection

With `freeze = true` (the default), selection shows a frozen screen, and the
final crop comes from that snapshot, not from the dimmed overlay. Each output
gets a full-screen `wlr-layer-shell` surface on the overlay layer with
exclusive keyboard focus, painting the frozen image from GPU tiles that were
built on the background pool while the window list was fetched. All overlays
share one `Selection` entity in global logical coordinates; because Wayland
keeps delivering motion to the surface that received the button press, a drag
can cross outputs.

Creating a window costs about as much as the capture itself (~12 ms each,
mostly surface and swapchain setup), so on layer-shell the two overlap: the
capture is started first, then one overlay per display opens *transparent*
and ignoring input while the compositor produces the frame. A frame that
happens to include the new surface therefore shows the desktop unchanged.
When the snapshot arrives each overlay receives its output's tiles and
starts dimming; if an output has no early overlay (hotplug during the
capture) they close and the regular path opens fresh ones. X11 and Windows
popups would capture as black without a compositor and fullscreen toplevels
animate in, so those open after the capture as before. Frozen portal captures
never get early overlays, even on layer-shell (KDE): the desktop may show a
permission dialog first, and an overlay's exclusive keyboard would keep the
user from answering it.

`quikscope pick` (and the editor color picker's eyedropper) reuses this frozen
path with a pick-mode `Selection`: the capture leaves out the pointer, the
overlays show the frames undimmed with a magnifier, and each overlay keeps
its output's `Arc<Image>` so a click maps the surface position to the
physical pixel and returns its color instead of a rectangle.

With `freeze = false` (CLI `--no-freeze`, IPC `no-freeze`), overlays use output
geometry without taking an initial screenshot. They paint translucent dimming
outside the selected rectangle and leave a transparent hole inside. Selection
gestures and window snapping are shared with frozen mode. Once selection
finishes, all overlays close before capture; the resulting new snapshot is
cropped to the selected rectangle. Cancellation takes no screenshot. Fixed
geometry and noninteractive modes bypass this choice.

Closing a GPUI window alone is not a compositor teardown barrier:

- Wayland records GPUI's raw display before closing and, after GPUI has
  destroyed the surfaces, performs a private-queue roundtrip on that **same**
  borrowed display. The compositor has therefore processed surface destruction
  before the separate capture connection requests screencopy or a portal
  screenshot. Native screencopy supplies the next rendered frame.
- X11 subscribes to overlay `DestroyNotify` before closing, waits for
  destruction, then waits for an X Present vblank barrier before capture.
  Live selection requires a compositor, an alpha visual and Present; without
  them it logs the limitation and falls back to the existing frozen path.
- Windows synchronously hides the overlay HWNDs (GPUI defers native window
  destruction), then uses `DwmFlush` before capture.
  Frozen and live selection were verified natively on one Windows 11 monitor
  at 96 DPI; Windows 10, mixed DPI and multiple monitors remain unverified.

Portal live captures ask permission only after selection and teardown, so the
overlays cannot intercept the permission dialog's keyboard input. Without
layer-shell (normally GNOME), selection remains confined to the output where
the fullscreen window lands; transparency depends on the compositor allowing
transparent fullscreen windows.

The interactive selection claim remains held until the live capture finishes,
including any portal permission dialog. Another selector cannot steal the
keyboard or contaminate that pending capture with new dimming overlays.


## Shots, documents and the editor

A `Shot` is an immutable capture (`Arc<Image>`, scale, time). Its GPU tiles
(`RenderImage`s of at most 4096² — every GPU supports that size) are built
once on a background thread right after capture, so opening the editor only
uploads textures.

A `Document` holds annotation shapes over a shot plus a crop and an undo
history. Shapes are immutable `Arc<Shape>`s; edits replace them, so undo
snapshots are vectors of pointers and each shape caches its render
primitives for its whole life. Documents live in the daemon keyed by shot,
which is why closing and reopening the editor keeps annotations.

The editor keeps selection membership in an ordered set of shape IDs.
Document batch operations checkpoint once per user action; live drag and
color-picker updates reuse that snapshot, so undo never exposes a partially
moved or recolored group. Gesture membership is immutable and shared across
pointer events. Marquee selection uses full shape bounds rather than
intersection, and stacking commands preserve relative order within both
the selection and the remaining shapes.

### One geometry, two renderers

`render::shape_prims` turns a shape into primitives in image space:

- every stroke is converted to its **filled outline** with tiny-skia's
  stroker (round caps and joins);
- text is shaped and emitted as glyph outlines (see below);
- pixelation is a precomputed patch of mosaic pixels;
- colour emoji glyphs are bitmaps (PNG strikes) resampled once to the size
  they are drawn at and placed as images.

Text layout (`text.rs`): each line is split into bidi runs in visual order
(`unicode-bidi`), each run into items that share a face and a script, and
each item is shaped with `rustybuzz` (kerning, ligatures, Arabic joining,
Indic conjuncts; neighbouring text is passed as context so joins survive
face changes). Faces are chosen per grapheme: the chosen font, then the
bundled Inter, then any installed font that has every visible char of the
grapheme. `fonts` indexes system fonts with `fontdb` on a background thread
at daemon start; the fallback search ranks Noto families first, prefers colour
emoji fonts for emoji, and for Han picks the CJK variant (JP, KR, SC, TC,
HK) the text itself implies (kana, hangul) or else the locale's, else
Japanese. Results are cached per grapheme. Variable fonts get their `wght`
(and `opsz`) axes set for both shaping and outlines; missing italics are
sheared.

### Text input and input methods

While the text tool is editing, the editor registers a GPUI
`EntityInputHandler`. That is what makes GPUI's Wayland backend enable
`text-input-v3`, so input methods (fcitx5, IBus) compose into the text:
pre-edit is shown underlined at the caret, the candidate window is anchored
there (`bounds_for_range`), and commits replace the pre-edit. When no text is
being edited the editor registers no handler, so the input method is off and
single-key tool shortcuts reach the editor. The text buffer
(`ui/edit_buffer.rs`) keeps byte offsets, converts the platform's UTF-16
ranges, and moves and deletes by grapheme. The font menu's family search
field is a text input too; number and hex fields refuse text input so an IME
cannot turn digits full-width.

The editor fills those outlines with GPUI paths (non-zero winding, 4× MSAA);
the exporter fills the identical outlines with tiny-skia. Neither renderer's
own stroker or text system is involved, so the saved PNG matches the editor
(the `editor_render` test compares them pixel by pixel). The exporter draws
straight into the BGRA buffer by giving tiny-skia colors with red and blue
swapped, so there is no conversion pass in either direction.

## Recording

```mermaid
flowchart LR
    req[record video] --> ctl[daemon/record.rs\none recording slot]
    ctl -- live selection --> overlay[overlays]
    ctl -- RecordTarget --> src[ScreenRecorder::open\nplatform VideoSource]
    src -- Frame --> sess[media::record::Session]
    aud[media::audio sources] -- AudioChunk --> sess
    sess -- video --> venc[convert::to_yuv420 →\ncodec::h264_encoder]
    sess -- audio --> aenc[mix::Mixer → codec::opus]
    venc --> mux[container::create_muxer]
    aenc --> mux
    mux --> file[(MP4 / MOV / MKV)]
    sess -- animation --> master[(anim::master\nlossless, cache)]
    master -- stop --> anim[anim::create_encoder] --> afile[(GIF / WebP / AVIF / JXL)]
    ctl -- stop --> fin[clipboard files\nnotification\ntray]
```

`src/daemon/record.rs` holds a single recording slot (idle, starting,
active, stopping), so the same request toggles and a second key press during
selection or the final encode is refused rather than starting a parallel
recording. A recording covers one output: the platform's `selection_outputs`
picks the output holding most of the selection, and the region is clipped to
it, because outputs of different scales have no common pixel size.

Platform sources (`ScreenRecorder::open`) deliver CPU `media::Frame`s
(`Frame::from_pixels`, 4-byte pixels in the order the source produced them):

| Backend | Frames |
| --- | --- |
| `wlroots` | A `quikscope __record-helper` child process captures `capture_output_region` into GBM DMA-BUFs with a modifier both the compositor and Vulkan accept, maps them to Vulkan and downloads them (NVIDIA cannot render into LINEAR buffers; linear DRM buffers are read directly) into a shared-memory ring the daemon wraps as frames without copying. The child exists because NVIDIA's driver imported some buffer sizes as all-zero images once GPUI's Vulkan renderer lived in the same process. DMA-BUF is used whenever the compositor offers it; shared-memory capture is only the logged fallback for compositors that offer none, because Hyprland reads SHM captures back synchronously on its main thread, which makes games stutter. |
| `x11` | MIT-SHM GetImage at the frame rate. |
| `portal` | ScreenCast portal + PipeWire, cropped on the CPU; the restore token is kept in `$XDG_STATE_HOME/quikscope`. |
| `windows` | DXGI Desktop Duplication read back to the CPU; rotated outputs are turned to desktop orientation and the pointer is composed from DXGI's pointer shapes. |

`media::record::Session` timestamps frames and audio chunks against one
clock (pauses excluded) with one presentation origin for every track, and
encodes on its own threads. A tick without a new frame is skipped rather than
encoded as a duplicate, so the files are variable frame rate. Odd capture
sizes are cropped to the even rectangle at their top left; a capture larger
than any H.264 encoder accepts is scaled down instead, with a warning.
Fragmented MP4 and MKV recordings stream through the H.264 encoder and the
muxer straight into the reserved output (each fragment leaves a playable
file), classic MP4 and MOV into its partial file; animations are written to a
lossless master in the cache and encoded into the chosen format on stop,
reporting progress, which `record status` and the tray show and a cancel
stops. The master stays so the
annotator can re-export without generation loss. The [media engine](#media-engine)
section describes the codecs and containers.

Outlines are drawn by the platform outside the region and excluded from
capture: a click-through layer-shell surface on wlroots, override-redirect
strips on X11 and `WDA_EXCLUDEFROMCAPTURE` windows on Windows.

## Media engine

`src/media` encodes, decodes and muxes everything itself; there is no FFmpeg.
C and C++ code is limited to libraries the crates build from source
(OpenH264, libopus, libwebp, zstd, the Vulkan Memory Allocator).

| Module | Role |
| --- | --- |
| `mod.rs`, `frame.rs` | `Frame`, `VideoSource`, `AudioSource`, output formats and the encoder preference. |
| `audio/` | Recording audio devices: PulseAudio's API on Linux (served by PipeWire on current desktops), WASAPI (loopback for outputs) on Windows. |
| `convert.rs` | 4-byte RGB ↔ YUV 4:2:0, BT.709 limited range (`yuv`, SIMD), and BGRA scaling (`fast_image_resize`). |
| `codec/` | `h264_vulkan` (Vulkan Video through `gpu-video`, NV12; the device exists only while an encoder or decoder does), `h264_soft` (OpenH264: Constrained Baseline encoding, decoding up to High profile with CABAC and B-frames), `opus` (every audio track we write), `aac` (AAC-LC decoding for third-party files, symphonia), `resample` (rubato). `h264_encoder`/`h264_decoder` try Vulkan Video first unless the preference is software, then OpenH264. Our encoders write no B-frames. |
| `container/` | `mp4`: MP4 and MOV boxes through `mp4-atom` with our own muxer and demuxer; fragmented MP4 by default (`record.mp4_fragmented = false` fragments while recording, then rewrites a classic fast-start file at finish), MOV always classic. `mkv`: `mkv-element` for writing (one cluster per keyframe) and `matroska-demuxer` for reading, whose cue-based seek is not trusted. |
| `anim/` | GIF (`gif` + `quantette`: a palette per frame, Floyd–Steinberg, index 255 transparent), WebP (`webp-animation` over a static libwebp, lossy by default; `image-webp` to read), AVIF (`rav1e` writes AVIS with `mp4-atom` boxes; `rav1d` reads), JPEG XL (a vendored `zune-jpegxl` fork writes lossless animations; `jxl-oxide` reads), APNG reading (`png`), and `master`, the lossless recording master (zstd-compressed frames with a keyframe index for seeking). |
| `mix.rs` | Mixes audio sources to 48 kHz stereo. Each source is placed by its running sample count from its first chunk and re-anchored only when it drifts more than about 40 ms, so chunk jitter never overlaps or gaps the sound. |
| `record.rs` | Recording sessions (above). |
| `export.rs` | Re-encoding for the video editor and annotator: trim, mute, video ↔ animation conversion and annotation compositing. |
| `decode.rs` | Probing, playback and seeking for the video editor: demuxer + H.264 and audio decoders + `convert::to_bgra`. |

Conventions shared by every codec and container: H.264 crosses the codec and
container APIs as Annex-B access units with SPS/PPS before every keyframe
(muxers build `avcC` and convert to length-prefixed samples, demuxers convert
back), so a decoder can start at any keyframe. Timestamps are relative to the
file's presentation start, shared by all tracks, so an audio track that starts
later than the picture stays in sync. Animation frames carry their own
durations end to end; encoders quantize them only to their format's precision
(10 ms for GIF). Video files must be progressive 8-bit 4:2:0 H.264; HEVC, VP9
and AV1 are not supported.

Every file the media code writes (recordings, exports, clipboard copies) goes
through `output::OutputFile`: a partial file per writer, published with one
rename, and an exclusive reservation of new names so concurrent writers that
derive the same name end up with distinct files.

## Persistence

- The latest capture is written uncompressed to
  `$XDG_CACHE_HOME/quikscope/last.raw` (20-byte header + BGRA) in the
  background; `open-last` after a daemon restart reads it in one `read`
  without decoding. Without it, the newest image in the screenshots folder
  is opened.
- Saves are atomic and never collide: `output::OutputFile` writes a partial
  file per writer (`<name>.<pid>.<n>.part`), renames it into place and
  reserves new names exclusively.
- `ui::jobs` registers every write in flight (recording saves, exports,
  editor saves, a recording starting). `quit` refuses new ones and awaits
  them before the process exits.
- A master whose final animation was never saved carries a `.pending` mark
  naming its output. At start-up the daemon moves marked masters of daemons
  that are gone into its own directory and opens them as shots, publishes
  dead writers' partial recordings that play, removes empty partials and
  their placeholders, then deletes the remaining unheld master directories.
- The daemon keeps the last eight shots and their documents in memory, and
  the paths of recent videos so a notification click can reopen them.

## Settings and onboarding

The daemon keeps one settings-window handle; command-line IPC, the editor's
`OpenSettings` action and Windows tray request all open or focus that handle.
The view uses the editor theme and reusable single-line fields. Typed values
are written through `config::persist_values`, which parses the original,
edits the relevant TOML paths with `toml_edit`, validates the entire result,
and atomically replaces the file while retaining comments and decorations.
Invalid original files or edits remain untouched and display an inline error.
Config writes from UI and notification IPC run on the app thread to serialize
their read/edit/replace transactions.

After a successful edit, the daemon swaps its `Arc<Config>` and the
`EditorServices` config together, updating capture notification state and
Windows hotkey registrations. Existing editor styles are not rewritten.
An open settings view refreshes after the originating entity update ends,
so IPC/tray changes cannot leave stale toggle state or re-enter a borrowed
view. Dirty field text is retained; Escape discards it back to the saved value.
Missing config files trigger a short welcome form; its choices preview in
memory, and Done or native close commits them. Creating the validated config
file is the onboarding marker, not a second flag or state file.

## IPC

See [ipc.md](ipc.md).
