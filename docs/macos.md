# macOS (12.3 and newer)

quikscope uses native ScreenCaptureKit capture, a menu-bar agent, and a Unix
socket. The application bundle identifier is `io.github.qt715.quikscope`.
`LSUIElement=true` keeps the background agent out of the Dock. macOS 12.3 is
the deployment minimum; both Apple Silicon and Intel binaries can be bundled.

## Build and assemble

Build on macOS with Rust, Xcode Command Line Tools, and the macOS SDK:

```sh
MACOSX_DEPLOYMENT_TARGET=12.3 cargo build --locked --release
packaging/macos/assemble-app.sh target/release/quikscope dist/quikscope.app
```

The two arguments are optional and default to the paths above. The first may
also be a previously assembled universal executable. The script refuses to
replace an existing output, copies `assets/branding/quikscope.icns`, reads the
package version from Cargo.toml without executing the supplied binary, and
ad-hoc signs the resulting app. It includes the project MIT/Apache licenses,
Inter font and icon licenses, and root `THIRD-PARTY-LICENSES.html` when that
release-generated file is available. Ad-hoc signing is for local/CI use, not
Developer ID signing or notarization. Copy the finished app to `/Applications`
before granting permissions or enabling autostart; changing its location or
signature can invalidate permission grants.

The macOS GPUI dependencies must enable **`runtime_shaders`** and **`font-kit`**.
In gpui-pre-macos 0.3.10 these forward to gpui-pre-apple. `runtime_shaders`
compiles bundled Metal source at runtime instead of requiring a precompiled
metallib during the build. Without `font-kit`, MacPlatform installs
`NoopTextSystem` and renders no text. These are dependency features, not CLI
switches. Tesseract and extra language models are optional for OCR, not for
screen capture.

Run the executable inside the bundle for CLI commands:

```sh
APP=/Applications/quikscope.app/Contents/MacOS/quikscope
"$APP" daemon
# In another terminal:
"$APP" capture screen --save --no-notify
"$APP" settings
"$APP" open-last
"$APP" quit
```

Commands connect to the running daemon and start one when needed. `daemon`
runs in the foreground, so a terminal launch can be stopped with `quit`.
The settings command opens the native application's settings window. The
menu-bar menu also exposes settings, captures, and quit.

## Permissions and shortcuts

Allow quikscope in System Settings → Privacy & Security → Screen Recording
(the panel is named Screen & System Audio Recording on newer macOS releases).
On Monterey, use System Preferences → Security & Privacy → Privacy → Screen
Recording. Approve the system prompt and restart the daemon if macOS asks.
The bundle includes a screen-capture usage description. A denial must be
resolved through macOS permission settings; an ordinary app cannot silently
grant itself screen recording. Notification authorization is separate and is
only needed for capture notifications. `--no-notify` suppresses them.

Default global shortcuts:

| Action | Shortcut |
| --- | --- |
| Select region | Cmd+Shift+2 |
| Capture screen | Cmd+Shift+8 |
| Capture active window | Cmd+Shift+7 |
| Open last capture | Cmd+Shift+E |

Change bindings in settings or the `[hotkeys]` config table (`region`, `screen`,
`window`, `open_last`); an empty binding disables that shortcut. Shortcuts that
conflict with another app or system assignment must be changed.
The defaults also avoid macOS's [Touch Bar screenshot shortcut, Cmd+Shift+6](https://support.apple.com/en-euro/guide/mac-help/mh26782/mac).

## Files and login startup

| Purpose | Default path |
| --- | --- |
| Configuration | `~/Library/Application Support/quikscope/config.toml` |
| Cache | `~/Library/Caches/quikscope` |
| Unix IPC socket | `~/Library/Caches/quikscope/ipc.sock` |
| Saved screenshots | `~/Pictures/Screenshots` |

`QUIKSCOPE_SOCKET` overrides the socket path for a separate daemon. `save_dir`
in the config changes screenshot storage. Raw captures are saved with `--save`;
the editor's save command also writes screenshots. A missing configuration
file triggers first-run onboarding; an existing valid config uses its values
and fills omitted fields from defaults.

Manage the per-user login agent using the installed bundle executable:

```sh
"$APP" autostart on
"$APP" autostart status
"$APP" autostart off
```

Enable only after putting the app in its permanent location. Startup is
per-user, not a system-wide privileged service. Use `autostart off` before
moving or removing the bundle.

## CI and verification boundaries

The requested `macos-14` runner was replaced with `macos-15` because GitHub's
[current hosted-runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)
no longer lists macOS 14 among supported standard images. `macos-15` is a
supported ARM64 image. The deployment minimum remains 12.3 through
`MACOSX_DEPLOYMENT_TARGET=12.3`; CI does not run on macOS 12.3 and therefore does
not establish runtime compatibility with that oldest supported release.

The `macos` job in `.github/workflows/ci.yml` asserts `uname -m` is `arm64`.
It builds release with the lockfile for the native architecture, installs the
`x86_64-apple-darwin` Rust target and cross-builds an Intel release on the same
runner. Intel is compile-only coverage, not an Intel runtime test. Clippy over
all native targets treats warnings as errors, followed by `cargo test --locked`
and a required English OCR region test using Homebrew Tesseract, then the
bundled capture smoke on ARM64. GPUI 0.3.10's local registry sources
were inspected: `gpui-pre-platform/src/gpui_platform.rs` returns
`Some(MetalHeadlessRenderer::new())` on macOS, and gpui-pre-macos re-exports that
renderer from gpui-pre-apple. Unlike Windows, this backend supports offscreen
GPU rendering; no GPU suite is excluded merely because the OS is macOS.
Actual availability still depends on the runner exposing a working Metal device.

After assembly, `packaging/macos/ci-smoke.sh` starts the bundled daemon and runs
`capture screen --save --no-notify --no-edit`. It reads promised PNG and TIFF
from NSPasteboard in a separate Swift process and compares the decoded PNG
pixels with the saved capture. Before launching
it writes a complete configuration with explicit defaults into a temporary HOME
(only `save_dir` is redirected to the artifact directory), avoiding onboarding.
Builds and dependency caches retain the runner's normal HOME. The smoke daemon
and clients share the temporary HOME and an explicit private socket.

**Runner-only permission attempt:** the script refuses to run outside GitHub
Actions on macOS. Before isolating HOME, it attempts `sudo sqlite3` inserts in
`/Library/Application Support/com.apple.TCC/TCC.db`, table `access`, for service
`kTCCServiceScreenCapture`, both bundle client `io.github.qt715.quikscope`
(`client_type=0`) and the absolute bundled executable (`client_type=1`). Each
record requests `auth_value=2`, `auth_reason=4`, `auth_version=1`, includes the
ad-hoc signed designated requirement compiled by `csreq`, and sets the indirect
object to `UNUSED`. It does not disable SIP, alter security policy, or attempt
grants on a developer machine. TCC database access, schema differences, or the
runner's responsible-process identity may make these inserts ineffective.
The attempted records/errors are logged; a denied grant does **not** turn the
capture into a skipped or successful smoke. The capture must succeed and
produce a PNG readable by `sips` or the job fails.

PNG files, capture output, daemon logs, TCC diagnostics and smoke logs are
uploaded even on failure. A successful job also archives the ad-hoc signed app.
This is automation, not a claim of interactive verification: permission dialogs,
menu-bar interaction, notification clicks, region/window selection, multi-monitor
and Retina alignment, editor keyboard behavior, and login startup still require
an interactive Mac. The CI job is not an interactive desktop verification or
a runtime test of the oldest supported macOS release.
