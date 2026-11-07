# Security policy

## Reporting a vulnerability

Do not disclose vulnerabilities in public issues or pull requests. Use GitHub's
[private vulnerability reporting](https://github.com/qt715/quikscope/security/advisories/new)
to send a draft security advisory to the maintainers. Include affected versions,
OS/backend, impact and a minimal reproduction without private screenshots or
credentials. If private reporting is unavailable, request a private reporting
channel from the maintainer without publishing exploit details.

The latest release and the main development branch are in scope. Fixes for older
versions are not guaranteed; upgrade to the latest release when available.

## Trust model and privacy

quikscope runs with your user account's privileges. The local IPC socket is
restricted to your account (Unix mode 0600 and a private runtime directory;
Windows uses the per-user LocalAppData directory's ACL). **Other processes
running as your user are trusted.** IPC is not a sandbox or an authorization
boundary against same-user software. Such clients can request captures, open
files and control the daemon. Do not expose the socket over a network or relax
its directory permissions.

Screenshots, saved images, the last-capture cache and daemon logs may contain
sensitive information. Clipboard managers and notification services may retain
data after a capture. In particular, `--text` includes extracted text in capture
notifications, which can remain in notification history or appear on screen.
Use `--no-notify` for sensitive text; disable clipboard output with
`--no-clipboard` when needed. Portal-provided screenshot files are retained
because the portal does not transfer ownership to quikscope.

OCR loads a locally installed Tesseract library and language models. Only install
these from trusted sources. quikscope is not intended to process hostile files
inside a security sandbox; malformed input handling and unintended code loading
are nevertheless valid security reports. Never use pixelation as a substitute
for reviewing the exported image before sharing it.
