//! ego-bridge shares one machine's ego lite browser with agents on other
//! machines. The host runs `ego-bridge serve`, an MCP server over Streamable
//! HTTP. Remote machines either call its tools directly from an MCP client or
//! run the bundled `ego-browser` client, which mirrors the local ego-browser
//! command line and moves files across transparently.

pub mod audit;
pub mod client;
pub mod config;
pub mod mirror;
pub mod preamble;
pub mod protocol;
pub mod runner;
pub mod scan;
pub mod server;
pub mod service;
pub mod tokens;
