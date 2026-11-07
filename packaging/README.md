# Release packages

The Packages workflow runs on `v*` tags or manual dispatch. Dispatch uploads
artifacts only; tags create a **draft**, never a published release. Ensure the
Cargo version, changelog version section and tag agree. All runs build Linux,
Windows and a universal macOS bundle using the landed native backend and app assembler.

## Cut a release

1. Run the CI gates and review `cargo deny --locked check` advisories.
2. Update Cargo.toml/Cargo.lock and CHANGELOG.md for the release version.
3. Dispatch Packages on the candidate branch. Download **quikscope-release**;
   verify SHA256SUMS, install and exercise each package before tagging.
4. With release-owner approval, push `v<VERSION>`. Review the generated draft,
   notes, checksums and platform assets before manually publishing it.

No workflow applies a distribution signature, notarizes the app, publishes an AUR package, or
changes repository settings. While the repository is private, release/source
URLs require GitHub access; an AUR submission is appropriate only after public
source releases are accessible.

## Licences and source

Install `cargo-about 0.8.4` with `cargo install cargo-about --locked --version
0.8.4`; run `packaging/licenses.sh`. about.toml enumerates **all four release
targets**, includes build/dev/transitive dependencies and uses Cargo.lock.
The generated THIRD-PARTY-LICENSES.html is committed and regenerated in every
release job. It includes MPL-2.0 source download instructions for the exact
unmodified crate versions and the complete Inter OFL and Lucide/Feather texts.
Every package ships this report plus the original asset and application
licences. Review changes to the dependency allowlist rather than accepting new
licences automatically. `cargo-deny 0.20.2` checks licences, security advisories,
registry sources and wildcard dependencies. Duplicate transitive versions warn;
six reviewed unmaintained-only advisories have individually documented exceptions
in deny.toml. New maintenance or vulnerability advisories fail CI.

## Linux

Ubuntu 24.04 builds **one** release executable used by both nFPM 2.43.4 formats.
Run `scripts/ci-deps-ubuntu.sh`, install `rpm dpkg-dev python3`, install nFPM,
build `cargo build --frozen --release --bin quikscope`, then run
`packaging/linux.sh [binary-path]`. The script requires branding PNGs and the
licence report and fails explicitly when these are missing.

`dpkg-shlibdeps` discovers Debian package/version dependencies; RPM's
`find-requires` discovers ELF sonames and versioned symbols. Explicit runtime
requirements cover xkbcommon and dynamically loaded Wayland/Vulkan.
Fontconfig and FreeType are build dependencies, not linked runtime dependencies;
fontdb reads font configuration itself. Tesseract/language data and
xdg-desktop-portal are recommendations,
not mandatory dependencies. No wl-clipboard dependency is needed: the daemon
owns the clipboard. Install a Vulkan ICD for the actual GPU (or Mesa software
rendering), and the appropriate portal backend when using GNOME/KDE.

The Ubuntu build has a glibc 2.39 floor. No Linux ARM64 package is shipped yet:
it would require a separately validated native dependency baseline and package
smoke matrix; adding an unchecked cross build would not be a cheap equivalent.

The release source archive includes the generated licence report. The workflow
sets PKGBUILD's SHA-256 to that archive's checksum and generates .SRCINFO with
makepkg, so the distributed Arch recipe is checksum-pinned. The checked-in
recipe is the release-generation input; do not submit its temporary SKIP checksum
to AUR. `prepare()` fetches the locked crates, `build()` uses `--frozen`, and
`check()` runs display-independent library unit tests with `--release`, reusing
the production dependency tree rather than compiling a second debug tree.
Release CI also removes the already-packaged Ubuntu build tree before Arch.
For local Arch builds,
place the matching source tarball next to the generated release PKGBUILD and
run `makepkg -si`; inspect `namcap PKGBUILD` and `namcap *.pkg.tar.zst`.
The recipe disables makepkg's injected C/C++ LTO (`!lto`): GCC LTO bytecode
for wayland-backend's C log trampoline cannot be linked by rustc.
Cargo's own release-profile LTO remains enabled. `!debug` avoids an empty split
debug package because the release profile strips its debug information.

Container smoke tests must use their native dependency resolver, not copy
libraries into the image: `apt install ./package.deb` on Debian trixie and
Ubuntu 24.04; `dnf install ./package.rpm` on Fedora; `zypper install` on
Tumbleweed. Then run `packaging/smoke-linux.sh` inside each disposable
container, with Xvfb installed for end-to-end X11 capture and opening the editor.
The smoke requires the original daemon PID to stay alive, checks IPC pings
through initial rendering, and rejects panic messages in its private log. This
also exercises bundled UI fonts on minimal images. No host display is used.
Arch validation should install the makepkg-produced package with pacman.

## Windows

`packaging/windows/installer.nsi` builds with NSIS 3 on Linux (`makensis`) and
Windows. Pass `-DVERSION=0.1.0 -DSOURCE=/absolute/repo` on Linux; Windows uses
`/D`. The binary defaults to target/release/quikscope.exe and can be overridden
with BINARY. OUTPUT can also be overridden. Missing branding ICO/report/binary
is a build failure, not a silent fallback.
The Windows runner passes Cargo's reported compiler-artifact executable path,
so custom target layouts do not rely on a guessed path. NSIS source-file paths
use Windows separators, which Linux makensis normalizes for its host filesystem.

The x86_64 MSVC target statically links the CRT via `.cargo/config.toml`, including
local builds and `cargo install --path .` run from the source checkout. If
overriding `RUSTFLAGS`, retain `-C target-feature=+crt-static`. The release runner
sets this explicitly (toolchain actions can override target flags), builds for
the explicit MSVC target, and rejects VCRUNTIME/MSVCP/api-ms-win-crt DLL imports
before packaging. No separate Visual C++ redistributable is required.

The installer writes only the current user's LocalAppData/Start menu/HKCU,
registers Installed Apps metadata, and delegates optional login startup to
`quikscope autostart on`. Upgrades stop the previous daemon and retain config;
uninstall invokes `quit` and `autostart off` before deleting installed files.
Known raw-image/log/socket cache files are removed; the cache directory is
removed only if empty. User configuration and saved screenshots remain untouched.
The optional startup section is off by default. `/S` installs/uninstalls
silently. Test both silent and interactive paths, upgrading over an existing
installation, daemon/tray startup, CLI capture, and uninstall cleanup. The
installer is unsigned; SmartScreen reputation is not bypassed automatically.
`packaging/windows/smoke.ps1 -Installer <absolute-installer-path>` exercises the
silent lifecycle in a disposable signed-in Windows environment using task-owned
`V:\qk-pkg` output. It refuses existing installs/config and schedules the daemon
in the interactive session; it does not depend on PrintScreen.

## macOS

The job cross-builds both Apple architectures on macOS, uses lipo, then
calls `packaging/macos/assemble-app.sh <universal-binary> <output.app>`.
That script owns Info.plist substitution, the bundle executable/icon and
licence placement. hdiutil wraps the app and Applications symlink into a UDZO
.dmg. No create-dmg dependency is needed. The assembler ad-hoc signs the bundle;
Developer ID signing and notarization remain release-owner operations. Both
backend architectures and the Gatekeeper experience require native runner/user
verification; a successful Linux packaging build does not validate macOS.
