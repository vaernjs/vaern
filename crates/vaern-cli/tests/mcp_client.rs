//! Starts a real `vaern mcp serve` through the runtime's MCP client and
//! drives it from start to close: spawn, version agreement, the tool list,
//! a call, and a teardown that leaves the server's exit code.

use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use vaern_core::budget::Limits;
use vaern_core::mcp::client::{self, Command, Launch, McpEvent};
use vaern_core::mcp::protocol::Era;

async fn next(events: &mut UnboundedReceiver<McpEvent>) -> McpEvent {
    match tokio::time::timeout(Duration::from_secs(20), events.recv()).await {
        Ok(Some(event)) => event,
        Ok(None) => panic!("the connection ended without reporting"),
        Err(_) => panic!("no event from the connection within 20 s"),
    }
}

#[tokio::test]
async fn the_client_starts_lists_calls_and_closes_a_served_script() {
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/sandbox_run/mcp_server.ts");
    let launch = Launch {
        program: PathBuf::from(env!("CARGO_BIN_EXE_vaern")),
        script,
        home: None,
        grant: Limits::default(),
    };
    let (commands, rx) = unbounded_channel();
    let (tx, mut events) = unbounded_channel();
    let task = tokio::spawn(client::run(
        launch,
        1,
        move |event| {
            let _ = tx.send(event);
        },
        rx,
    ));

    let hello = match next(&mut events).await {
        McpEvent::Connected {
            conn_id: 1,
            result: Ok(hello),
        } => hello,
        other => panic!("expected a connection, got {other:?}"),
    };
    assert_eq!(hello.era, Era::Modern);
    assert_eq!(hello.server["name"], "calc");
    let names: Vec<_> = hello
        .tools
        .iter()
        .map(|tool| tool["name"].clone())
        .collect();
    assert_eq!(names, [json!("add"), json!("fail")]);
    // Run with --no-journal, the server has no run id to report.
    assert_eq!(hello.run, None);

    commands
        .send(Command::Call {
            req_id: 9,
            tool: "add".into(),
            args: json!({ "a": 2, "b": 3 }),
        })
        .unwrap();
    match next(&mut events).await {
        McpEvent::Called {
            req_id: 9,
            result: Ok(result),
        } => {
            assert_eq!(result["structuredContent"], json!({ "sum": 5 }));
            assert_eq!(result["isError"], false);
        }
        other => panic!("expected the call's result, got {other:?}"),
    }

    commands.send(Command::Close).unwrap();
    match next(&mut events).await {
        McpEvent::Closed { conn_id: 1, closed } => assert_eq!(closed.exit_code, Some(0)),
        other => panic!("expected the close, got {other:?}"),
    }
    task.await.unwrap();
}
