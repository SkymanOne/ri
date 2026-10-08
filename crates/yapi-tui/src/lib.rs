#![doc = env!("CARGO_PKG_DESCRIPTION")]
#![forbid(unsafe_code)]

pub mod ansi;
pub mod autocomplete;
pub mod color;
pub mod editor;
pub mod fuzzy;
pub mod input;
pub mod keybindings;
pub mod keys;
mod kill_ring;
pub mod lines;
pub mod markdown;
pub mod screen;
mod segment;
pub mod select_list;
pub mod settings_list;
pub mod terminal;
pub mod text;
pub mod text_input;
pub mod theme;

/// The styled text types the widgets render.
pub use ratatui_core;
