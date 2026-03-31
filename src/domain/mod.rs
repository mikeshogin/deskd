//! Domain layer — pure types and business rules, no I/O.
//!
//! This module depends only on `std` and `serde`. It must never import from
//! `infra`, `app`, `adapters`, or any other outer layer.

pub mod message;
pub mod statemachine;
pub mod task;
