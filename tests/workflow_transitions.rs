//! Functional test: workflow state transition → dispatch (#122).
//!
//! Tests the flow that existing unit tests don't cover:
//! completion event → find_next_state → dispatch new instance.
//!
//! We simulate the workflow engine's behavior on the real bus:
//! 1. Create a state machine instance in a temp store
//! 2. Start bus, connect workflow-engine and target agent
//! 3. Send completion message to sm:<instance-id>
//! 4. Workflow engine processes it: transitions state, dispatches task
//! 5. Target agent receives the dispatched task

use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

fn temp_socket() -> String {
    format!(
        "/tmp/deskd-test-wf-{}.sock",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn temp_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(format!(
        "/tmp/deskd-test-wf-dir-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
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

fn test_model() -> deskd::config::ModelDef {
    deskd::config::ModelDef {
        name: "pipeline".into(),
        description: "Test pipeline".into(),
        states: vec![
            "draft".into(),
            "review".into(),
            "approved".into(),
            "rejected".into(),
        ],
        initial: "draft".into(),
        terminal: vec!["approved".into(), "rejected".into()],
        transitions: vec![
            deskd::config::TransitionDef {
                from: "draft".into(),
                to: "review".into(),
                trigger: Some("auto".into()),
                on: None,
                assignee: Some("agent:reviewer".into()),
                prompt: Some("Review this code carefully.".into()),
                step_type: None,
                notify: None,
                timeout: None,
                timeout_goto: None,
                criteria: None,
            },
            deskd::config::TransitionDef {
                from: "review".into(),
                to: "approved".into(),
                trigger: None,
                on: Some("LGTM".into()),
                assignee: None,
                prompt: None,
                step_type: None,
                notify: None,
                timeout: None,
                timeout_goto: None,
                criteria: None,
            },
            deskd::config::TransitionDef {
                from: "review".into(),
                to: "rejected".into(),
                trigger: None,
                on: Some("REJECT".into()),
                assignee: None,
                prompt: None,
                step_type: None,
                notify: None,
                timeout: None,
                timeout_goto: None,
                criteria: None,
            },
        ],
    }
}

/// Simulate find_next_state logic (same as workflow.rs, replicated here
/// because the function is private).
fn find_next_state(
    model: &deskd::config::ModelDef,
    current_state: &str,
    result: &str,
) -> Option<String> {
    let transitions = deskd::statemachine::valid_transitions(model, current_state);
    let result_upper = result.trim().to_uppercase();

    // Keyword matches first.
    for t in &transitions {
        if let Some(ref keyword) = t.on {
            if result_upper.starts_with(&keyword.to_uppercase()) {
                return Some(t.to.clone());
            }
        }
    }

    // Auto triggers.
    for t in &transitions {
        if t.trigger.as_deref() == Some("auto") {
            return Some(t.to.clone());
        }
    }

    None
}

/// Full flow: completion event → state transition → dispatch to agent.
///
/// Simulates the workflow engine processing a completion for a "draft" instance:
/// draft --auto--> review (dispatches to agent:reviewer).
#[tokio::test]
async fn test_completion_triggers_transition_and_dispatch() {
    let socket = temp_socket();
    let tmp = temp_dir();

    // Create a temp statemachine store with an instance in "draft" state.
    let store = deskd::statemachine::StateMachineStore::new(tmp.clone());
    let model = test_model();
    let inst = store
        .create(
            &model,
            "Fix bug #42",
            "The login page crashes on submit",
            "test-sender",
        )
        .unwrap();
    let instance_id = inst.id.clone();
    assert_eq!(inst.state, "draft");

    // Start bus.
    let sock = socket.clone();
    tokio::spawn(async move {
        deskd::bus::serve(&sock).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connect "workflow-engine" (simulates the real engine, subscribed to sm:*).
    let (mut engine_rx, mut engine_tx) =
        connect_and_register(&socket, "workflow-engine", &["sm:*"]).await;

    // Connect "reviewer" (the agent that should receive the dispatched task).
    let (mut reviewer_rx, _reviewer_tx) =
        connect_and_register(&socket, "reviewer", &["agent:reviewer"]).await;

    // Connect "sender" who will send the completion event.
    let (_sender_rx, mut sender_tx) = connect_and_register(&socket, "sender", &[]).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Sender sends completion to sm:<instance_id>.
    let completion_msg = serde_json::json!({
        "type": "message",
        "id": uuid::Uuid::new_v4().to_string(),
        "source": "sender",
        "target": format!("sm:{}", instance_id),
        "payload": {"result": "Draft complete, ready for review"},
        "metadata": {"priority": 5},
    });
    let mut line = serde_json::to_string(&completion_msg).unwrap();
    line.push('\n');
    sender_tx.write_all(line.as_bytes()).await.unwrap();

    // Workflow engine receives the completion.
    let received = read_one(&mut engine_rx, 1000).await;
    assert!(received.is_some(), "engine should receive completion event");
    let received = received.unwrap();
    assert_eq!(received["target"], format!("sm:{}", instance_id));

    let result = received["payload"]["result"].as_str().unwrap_or("");

    // Simulate handle_completion: load instance, find next state, transition, dispatch.
    let mut inst = store.load(&instance_id).unwrap();
    inst.result = Some(result.to_string());
    inst.updated_at = chrono::Utc::now().to_rfc3339();
    store.save(&inst).unwrap();

    let current_state = inst.state.clone();
    let next_state = find_next_state(&model, &current_state, result);
    assert_eq!(
        next_state,
        Some("review".into()),
        "draft + auto trigger should transition to review"
    );

    let target_state = next_state.unwrap();
    store
        .move_to(&mut inst, &model, &target_state, "auto", Some(result))
        .unwrap();

    // Verify state transitioned.
    assert_eq!(inst.state, "review");
    assert_eq!(inst.assignee, "agent:reviewer");
    assert!(!deskd::statemachine::is_terminal(&model, &inst));

    // Dispatch task to agent:reviewer (same as workflow engine does).
    let task_msg = serde_json::json!({
        "type": "message",
        "id": uuid::Uuid::new_v4().to_string(),
        "source": "workflow-engine",
        "target": "agent:reviewer",
        "payload": {
            "task": format!(
                "Review this code carefully.\n\n---\n## Task: {}\n\n{}\n\n---\n## Previous step result\n\n{}\n\n---\n## Metadata\ninstance_id: {}\nmodel: {}\nstate: {}",
                inst.title, inst.body, result, inst.id, inst.model, inst.state
            ),
            "sm_instance_id": inst.id,
        },
        "reply_to": format!("sm:{}", inst.id),
        "metadata": {"priority": 5u8},
    });
    let mut dispatch_line = serde_json::to_string(&task_msg).unwrap();
    dispatch_line.push('\n');
    engine_tx.write_all(dispatch_line.as_bytes()).await.unwrap();

    // Reviewer should receive the dispatched task.
    let task = read_one(&mut reviewer_rx, 1000).await;
    assert!(task.is_some(), "reviewer should receive dispatched task");
    let task = task.unwrap();
    assert_eq!(task["source"], "workflow-engine");
    assert_eq!(task["target"], "agent:reviewer");

    let task_text = task["payload"]["task"].as_str().unwrap();
    assert!(
        task_text.contains("Review this code carefully"),
        "task should contain prompt"
    );
    assert!(
        task_text.contains("Fix bug #42"),
        "task should contain title"
    );
    assert!(
        task_text.contains("login page crashes"),
        "task should contain body"
    );
    assert!(
        task_text.contains(&instance_id),
        "task should contain instance_id"
    );

    assert_eq!(
        task["reply_to"],
        format!("sm:{}", instance_id),
        "reply_to should route back to state machine"
    );

    // Verify persisted state.
    let final_inst = store.load(&instance_id).unwrap();
    assert_eq!(final_inst.state, "review");
    assert_eq!(final_inst.history.len(), 1);
    assert_eq!(final_inst.history[0].from, "draft");
    assert_eq!(final_inst.history[0].to, "review");

    // Cleanup.
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Test keyword-triggered transition: review → approved on "LGTM".
#[tokio::test]
async fn test_keyword_transition_lgtm_approves() {
    let tmp = temp_dir();
    let store = deskd::statemachine::StateMachineStore::new(tmp.clone());
    let model = test_model();

    // Create instance and move to review state.
    let mut inst = store
        .create(&model, "Review PR #99", "Add caching layer", "kira")
        .unwrap();
    store
        .move_to(&mut inst, &model, "review", "auto", None)
        .unwrap();
    assert_eq!(inst.state, "review");

    // Simulate completion with LGTM keyword.
    let result = "LGTM - code looks clean, good test coverage";
    let next = find_next_state(&model, "review", result);
    assert_eq!(next, Some("approved".into()));

    store
        .move_to(&mut inst, &model, "approved", "auto", Some(result))
        .unwrap();
    assert_eq!(inst.state, "approved");
    assert!(deskd::statemachine::is_terminal(&model, &inst));

    // Terminal state — workflow engine should NOT dispatch.
    // Verify history records both transitions.
    assert_eq!(inst.history.len(), 2);
    assert_eq!(inst.history[0].from, "draft");
    assert_eq!(inst.history[0].to, "review");
    assert_eq!(inst.history[1].from, "review");
    assert_eq!(inst.history[1].to, "approved");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Test keyword-triggered transition: review → rejected on "REJECT".
#[tokio::test]
async fn test_keyword_transition_reject() {
    let tmp = temp_dir();
    let store = deskd::statemachine::StateMachineStore::new(tmp.clone());
    let model = test_model();

    let mut inst = store
        .create(&model, "Review PR #100", "Risky change", "kira")
        .unwrap();
    store
        .move_to(&mut inst, &model, "review", "auto", None)
        .unwrap();

    let result = "REJECT: missing error handling in critical path";
    let next = find_next_state(&model, "review", result);
    assert_eq!(next, Some("rejected".into()));

    store
        .move_to(&mut inst, &model, "rejected", "auto", Some(result))
        .unwrap();
    assert!(deskd::statemachine::is_terminal(&model, &inst));

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Test no matching transition: ambiguous result stays in current state.
#[tokio::test]
async fn test_no_matching_transition_stays_in_state() {
    let tmp = temp_dir();
    let store = deskd::statemachine::StateMachineStore::new(tmp.clone());
    let model = test_model();

    let mut inst = store
        .create(&model, "Review PR #101", "Unclear change", "kira")
        .unwrap();
    store
        .move_to(&mut inst, &model, "review", "auto", None)
        .unwrap();

    // Result doesn't match LGTM or REJECT — no transition.
    let result = "Need more context, requesting clarification from author";
    let next = find_next_state(&model, "review", result);
    assert_eq!(next, None, "ambiguous result should not trigger transition");

    // Instance stays in review.
    assert_eq!(inst.state, "review");

    let _ = std::fs::remove_dir_all(&tmp);
}
