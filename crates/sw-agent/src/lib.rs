//! Sitewright agent: the tool-calling loop, its tools and the skills loader.
//!
//! The model never touches the filesystem, a shell or the network. It can only ask for
//! the tools in [`tools`], each of which is ordinary Rust with its own guardrails, and
//! every call is recorded in a replayable event stream.

pub mod events;
pub mod prompt;
pub mod runner;
pub mod tools;

pub use events::RunEvent;
pub use runner::{Agent, AgentError, ProviderSource, Run, RunRequest, RunStatus, Subscription};
