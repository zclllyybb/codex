#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used)]

use core_test_support::responses;
use core_test_support::test_codex_exec::test_codex_exec;
use pretty_assertions::assert_eq;
use serde_json::Value;

fn tool_names(body: &Value) -> Vec<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool.get("name")
                        .or_else(|| tool.get("type"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn tool_is_exposed(body: &Value, tool_name: &str) -> bool {
    if tool_names(body).iter().any(|name| name == tool_name) {
        return true;
    }

    let nested_tool_heading = format!("### `{tool_name}`");
    body.get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("additional_tools"))
        .filter_map(|item| item.get("tools").and_then(Value::as_array))
        .flatten()
        .filter_map(|tool| tool.get("description").and_then(Value::as_str))
        .any(|description| description.contains(&nested_tool_heading))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_flag_starts_goal_mode_and_waits_for_completion() -> anyhow::Result<()> {
    let test = test_codex_exec();
    let server = responses::start_mock_server().await;
    let call_id = "complete-goal";
    let first_response = responses::sse(vec![
        responses::ev_response_created("resp-goal-1"),
        responses::ev_function_call(call_id, "update_goal", r#"{"status":"complete"}"#),
        responses::ev_completed("resp-goal-1"),
    ]);
    let second_response = responses::sse(vec![
        responses::ev_response_created("resp-goal-2"),
        responses::ev_assistant_message("msg-goal", "goal complete"),
        responses::ev_completed("resp-goal-2"),
    ]);
    let response_mock =
        responses::mount_sse_sequence(&server, vec![first_response, second_response]).await;

    let objective = "finish the goal-mode exec task";
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("--goal")
        .arg(objective)
        .assert()
        .success();

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);

    let first_request = requests[0].body_json();
    assert!(
        first_request.to_string().contains(objective),
        "goal objective should be injected into the continuation context"
    );
    assert!(
        tool_is_exposed(&first_request, "update_goal"),
        "active goal turns should expose update_goal"
    );

    let tool_output = requests[1]
        .function_call_output_text(call_id)
        .expect("second request should include update_goal output");
    assert!(
        tool_output.contains("\"status\":\"complete\""),
        "update_goal output should mark the goal complete"
    );

    Ok(())
}
