use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, info, warn};

use crate::message::{Envelope, Message};

type Tx = mpsc::UnboundedSender<Message>;

struct Client {
    name: String,
    tx: Tx,
    subscriptions: HashSet<String>,
}

struct BusState {
    clients: HashMap<String, Client>,
}

impl BusState {
    fn new() -> Self {
        Self {
            clients: HashMap::new(),
        }
    }

    fn list_clients(&self) -> Vec<String> {
        self.clients.keys().cloned().collect()
    }

    fn route(&self, msg: &Message) {
        let target = &msg.target;

        if target == "broadcast" {
            for client in self.clients.values() {
                if client.name != msg.source {
                    let _ = client.tx.send(msg.clone());
                }
            }
        } else if let Some(name) = target.strip_prefix("agent:") {
            if let Some(client) = self.clients.get(name) {
                let _ = client.tx.send(msg.clone());
            } else {
                warn!(target = %name, "no such agent on bus");
            }
        } else if target.starts_with("queue:") {
            for client in self.clients.values() {
                if client.subscriptions.contains(target) && client.name != msg.source {
                    let _ = client.tx.send(msg.clone());
                }
            }
        } else {
            // Subscription-based routing for reply:* and other custom targets.
            // Match clients whose subscriptions match the target (supports glob with *).
            let mut delivered = false;
            for client in self.clients.values() {
                if client.name != msg.source {
                    for sub in &client.subscriptions {
                        if sub == target
                            || (sub.ends_with('*') && target.starts_with(&sub[..sub.len() - 1]))
                        {
                            let _ = client.tx.send(msg.clone());
                            delivered = true;
                            break;
                        }
                    }
                }
            }
            if delivered {
                debug!(target = %target, "delivered via pattern match");
            } else {
                warn!(target = %target, "no subscriber for target");
            }
        }
    }
}

pub async fn serve(socket_path: &str) -> Result<()> {
    let path = Path::new(socket_path);

    // Clean up stale socket
    if path.exists() {
        std::fs::remove_file(path).ok();
    }

    let listener = UnixListener::bind(path).context("Failed to bind Unix socket")?;

    // Make socket world-accessible so container users (non-root) can connect
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777))?;
    }

    info!(socket = %socket_path, "bus listening");

    let state = Arc::new(RwLock::new(BusState::new()));

    loop {
        let (stream, _) = listener.accept().await?;
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, state).await {
                warn!(error = %e, "connection error");
            }
        });
    }
}

async fn handle_connection(stream: UnixStream, state: Arc<RwLock<BusState>>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    // First message must be REGISTER
    let first_line = lines
        .next_line()
        .await?
        .context("connection closed before register")?;

    let envelope: Envelope =
        serde_json::from_str(&first_line).context("invalid register message")?;

    let (name, subscriptions) = match envelope {
        Envelope::Register(reg) => (reg.name, reg.subscriptions.into_iter().collect()),
        Envelope::Message(_) | Envelope::List => {
            anyhow::bail!("first message must be register");
        }
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    info!(client = %name, "client registered");

    {
        let mut bus = state.write().await;
        bus.clients.insert(
            name.clone(),
            Client {
                name: name.clone(),
                tx,
                subscriptions,
            },
        );
    }

    // Writer task — sends messages from channel to the socket
    let writer_handle = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let mut line = serde_json::to_string(&msg).unwrap_or_default();
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    // Reader loop — reads messages from client and routes them
    while let Some(line) = lines.next_line().await? {
        if line.is_empty() {
            continue;
        }

        let envelope: Envelope = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(e) => {
                warn!(client = %name, error = %e, "invalid message");
                continue;
            }
        };

        match envelope {
            Envelope::Message(msg) => {
                debug!(from = %msg.source, to = %msg.target, "routing message");
                let bus = state.read().await;
                bus.route(&msg);
            }
            Envelope::Register(_) => {
                warn!(client = %name, "ignoring duplicate register");
            }
            Envelope::List => {
                let bus = state.read().await;
                let clients = bus.list_clients();
                if let Some(client) = bus.clients.get(&name) {
                    let resp = Message {
                        id: "list-response".to_string(),
                        source: "bus".to_string(),
                        target: name.clone(),
                        payload: serde_json::json!({"type": "list_response", "clients": clients}),
                        reply_to: None,
                        metadata: crate::message::Metadata::default(),
                    };
                    let _ = client.tx.send(resp);
                }
            }
        }
    }

    // Client disconnected
    info!(client = %name, "client disconnected");
    {
        let mut bus = state.write().await;
        bus.clients.remove(&name);
    }
    writer_handle.abort();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Metadata;
    use std::collections::HashSet;

    fn make_bus() -> BusState {
        BusState::new()
    }

    fn register_client(
        bus: &mut BusState,
        name: &str,
        subs: Vec<&str>,
    ) -> mpsc::UnboundedReceiver<Message> {
        let (tx, rx) = mpsc::unbounded_channel();
        bus.clients.insert(
            name.to_string(),
            Client {
                name: name.to_string(),
                tx,
                subscriptions: subs.into_iter().map(String::from).collect::<HashSet<_>>(),
            },
        );
        rx
    }

    fn make_msg(source: &str, target: &str) -> Message {
        Message {
            id: "test-id".to_string(),
            source: source.to_string(),
            target: target.to_string(),
            payload: serde_json::json!({"result": "hello"}),
            reply_to: None,
            metadata: Metadata::default(),
        }
    }

    #[tokio::test]
    async fn test_broadcast_routing() {
        let mut bus = make_bus();
        let mut rx_a = register_client(&mut bus, "alice", vec![]);
        let mut rx_b = register_client(&mut bus, "bob", vec![]);

        let msg = make_msg("alice", "broadcast");
        bus.route(&msg);

        // alice should NOT receive (sender excluded)
        assert!(rx_a.try_recv().is_err());
        // bob should receive
        assert!(rx_b.try_recv().is_ok());
    }

    #[tokio::test]
    async fn test_agent_direct_routing() {
        let mut bus = make_bus();
        let mut rx_a = register_client(&mut bus, "alice", vec![]);
        let mut rx_b = register_client(&mut bus, "bob", vec![]);

        let msg = make_msg("bob", "agent:alice");
        bus.route(&msg);

        assert!(rx_a.try_recv().is_ok());
        assert!(rx_b.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_queue_routing() {
        let mut bus = make_bus();
        let mut rx_a = register_client(&mut bus, "worker1", vec!["queue:tasks"]);
        let mut rx_b = register_client(&mut bus, "worker2", vec!["queue:tasks"]);
        let mut rx_c = register_client(&mut bus, "sender", vec![]);

        let msg = make_msg("sender", "queue:tasks");
        bus.route(&msg);

        assert!(rx_a.try_recv().is_ok());
        assert!(rx_b.try_recv().is_ok());
        assert!(rx_c.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_subscription_glob_routing() {
        // This is the core bug scenario: telegram adapter subscribes to "telegram:*",
        // agent sends reply to "telegram:-123456", bus should deliver via glob match.
        let mut bus = make_bus();
        let mut rx_telegram = register_client(&mut bus, "telegram-adapter", vec!["telegram:*"]);
        let mut rx_agent = register_client(&mut bus, "agent1", vec!["agent:agent1", "queue:tasks"]);

        let msg = make_msg("agent1", "telegram:-123456");
        bus.route(&msg);

        // telegram adapter should receive via glob match
        let received = rx_telegram.try_recv();
        assert!(
            received.is_ok(),
            "telegram adapter should receive message targeted to telegram:-123456"
        );
        let received_msg = received.unwrap();
        assert_eq!(received_msg.target, "telegram:-123456");

        // agent should NOT receive
        assert!(rx_agent.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_subscription_exact_match() {
        let mut bus = make_bus();
        let mut rx = register_client(&mut bus, "listener", vec!["custom:exact-target"]);

        let msg = make_msg("sender", "custom:exact-target");
        bus.route(&msg);

        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn test_subscription_no_self_delivery() {
        // A client subscribed to "telegram:*" should NOT receive its own messages
        let mut bus = make_bus();
        let mut rx = register_client(&mut bus, "telegram-adapter", vec!["telegram:*"]);

        let msg = make_msg("telegram-adapter", "telegram:-123");
        bus.route(&msg);

        assert!(
            rx.try_recv().is_err(),
            "should not deliver to sender even if subscription matches"
        );
    }

    #[tokio::test]
    async fn test_no_subscriber_for_target() {
        let mut bus = make_bus();
        let mut rx = register_client(&mut bus, "alice", vec!["other:*"]);

        let msg = make_msg("bob", "telegram:-123");
        bus.route(&msg);

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_telegram_out_glob_routing() {
        // telegram.out:* should match telegram.out:-123
        let mut bus = make_bus();
        let mut rx_tg = register_client(&mut bus, "telegram-adapter", vec!["telegram.out:*"]);
        let _rx_other = register_client(&mut bus, "other-client", vec![]);

        let msg = make_msg("agent1", "telegram.out:-123");
        bus.route(&msg);

        let received = rx_tg.try_recv();
        assert!(
            received.is_ok(),
            "telegram.out:* should match telegram.out:-123"
        );
        assert_eq!(received.unwrap().target, "telegram.out:-123");
    }

    #[tokio::test]
    async fn test_glob_does_not_cross_match() {
        // telegram.out:-123 should NOT be delivered to github:* subscriber
        let mut bus = make_bus();
        let mut rx_gh = register_client(&mut bus, "github-adapter", vec!["github:*"]);
        let _rx_sender = register_client(&mut bus, "agent1", vec![]);

        let msg = make_msg("agent1", "telegram.out:-123");
        bus.route(&msg);

        assert!(
            rx_gh.try_recv().is_err(),
            "github:* should not match telegram.out:-123"
        );
    }

    #[tokio::test]
    async fn test_glob_matches_multiple_targets() {
        // telegram.out:* should match both telegram.out:123 and telegram.out:-456
        let mut bus = make_bus();
        let mut rx_tg = register_client(&mut bus, "telegram-adapter", vec!["telegram.out:*"]);

        let msg1 = make_msg("agent1", "telegram.out:123");
        bus.route(&msg1);
        let msg2 = make_msg("agent1", "telegram.out:-456");
        bus.route(&msg2);

        let first = rx_tg.try_recv();
        assert!(
            first.is_ok(),
            "telegram.out:* should match telegram.out:123"
        );
        assert_eq!(first.unwrap().target, "telegram.out:123");

        let second = rx_tg.try_recv();
        assert!(
            second.is_ok(),
            "telegram.out:* should match telegram.out:-456"
        );
        assert_eq!(second.unwrap().target, "telegram.out:-456");
    }

    #[tokio::test]
    async fn test_unknown_target_no_panic() {
        // Message to a completely unknown target with no clients subscribed
        let mut bus = make_bus();
        let _rx = register_client(&mut bus, "alice", vec!["queue:tasks"]);

        // Should not panic — just silently drop
        let msg = make_msg("alice", "nonexistent:target");
        bus.route(&msg);

        // Empty bus — still should not panic
        let empty_bus = make_bus();
        let msg2 = make_msg("ghost", "agent:nobody");
        empty_bus.route(&msg2);
    }

    #[tokio::test]
    async fn test_broadcast_all_except_sender() {
        // Verify ALL clients except sender receive broadcast
        let mut bus = make_bus();
        let mut rx_a = register_client(&mut bus, "a", vec![]);
        let mut rx_b = register_client(&mut bus, "b", vec![]);
        let mut rx_c = register_client(&mut bus, "c", vec![]);
        let mut rx_sender = register_client(&mut bus, "sender", vec![]);

        let msg = make_msg("sender", "broadcast");
        bus.route(&msg);

        assert!(rx_a.try_recv().is_ok(), "client a should receive broadcast");
        assert!(rx_b.try_recv().is_ok(), "client b should receive broadcast");
        assert!(rx_c.try_recv().is_ok(), "client c should receive broadcast");
        assert!(
            rx_sender.try_recv().is_err(),
            "sender should not receive own broadcast"
        );
    }

    #[tokio::test]
    async fn test_queue_sender_exclusion() {
        // A client subscribed to a queue should not receive its own messages to that queue
        let mut bus = make_bus();
        let mut rx = register_client(&mut bus, "worker", vec!["queue:tasks"]);

        let msg = make_msg("worker", "queue:tasks");
        bus.route(&msg);

        assert!(
            rx.try_recv().is_err(),
            "sender should not receive own queue message"
        );
    }

    #[tokio::test]
    async fn test_agent_direct_to_dev() {
        // Explicit test: message to agent:dev delivered to client named "dev"
        let mut bus = make_bus();
        let mut rx_dev = register_client(&mut bus, "dev", vec![]);
        let mut rx_other = register_client(&mut bus, "other", vec![]);

        let msg = make_msg("cli", "agent:dev");
        bus.route(&msg);

        let received = rx_dev.try_recv();
        assert!(received.is_ok(), "agent:dev should deliver to client 'dev'");
        assert_eq!(received.unwrap().source, "cli");

        assert!(
            rx_other.try_recv().is_err(),
            "other clients should not receive agent:dev message"
        );
    }
}
