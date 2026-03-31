//! deskd library — exposes modules for integration testing.

pub mod domain;
pub mod ports;

pub mod agent;
pub mod bus;
pub mod config;
#[allow(dead_code)]
mod context;
pub mod message;
pub mod statemachine;
pub mod task;
pub mod tasklog;
