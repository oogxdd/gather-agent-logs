//! Collecting coding-agent sessions from every machine into one Postgres
//! database, and reading them back.
//!
//! - [`discovery`] finds the agents' log files on the current machine.
//! - [`transcript`] turns their records into metadata, messages, and readable
//!   transcripts.
//! - [`sync`] uploads new log bytes incrementally.
//! - [`db`] owns the schema and every query.
//! - [`hooks`] installs the agent-side triggers that keep uploads current.
//! - [`ui`] is the terminal picker over local and collected sessions.

pub mod config;
pub mod db;
pub mod discovery;
pub mod hooks;
pub mod model;
pub mod sync;
pub mod transcript;
pub mod ui;
