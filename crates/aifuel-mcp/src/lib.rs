//! MCP interfaces supplied by AI Fuel.

pub mod execution;
pub mod gateway;
pub(crate) mod http;
pub mod monitoring;

pub use http::DEFAULT_PORT as MCP_HTTP_DEFAULT_PORT;
pub use monitoring::serve;
pub use monitoring::serve_http;
