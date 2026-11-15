//! M2 end to end: `sitewright prompt` drives the daemon's agent against a mock
//! OpenAI-compatible server, and the site it builds is served.

#![allow(clippy::unwrap_used)]

mod common;

use common::{PAGE, boot_with, host, llm_config, mock_model, text_turn, tool_turn, write_secrets};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn prompt_new_builds_a_site_and_a_follow_up_edits_it() {
    let model = mock_model(vec![
        tool_turn("create_site", json!({"name": "hikes", "title": "Washington Hikes"})),
        tool_turn("write_file", json!({"path": "index.html", "content": PAGE})),
        tool_turn("publish_site", json!({"message": "First version"})),
        text_turn("Your site is live."),
        // Second run: an edit.
        tool_turn("write_file", json!({"path": "index.html", "content": PAGE.replace("Washington trails", "My favourite trails")})),
        tool_turn("publish_site", json!({"message": "Change heading"})),
        text_turn("Heading changed."),
    ])
    .await;
    let t = boot_with(&llm_config(&model)).await;
    write_secrets(&t);

    t.cli(&[
        "prompt",
        "--new",
        "hikes",
        "a hiking log with my favorite Washington trails",
    ])
    .await
    .unwrap();
    assert!(
        t.get(&host("hikes"), "/")
            .await
            .text()
            .await
            .unwrap()
            .contains("Washington trails")
    );

    t.cli(&["prompt", "hikes", "change the heading"])
        .await
        .unwrap();
    assert!(
        t.get(&host("hikes"), "/")
            .await
            .text()
            .await
            .unwrap()
            .contains("My favourite trails")
    );

    // Runs are recorded with tokens and cost: 4 calls of 1000 in / 100 out.
    let runs: Vec<Value> = t.admin().get("/v1/runs?site=hikes").await.unwrap();
    assert_eq!(runs.len(), 2);
    let first = &runs[1];
    assert_eq!(first["status"], "succeeded");
    assert_eq!(
        (
            first["turns"].as_i64(),
            first["input_tokens"].as_i64(),
            first["output_tokens"].as_i64()
        ),
        (Some(4), Some(4000), Some(400))
    );
    assert!((first["cost_usd"].as_f64().unwrap() - 0.0018).abs() < 1e-9);
    assert_eq!(first["model"], "mock-mini");

    // The stored event stream replays over SSE after the run has ended.
    let body = t
        .admin()
        .stream(&format!("/v1/runs/{}/events", first["id"]))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let kinds: Vec<String> = body
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(|d| {
            serde_json::from_str::<Value>(d).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .filter(|k| k != "usage")
        .collect();
    assert_eq!(
        kinds,
        [
            "started",
            "tool_call",
            "tool_result",
            "tool_call",
            "tool_result",
            "tool_call",
            "tool_result",
            "text",
            "done"
        ]
    );

    // The model received the system prompt and the tool definitions.
    let sent: Value = model.received_requests().await.unwrap()[0]
        .body_json()
        .unwrap();
    assert!(
        sent["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("Sitewright's site builder")
    );
    assert_eq!(sent["tools"].as_array().unwrap().len(), 14);
    t.daemon.shutdown().await;
}

#[tokio::test]
async fn prompt_fails_clearly_without_a_provider_or_key_or_site() {
    // No provider configured at all.
    let t = boot_with("").await;
    let err = t.cli(&["prompt", "--new", "hikes", "x"]).await.unwrap_err();
    assert!(
        err.to_string().contains("no LLM provider is configured"),
        "{err}"
    );
    t.daemon.shutdown().await;

    // Provider configured, key missing from secrets.toml.
    let model = mock_model(vec![]).await;
    let t = boot_with(&llm_config(&model)).await;
    let err = t.cli(&["prompt", "--new", "hikes", "x"]).await.unwrap_err();
    assert!(err.to_string().contains("not in secrets.toml"), "{err}");

    write_secrets(&t);
    let err = t.cli(&["prompt", "hikes", "x"]).await.unwrap_err();
    assert!(err.to_string().contains("does not exist"), "{err}");
    assert!(t.cli(&["prompt", "--new", "admin", "x"]).await.is_err());
    t.daemon.shutdown().await;
}

#[tokio::test]
async fn a_rejected_key_fails_the_run_without_leaking_it() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_json(
            json!({"error": {"message": "Access denied due to invalid subscription key."}}),
        ))
        .mount(&server)
        .await;
    let t = boot_with(&llm_config(&server)).await;
    write_secrets(&t);
    let err = t
        .cli(&["prompt", "--new", "hikes", "x"])
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("failed") && err.contains("invalid subscription key"),
        "{err}"
    );
    assert!(!err.contains("test-key"));
    let runs: Vec<Value> = t.admin().get("/v1/runs").await.unwrap();
    assert_eq!(runs[0]["status"], "failed");
    assert!(!runs[0].to_string().contains("test-key"));
    t.daemon.shutdown().await;
}

#[tokio::test]
async fn domain_check_tokens_are_answered_on_any_host() {
    let t = boot_with("").await;
    // An unknown token is a 404 like any other unknown path.
    assert_eq!(
        t.get("whatever.example.org", "/.well-known/sitewright/nope")
            .await
            .status(),
        404
    );

    // A real check against the daemon's own listener: `localhost` resolves here, and the
    // daemon answers the token on a hostname it has never heard of.
    let check = t
        .daemon
        .state
        .domains
        .check("localhost", t.daemon.addr.port())
        .await;
    assert!(check.reaches_this_server, "{check:?}");
    assert!(!check.addresses.is_empty());
    t.daemon.shutdown().await;
}
