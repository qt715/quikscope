//! wlroots-protocol compositors (Hyprland, sway, river, labwc, …):
//! `wlr-screencopy` for pixels and `wlr-data-control` for the clipboard.

pub mod data_control;
pub mod screencopy;

pub use data_control::DataControl;
pub use screencopy::Screencopy;
