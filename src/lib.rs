pub mod agent;
pub mod client;
pub mod config;
pub mod daemon;
pub mod error;
pub mod executor;
pub mod mcp;
pub mod model;
pub mod onboarding;
pub mod protocol;
pub mod session;
pub mod store;
pub mod supervisor;
pub mod watch;

pub use error::{Error, Result};
