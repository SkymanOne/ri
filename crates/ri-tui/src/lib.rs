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
pub mod kill_ring;
pub mod lines;
pub mod markdown;
pub mod screen;
pub mod segment;
pub mod select_list;
pub mod terminal;
pub mod text;
pub mod text_input;
pub mod theme;
