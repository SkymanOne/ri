#![doc = env!("CARGO_PKG_DESCRIPTION")]
//!
//! A pi file is handled as a document: an order-preserving [`serde_json::Value`]
//! that [`json`] writes back byte-identically. pi's key order depends on the code
//! path that built an object, so the typed structs here are views: they read
//! fields from a document and build new objects in pi's usual key order.
#![forbid(unsafe_code)]

pub mod auth;
pub mod classify;
pub mod collate;
pub mod config;
pub mod event;
pub mod js;
pub mod json;
pub mod message;
pub mod model;
pub mod models;
pub mod rpc;
pub mod session;
pub mod settings;

use serde::{Deserialize, Deserializer};

/// Deserializes a field that pi writes as a value, as `null`, or not at all: a
/// present key becomes `Some`, even when `null`. Combine with `#[serde(default)]`
/// for the absent case and `skip_serializing_if = "Option::is_none"` to omit it.
pub(crate) fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}
