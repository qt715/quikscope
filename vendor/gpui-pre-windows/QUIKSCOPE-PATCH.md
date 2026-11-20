# gpui-pre-windows 0.3.10, patched

This directory is the crates.io release
[gpui-pre-windows 0.3.10](https://crates.io/crates/gpui-pre-windows/0.3.10),
a snapshot of Zed's `gpui_windows` crate at zed-industries/zed commit
`08cbb88a152c3751a43e853425a70e9f631a5918`. `Cargo.toml` uses it through
`[patch.crates-io]`. Apart from the change below, the files are the published
`.crate` contents, unmodified (Apache-2.0, see `LICENSE-APACHE`).

## The change

```diff
--- a/src/events.rs
+++ b/src/events.rs
@@ -335,14 +335,16 @@
         if let Some(callback) = callback {
             callback();
         }
+        // Untrack the window synchronously, while `handle` is still valid.
+        // Posting this left it tracked until the message loop ran again after
+        // `DestroyWindow`, so the VSync thread's `RedrawWindow` hit a dead handle.
         unsafe {
-            PostMessageW(
-                Some(self.platform_window_handle),
+            SendMessageW(
+                self.platform_window_handle,
                 WM_GPUI_CLOSE_ONE_WINDOW,
-                WPARAM(self.validation_number),
-                LPARAM(handle.0 as isize),
-            )
-            .log_err();
+                Some(WPARAM(self.validation_number)),
+                Some(LPARAM(handle.0 as isize)),
+            );
         }
         Some(0)
     }
```

## Why

On every vsync, the `VSyncProvider` thread calls
`RedrawWindow(hwnd).ok().log_err()` for each window in the platform's
`raw_window_handles` list (`src/platform.rs`, `begin_vsync_thread`). A window
left that list only when the main thread handled the `WM_GPUI_CLOSE_ONE_WINDOW`
that `WM_DESTROY` posts. That happens after `DestroyWindow` returns and the
foreground task that called it, and whatever else is queued, finishes. A vsync
in that gap made `RedrawWindow` fail on the destroyed handle. The daemon log
then showed `[ERROR] Invalid window handle. (0x80070578)` (logged from
`platform.rs:489`) for an occasional closed window.

Sending the message instead removes the window from the list while it is
being destroyed and its handle is still valid. The removal still runs in
`close_one_window`, under the list's existing write lock, so the VSync thread
cannot be iterating the list when it happens. This covers every way a window
is destroyed: GPUI's own `DestroyWindow` when a window is dropped, and Windows
destroying it natively. Removing the window in `WindowsWindow::drop` before
`DestroyWindow` would miss the native path.

`close_one_window` keeps its `unwrap` because each window is still removed
exactly once. It takes the same write lock on the main thread as before, so
lock ordering with the device-lost path is unchanged.

## Removing this patch

Drop this directory and the `[patch.crates-io]` entry once upstream (Zed's
`gpui_windows` or a later gpui-pre release) stops leaving a destroyed window
in the vsync list. Checked against gpui-pre-windows 0.3.13: still present
there.
