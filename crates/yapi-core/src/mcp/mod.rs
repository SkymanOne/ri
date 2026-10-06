//! Model Context Protocol: a client for stdio and streamable HTTP servers
//! (pi-mcp), and the built-in extension that exposes their tools.

pub mod client;
pub mod config;
pub mod connection;
pub mod content;
pub mod extension;
pub mod http;
pub mod jsonrpc;
pub mod log;
pub mod oauth;
pub mod sign_in;
pub mod stdio;
pub mod tools;
pub mod transport;

pub use client::{ClientOptions, McpClient, RequestOptions, ResourceKind, Root, Tool};
pub use jsonrpc::McpError;
pub use transport::Transport;
