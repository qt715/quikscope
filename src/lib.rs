//! quikscope: a fast screenshot daemon with a GPUI annotation editor.

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
compile_error!("quikscope supports Linux, Windows and macOS only");

pub mod cache;
pub mod capture;
pub mod clipboard;
pub mod color;
pub mod config;
pub mod daemon;
pub mod document;
pub mod export;
pub mod fonts;
pub mod geom;
pub mod ipc;
pub mod ocr;
pub mod platform;
pub mod raster;
pub mod render;
pub mod text;
pub mod ui;
