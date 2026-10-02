//! Model Context Protocol: a client for stdio and streamable HTTP servers
//! (pi-mcp), and the built-in extension that exposes their tools.

pub mod client;
pub mod content;
pub mod http;
pub mod jsonrpc;
pub mod stdio;
pub mod transport;

pub use client::{ClientOptions, McpClient, RequestOptions, Root, Tool};
pub use jsonrpc::McpError;
pub use transport::Transport;
