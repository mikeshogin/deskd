use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Register {
    pub name: String,
    #[serde(default)]
    pub subscriptions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub source: String,
    pub target: String,
    pub payload: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub metadata: Metadata,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metadata {
    #[serde(default = "default_priority")]
    pub priority: u8,
}

fn default_priority() -> u8 {
    5
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Envelope {
    Register(Register),
    Message(Message),
    /// Query: list currently connected agents on this bus.
    List,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_envelope_register_deserialize() {
        let json = r#"{"type":"register","name":"telegram","subscriptions":["telegram:*"]}"#;
        let env: Envelope = serde_json::from_str(json).unwrap();
        match env {
            Envelope::Register(reg) => {
                assert_eq!(reg.name, "telegram");
                assert_eq!(reg.subscriptions, vec!["telegram:*"]);
            }
            _ => panic!("expected Register variant"),
        }
    }

    #[test]
    fn test_envelope_message_deserialize() {
        let json = r#"{"type":"message","id":"abc","source":"agent1","target":"telegram:-123","payload":{"result":"done"},"metadata":{"priority":5}}"#;
        let env: Envelope = serde_json::from_str(json).unwrap();
        match env {
            Envelope::Message(msg) => {
                assert_eq!(msg.id, "abc");
                assert_eq!(msg.source, "agent1");
                assert_eq!(msg.target, "telegram:-123");
                assert!(msg.reply_to.is_none());
            }
            _ => panic!("expected Message variant"),
        }
    }

    #[test]
    fn test_envelope_message_with_reply_to() {
        let json = r#"{"type":"message","id":"abc","source":"cli","target":"queue:tasks","payload":{"task":"hello"},"reply_to":"telegram:-123","metadata":{"priority":5}}"#;
        let env: Envelope = serde_json::from_str(json).unwrap();
        match env {
            Envelope::Message(msg) => {
                assert_eq!(msg.reply_to, Some("telegram:-123".to_string()));
            }
            _ => panic!("expected Message variant"),
        }
    }

    #[test]
    fn test_worker_reply_envelope_deserialize() {
        // Simulate what the worker sends back — manually constructed JSON
        let json = r#"{"type":"message","id":"reply-1","source":"agent1","target":"telegram:-123","payload":{"result":"task done","in_reply_to":"orig-1"},"metadata":{"priority":5}}"#;
        let env: Envelope = serde_json::from_str(json).unwrap();
        match env {
            Envelope::Message(msg) => {
                assert_eq!(msg.source, "agent1");
                assert_eq!(msg.target, "telegram:-123");
                assert_eq!(msg.payload["result"], "task done");
            }
            _ => panic!("expected Message variant"),
        }
    }
}
