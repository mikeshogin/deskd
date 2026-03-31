//! Task queue domain types.
//!
//! Pure data types — no I/O, no persistence logic.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    Active,
    Done,
    Failed,
    Cancelled,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Active => write!(f, "active"),
            Self::Done => write!(f, "done"),
            Self::Failed => write!(f, "failed"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// Criteria for matching tasks to workers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskCriteria {
    /// Required model (e.g. "claude-sonnet-4-6").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Required labels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
}

/// A task in the queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub description: String,
    pub status: TaskStatus,
    #[serde(default)]
    pub criteria: TaskCriteria,
    /// Agent assigned to the task (set when status -> active).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// Result text (set when status -> done).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Error message (set when status -> failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Who created the task.
    #[serde(default)]
    pub created_by: String,
    /// Linked state machine instance ID (set when task is created by SM dispatch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sm_instance_id: Option<String>,
}

/// Summary of the queue for status display.
pub struct QueueSummary {
    pub pending: usize,
    pub active: usize,
    pub done: usize,
    pub failed: usize,
}

/// Check if a task's criteria match a worker's capabilities.
pub fn matches_criteria(
    criteria: &TaskCriteria,
    agent_model: &str,
    agent_labels: &[String],
) -> bool {
    if let Some(ref required_model) = criteria.model
        && required_model != agent_model
    {
        return false;
    }
    for label in &criteria.labels {
        if !agent_labels.contains(label) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_matches_criteria() {
        assert!(matches_criteria(&TaskCriteria::default(), "any", &[]));
        assert!(matches_criteria(
            &TaskCriteria {
                model: Some("sonnet".into()),
                labels: vec![]
            },
            "sonnet",
            &[]
        ));
        assert!(!matches_criteria(
            &TaskCriteria {
                model: Some("sonnet".into()),
                labels: vec![]
            },
            "haiku",
            &[]
        ));
        assert!(matches_criteria(
            &TaskCriteria {
                model: None,
                labels: vec!["x".into()]
            },
            "any",
            &["x".into(), "y".into()]
        ));
        assert!(!matches_criteria(
            &TaskCriteria {
                model: None,
                labels: vec!["x".into()]
            },
            "any",
            &["y".into()]
        ));
    }
}
