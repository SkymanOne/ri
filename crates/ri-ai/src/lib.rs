#![doc = env!("CARGO_PKG_DESCRIPTION")]
#![forbid(unsafe_code)]

pub mod api;
pub mod catalog;
pub mod cost;
pub mod faux;
pub mod http;
pub mod json_parse;
pub mod schema;
pub mod sse;
pub mod stream;
pub mod thinking;
pub mod transcript;
pub mod validation;
