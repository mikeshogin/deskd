//! Functional test: schedule fires → post_to_bus → worker picks up (#121).
//!
//! Tests the full path that cron schedule parsing is already unit-tested for,
//! but NOT the actual bus delivery:
//! 1. Schedule fires (simulated — we replicate what fire_raw/post_to_bus does)
//! 2. Message arrives on the bus at the correct target
//! 3. Worker (subscribed to agent:<name>) receives the task
//!
//! Also tests that the schedule source labeling and payload format match
//! what the worker expects.

use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

fn temp_socket() -> String {
    format!(
        "/tmp/deskd-test-sched-{}.sock",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

async fn connect_and_register(
    socket: &str,
    name: &str,
    subscriptions: &[&str],
) -> (
    tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    tokio::net::unix::OwnedWriteHalf,
) {
    let stream = UnixStream::connect(socket).await.unwrap();
    let (reader, mut writer) = stream.into_split();

    let reg = serde_json::json!({
        "type": "register",
        "name": name,
        "subscriptions": subscriptions,
    });
    let mut line = serde_json::to_string(&reg).unwrap();
    line.push('\n');
    writer.write_all(line.as_bytes()).await.unwrap();

    (BufReader::new(reader).lines(), writer)
}

async fn read_one(
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    timeout_ms: u64,
) -> Option<serde_json::Value> {
    tokio::time::timeout(Duration::from_millis(timeout_ms), lines.next_line())
        .await
        .ok()?
        .ok()?
        .and_then(|l| serde_json::from_str(&l).ok())
}

/// Simulate what schedule::post_to_bus does: connect, register as schedule-<agent>,
/// send a task message to the target. This is the exact wire format the real
/// schedule module produces.
async fn simulate_schedule_fire(
    socket: &str,
    agent_name: &str,
    target: &str,
    task_text: &str,
    source_label: &str,
) {
    let mut stream = UnixStream::connect(socket).await.unwrap();

    // Register with a schedule-style name (same as real post_to_bus).
    let reg = serde_json::json!({
        "type": "register",
        "name": format!("schedule-{}-fire", agent_name),
        "subscriptions": [],
    });
    let mut line = serde_json::to_string(&reg).unwrap();
    line.push('\n');
    stream.write_all(line.as_bytes()).await.unwrap();

    // Send task message (same format as post_payload_to_stream).
    let msg = serde_json::json!({
        "type": "message",
        "id": uuid::Uuid::new_v4().to_string(),
        "source": format!("{}-{}", source_label, agent_name),
        "target": target,
        "payload": {"task": task_text},
        "metadata": {"priority": 5u8},
    });
    let mut msg_line = serde_json::to_string(&msg).unwrap();
    msg_line.push('\n');
    stream.write_all(msg_line.as_bytes()).await.unwrap();
}

/// Full path: schedule fires → message on bus → worker receives task.
#[tokio::test]
async fn test_schedule_fire_delivers_to_worker() {
    let socket = temp_socket();

    // Start bus.
    let sock = socket.clone();
    tokio::spawn(async move {
        deskd::bus::serve(&sock).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connect "worker" subscribed to agent:kira (the target of the schedule).
    let (mut worker_rx, _worker_tx) = connect_and_register(&socket, "kira", &["agent:kira"]).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Simulate schedule firing a raw task.
    simulate_schedule_fire(
        &socket,
        "kira",
        "agent:kira",
        "Check GitHub issues for updates",
        "schedule",
    )
    .await;

    // Worker should receive the scheduled task.
    let msg = read_one(&mut worker_rx, 1000).await;
    assert!(msg.is_some(), "worker should receive schedule-fired task");
    let msg = msg.unwrap();

    // Verify source format matches schedule convention.
    assert_eq!(
        msg["source"], "schedule-kira",
        "source should be schedule-<agent>"
    );
    assert_eq!(msg["target"], "agent:kira");

    // Verify payload has the task field (what worker.rs expects).
    let task = msg["payload"]["task"].as_str().unwrap();
    assert_eq!(task, "Check GitHub issues for updates");

    let _ = std::fs::remove_file(&socket);
}

/// Schedule fires with github metadata → worker receives task with repo/PR info.
#[tokio::test]
async fn test_schedule_github_poll_delivers_with_metadata() {
    let socket = temp_socket();

    let sock = socket.clone();
    tokio::spawn(async move {
        deskd::bus::serve(&sock).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (mut worker_rx, _worker_tx) = connect_and_register(&socket, "kira", &["agent:kira"]).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Simulate what fire_github_poll does: post_to_bus_with_github.
    {
        let mut stream = UnixStream::connect(&socket).await.unwrap();

        let reg = serde_json::json!({
            "type": "register",
            "name": "schedule-kira-ghpoll",
            "subscriptions": [],
        });
        let mut line = serde_json::to_string(&reg).unwrap();
        line.push('\n');
        stream.write_all(line.as_bytes()).await.unwrap();

        // Payload with github_repo and github_pr (same as post_to_bus_with_github).
        let msg = serde_json::json!({
            "type": "message",
            "id": uuid::Uuid::new_v4().to_string(),
            "source": "github_poll-kira",
            "target": "agent:kira",
            "payload": {
                "task": "[kgatilin/deskd#42] Fix bus routing for glob patterns",
                "github_repo": "kgatilin/deskd",
                "github_pr": 42,
            },
            "metadata": {"priority": 5u8},
        });
        let mut msg_line = serde_json::to_string(&msg).unwrap();
        msg_line.push('\n');
        stream.write_all(msg_line.as_bytes()).await.unwrap();
    }

    // Worker receives the task with github metadata.
    let msg = read_one(&mut worker_rx, 1000).await;
    assert!(
        msg.is_some(),
        "worker should receive github_poll schedule task"
    );
    let msg = msg.unwrap();

    assert_eq!(msg["source"], "github_poll-kira");
    assert_eq!(msg["payload"]["github_repo"], "kgatilin/deskd");
    assert_eq!(msg["payload"]["github_pr"], 42);

    let task = msg["payload"]["task"].as_str().unwrap();
    assert!(task.contains("deskd#42"));

    let _ = std::fs::remove_file(&socket);
}

/// Multiple schedules fire → worker receives all tasks in order.
#[tokio::test]
async fn test_multiple_schedule_fires_all_delivered() {
    let socket = temp_socket();

    let sock = socket.clone();
    tokio::spawn(async move {
        deskd::bus::serve(&sock).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (mut worker_rx, _worker_tx) = connect_and_register(&socket, "dev", &["agent:dev"]).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Fire 3 schedule events in sequence.
    for i in 1..=3 {
        simulate_schedule_fire(
            &socket,
            "dev",
            "agent:dev",
            &format!("Scheduled task #{}", i),
            "schedule",
        )
        .await;
        // Small delay to ensure ordering.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Worker should receive all 3.
    let mut received = Vec::new();
    for _ in 0..3 {
        if let Some(msg) = read_one(&mut worker_rx, 1000).await {
            received.push(msg);
        }
    }

    assert_eq!(
        received.len(),
        3,
        "worker should receive all 3 schedule tasks"
    );
    for (i, msg) in received.iter().enumerate() {
        let task = msg["payload"]["task"].as_str().unwrap();
        assert!(
            task.contains(&format!("#{}", i + 1)),
            "task {} should contain correct number: {}",
            i,
            task
        );
    }

    let _ = std::fs::remove_file(&socket);
}

/// Shell schedule output is posted to bus (simulates fire_shell).
#[tokio::test]
async fn test_shell_schedule_output_delivered() {
    let socket = temp_socket();

    let sock = socket.clone();
    tokio::spawn(async move {
        deskd::bus::serve(&sock).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (mut worker_rx, _worker_tx) = connect_and_register(&socket, "kira", &["agent:kira"]).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Simulate what fire_shell does: runs command, posts stdout as task.
    simulate_schedule_fire(
        &socket,
        "kira",
        "agent:kira",
        "Shell output:\nuptime: 42 days\nload: 0.5",
        "shell",
    )
    .await;

    let msg = read_one(&mut worker_rx, 1000).await;
    assert!(msg.is_some(), "worker should receive shell schedule output");
    let msg = msg.unwrap();

    assert_eq!(
        msg["source"], "shell-kira",
        "shell schedule should use shell-<agent> source"
    );
    let task = msg["payload"]["task"].as_str().unwrap();
    assert!(task.contains("uptime: 42 days"));

    let _ = std::fs::remove_file(&socket);
}
