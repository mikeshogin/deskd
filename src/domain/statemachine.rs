//! State machine domain types.
//!
//! Pure data types — no I/O, no persistence logic.

use serde::{Deserialize, Serialize};

/// An instance of a state machine model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub id: String,
    pub model: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub state: String,
    pub assignee: String,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
    pub history: Vec<Transition>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// A recorded transition in the instance history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    pub from: String,
    pub to: String,
    pub trigger: String,
    pub timestamp: String,
    #[serde(default)]
    pub note: Option<String>,
}
