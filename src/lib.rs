//! nerdfleet: watch a fleet of machines from one terminal.
//!
//! One binary is both the viewer you look at and the agent it starts on
//! each host over SSH; this library holds both halves and what they share.

pub mod add;
pub mod agent;
pub mod config;
pub mod conn;
pub mod history;
pub mod model;
pub mod paths;
pub mod proto;
pub mod remote;
pub mod resident_ctl;
pub mod text;
pub mod viewer;
