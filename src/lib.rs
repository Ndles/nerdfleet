//! nerdfleet: watch a fleet of machines from one terminal.
//!
//! One binary is both the viewer you look at and the agent it starts on
//! each host over SSH; this library holds both halves and what they share.

pub mod agent;
pub mod config;
pub mod model;
pub mod paths;
pub mod proto;
pub mod text;
pub mod conn;
pub mod remote;
pub mod viewer;
