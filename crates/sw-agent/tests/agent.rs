//! The agent loop driven by a scripted provider: full create and edit runs, tool errors,
//! limits, cancellation and the delete confirmation flow.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use sw_agent::{Agent, AgentError, ProviderSource, Run, RunEvent, RunRequest, RunStatus, Skills};
use sw_core::{Config, DataDir};
use sw_llm::mock::{ScriptedProvider, Turn};
use sw_llm::{ChatRequest, EventStream, LlmError, LlmProvider, Message};
use sw_sites::{Author, DomainChecker, SiteStore};

const PAGE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
    <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Hikes</title>\
    <link rel=\"stylesheet\" href=\"site.css\"></head><body><h1>Hikes</h1></body></html>";

struct Fixed(Arc<dyn LlmProvider>);
#[async_trait::async_trait]
impl ProviderSource for Fixed {
    async fn provider(&self, id: Option<&str>) -> Result<Arc<dyn LlmProvider>, String> {
        match id {
            Some("missing") => Err("no provider named \"missing\"".into()),
            _ => Ok(self.0.clone()),
        }
    }
}

struct Env {
    _dir: tempfile::TempDir,
    store: SiteStore,
    agent: Agent,
    provider: Arc<ScriptedProvider>,
}

async fn env(turns: Vec<Turn>) -> Env {
    env_with(ScriptedProvider::new(turns), "").await
}

async fn env_with(provider: ScriptedProvider, extra: &str) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path());
    data.ensure().unwrap();
    let skills_dir = data.skills_dir();
    let db = sw_core::db::open(&data.db_file()).await.unwrap();
    sw_core::db::migrate(&db, &data.backups_dir())
        .await
        .unwrap();
    let metrics = sw_metrics::Metrics::open(db.clone(), &data.db_file(), 2000)
        .await
        .unwrap();
    let toml = format!(
        "[domains]\nzones = [\"example.com\"]\npublic_scheme = \"https\"\n\
         [[llm.prices]]\nmodel = \"mock-model\"\ninput_per_mtok = 1.0\noutput_per_mtok = 10.0\n{extra}"
    );
    let config = Arc::new(Config::from_toml(&toml).unwrap());
    let store = SiteStore::open(db, data, &config).await.unwrap();
    let provider = Arc::new(provider);
    let agent = Agent::new(
        store.clone(),
        DomainChecker::new(),
        Skills::new(Some(skills_dir.clone())),
        metrics.clone(),
        config,
        Arc::new(Fixed(provider.clone())),
    )
    .await
    .unwrap();
    Env {
        _dir: dir,
        store,
        agent,
        provider,
    }
}

fn request(site: &str, prompt: &str, new: bool) -> RunRequest {
    RunRequest {
        site: site.into(),
        prompt: prompt.into(),
        new,
        provider: None,
    }
}

/// Wait for a run to finish and return its record and stored events.
async fn finish(agent: &Agent, run: &Run) -> (Run, Vec<RunEvent>) {
    let mut subscription = agent.subscribe(run.id).await.unwrap();
    if let Some(live) = subscription.live.as_mut() {
        loop {
            match tokio::time::timeout(Duration::from_secs(10), live.recv())
                .await
                .expect("run did not finish")
            {
                Ok((_, RunEvent::Done { .. })) | Err(_) => break,
                Ok(_) => {}
            }
        }
    }
    // The Done event is broadcast just before the run is unregistered.
    for _ in 0..100 {
        if !agent.is_busy(&run.site_name) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let events = agent
        .subscribe(run.id)
        .await
        .unwrap()
        .past
        .into_iter()
        .map(|(_, e)| e)
        .collect();
    (agent.get(run.id).await.unwrap(), events)
}

fn tool_results(events: &[RunEvent]) -> Vec<(String, bool, Value)> {
    events
        .iter()
        .filter_map(|e| match e {
            RunEvent::ToolResult {
                name,
                is_error,
                content,
                ..
            } => Some((
                name.clone(),
                *is_error,
                serde_json::from_str(content).unwrap_or(Value::Null),
            )),
            _ => None,
        })
        .collect()
}

fn create_script() -> Vec<Turn> {
    vec![
        Turn::tools(&[(
            "create_site",
            json!({"name": "hikes", "title": "Washington Hikes"}),
        )]),
        Turn::tools(&[
            ("write_file", json!({"path": "index.html", "content": PAGE})),
            (
                "write_file",
                json!({"path": "site.css", "content": "body { font-family: system-ui }"}),
            ),
        ]),
        Turn::tools(&[("validate_site", json!({}))]),
        Turn::tools(&[("publish_site", json!({"message": "First version"}))]),
        Turn::text("Your hiking log is live at https://hikes.example.com"),
    ]
}

#[tokio::test]
async fn a_create_run_builds_and_publishes_a_site() {
    let env = env(create_script()).await;
    let run = env
        .agent
        .start(request(
            "hikes",
            "a hiking log with my favorite Washington trails",
            true,
        ))
        .await
        .unwrap();
    assert_eq!(run.status, RunStatus::Running);
    let (run, events) = finish(&env.agent, &run).await;

    assert_eq!(run.status, RunStatus::Succeeded, "{:?}", run.message);
    assert_eq!(
        run.message.as_deref(),
        Some("Your hiking log is live at https://hikes.example.com")
    );
    assert_eq!(
        (run.turns, run.input_tokens, run.output_tokens),
        (5, 500, 100)
    );
    // 500 input tokens at $1/M plus 100 output tokens at $10/M.
    assert!((run.cost_usd.unwrap() - 0.0015).abs() < 1e-12);
    assert_eq!(
        (run.provider.as_str(), run.model.as_str()),
        ("mock", "mock-model")
    );

    // The site is live with the agent's content, attributed to the agent.
    let info = env.store.info("hikes").await.unwrap();
    assert_eq!(info.site.title, "Washington Hikes");
    assert_eq!(info.releases[0].author, "agent");
    assert_eq!(info.releases[0].message, "First version");
    let routes = env.store.routes();
    let root = &routes.get("hikes.example.com").unwrap().root;
    assert_eq!(
        std::fs::read_to_string(root.join("index.html")).unwrap(),
        PAGE
    );

    // The event stream is ordered and complete.
    assert!(matches!(events.first(), Some(RunEvent::Started { .. })));
    assert!(matches!(events.last(), Some(RunEvent::Done { status, .. }) if status == "succeeded"));
    let results = tool_results(&events);
    let names: Vec<_> = results.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(
        names,
        [
            "create_site",
            "write_file",
            "write_file",
            "validate_site",
            "publish_site"
        ]
    );
    assert!(results.iter().all(|r| !r.1), "{results:?}");
    assert_eq!(results[3].2["ok"], true);
    assert_eq!(results[4].2["url"], "https://hikes.example.com");

    // The model saw the system prompt, the tools and each tool result.
    let requests = env.provider.requests();
    assert_eq!(requests.len(), 5);
    assert!(
        requests[0]
            .system
            .contains("- Name: `hikes`\n- It does not exist yet")
            && requests[0].system.contains("- `create-site`:")
    );
    assert_eq!(requests[0].tools.len(), 23);
    assert!(
        matches!(&requests[1].messages[2], Message::Tool { name, is_error: false, .. } if name == "create_site")
    );
    assert_eq!(requests[4].messages.len(), 1 + 2 + 3 + 2 + 2);
}

#[tokio::test]
async fn an_edit_run_reads_changes_and_republishes() {
    let env = env(vec![
        Turn::tools(&[("read_file", json!({"path": "index.html"}))]),
        Turn::tools(&[("write_file", json!({"path": "index.html", "content": PAGE.replace("<h1>Hikes</h1>", "<h1>My Hikes</h1>")}))]),
        Turn::tools(&[("publish_site", json!({"message": "Rename heading"}))]),
        Turn::text("Renamed the heading."),
    ])
    .await;
    env.store
        .create("hikes", "Hikes", Author::User)
        .await
        .unwrap();
    env.store
        .write_file("hikes", "index.html", PAGE.as_bytes().to_vec())
        .await
        .unwrap();
    env.store
        .write_file("hikes", "site.css", b"body{}".to_vec())
        .await
        .unwrap();
    env.store
        .publish("hikes", "first", Author::User)
        .await
        .unwrap();

    let run = env
        .agent
        .start(request("hikes", "rename the heading to My Hikes", false))
        .await
        .unwrap();
    let (run, events) = finish(&env.agent, &run).await;
    assert_eq!(run.status, RunStatus::Succeeded, "{:?}", run.message);
    let results = tool_results(&events);
    assert_eq!(
        results[0].2["content"], PAGE,
        "read_file returns the current content"
    );
    let routes = env.store.routes();
    let live = std::fs::read_to_string(
        routes
            .get("hikes.example.com")
            .unwrap()
            .root
            .join("index.html"),
    )
    .unwrap();
    assert!(live.contains("<h1>My Hikes</h1>"));
    assert_eq!(env.store.info("hikes").await.unwrap().releases.len(), 2);

    // The prompt for an existing site lists its files and releases.
    let system = &env.provider.requests()[0].system;
    assert!(
        system.contains("- `edit-site`:")
            && system.contains("- `index.html`")
            && system.contains("\"first\"")
    );
}

#[tokio::test]
async fn tool_errors_go_back_to_the_model_and_the_sandbox_holds() {
    let env = env(vec![
        Turn::tools(&[
            (
                "write_file",
                json!({"path": "../../etc/cron.d/evil", "content": "x"}),
            ),
            (
                "write_file",
                json!({"path": ".git/hooks/pre-commit.js", "content": "x"}),
            ),
            ("write_file", json!({"path": "run.sh", "content": "x"})),
            ("read_file", json!({"path": "/etc/passwd"})),
            ("write_file", json!({"path": "index.html"})),
            (
                "write_file",
                json!({"_invalid_json": "{\"path\": \"index.ht"}),
            ),
            ("run_shell", json!({"command": "id"})),
            ("create_site", json!({"name": "other", "title": "Other"})),
            ("create_site", json!({"name": "hikes", "title": "Again"})),
            ("delete_file", json!({"path": "missing.html"})),
        ]),
        // A page with a broken link cannot be published.
        Turn::tools(&[(
            "write_file",
            json!({"path": "index.html", "content": PAGE.replace("site.css", "nope.css")}),
        )]),
        Turn::tools(&[("publish_site", json!({"message": "broken"}))]),
        Turn::text("I could not publish."),
    ])
    .await;
    env.store
        .create("hikes", "Hikes", Author::User)
        .await
        .unwrap();

    let run = env
        .agent
        .start(request("hikes", "do bad things", false))
        .await
        .unwrap();
    let (run, events) = finish(&env.agent, &run).await;
    assert_eq!(
        run.status,
        RunStatus::Succeeded,
        "the run survives tool errors"
    );

    let results = tool_results(&events);
    for (i, needle) in [
        (0, "not allowed"),
        (1, "hidden files"),
        (2, ".sh are not allowed"),
        (3, "absolute paths"),
        (4, "missing required string argument \"content\""),
        (5, "not valid JSON"),
        (6, "no tool named"),
        (7, "already exists"),
        (8, "already exists"),
        (9, "does not exist"),
    ] {
        assert!(results[i].1, "call {i} should fail: {:?}", results[i]);
        assert!(
            results[i].2["error"].as_str().unwrap().contains(needle),
            "call {i}: {:?}",
            results[i].2
        );
    }
    assert!(!results[10].1);
    assert!(
        results[11].1
            && results[11].2["error"]
                .as_str()
                .unwrap()
                .contains("nope.css"),
        "{:?}",
        results[11]
    );
    assert!(env.store.routes().is_empty(), "nothing was published");
    assert!(env.store.find("other").await.unwrap().is_none());

    // The model was shown every error.
    let requests = env.provider.requests();
    let errors = requests[1]
        .messages
        .iter()
        .filter(|m| matches!(m, Message::Tool { is_error: true, .. }))
        .count();
    assert_eq!(errors, 10);
}

#[tokio::test]
async fn delete_only_asks_for_confirmation() {
    let env = env(vec![
        Turn::tools(&[("delete_site", json!({}))]),
        Turn::text("I have asked you to confirm."),
    ])
    .await;
    env.store
        .create("hikes", "Hikes", Author::User)
        .await
        .unwrap();
    let run = env
        .agent
        .start(request("hikes", "delete this site", false))
        .await
        .unwrap();
    let (run, events) = finish(&env.agent, &run).await;
    assert_eq!(run.status, RunStatus::Succeeded);
    assert!(events.iter().any(|e| matches!(e, RunEvent::Confirmation { action, site, .. } if action == "delete_site" && site == "hikes")));
    assert_eq!(tool_results(&events)[0].2["status"], "needs_confirmation");
    assert!(
        env.store.get("hikes").await.is_ok(),
        "the agent cannot delete by itself"
    );
}

#[tokio::test]
async fn turn_token_and_spend_limits_end_runs_cleanly() {
    let looping = || ScriptedProvider::repeating(Turn::tools(&[("list_files", json!({}))]));

    let env = env_with(looping(), "[agent]\nmax_turns = 3\n").await;
    env.store.create("s", "S", Author::User).await.unwrap();
    let run = env.agent.start(request("s", "loop", false)).await.unwrap();
    let (run, events) = finish(&env.agent, &run).await;
    assert_eq!(run.status, RunStatus::Limit);
    assert_eq!(run.turns, 3);
    assert!(run.message.unwrap().contains("max_turns"));
    assert!(matches!(events.last(), Some(RunEvent::Done { status, .. }) if status == "limit"));

    // Each scripted call uses 120 tokens: the cap of 300 is passed after the third.
    let env = env_with(looping(), "[agent]\nmax_run_tokens = 300\n").await;
    env.store.create("s", "S", Author::User).await.unwrap();
    let run = env.agent.start(request("s", "loop", false)).await.unwrap();
    let (run, _) = finish(&env.agent, &run).await;
    assert_eq!(run.status, RunStatus::Limit);
    assert_eq!(run.turns, 3);
    assert!(run.message.unwrap().contains("max_run_tokens"));

    // Each call costs $0.0003: a cap of $0.0005 stops the run after the second, and
    // then refuses to start another run today.
    let env = env_with(looping(), "[agent]\ndaily_spend_cap_usd = 0.0005\n").await;
    env.store.create("s", "S", Author::User).await.unwrap();
    let run = env.agent.start(request("s", "loop", false)).await.unwrap();
    let (run, _) = finish(&env.agent, &run).await;
    assert_eq!(run.status, RunStatus::Limit);
    assert_eq!(run.turns, 2);
    assert!(run.message.unwrap().contains("daily cap"));
    let refused = env
        .agent
        .start(request("s", "again", false))
        .await
        .unwrap_err();
    assert!(matches!(refused, AgentError::Budget(_)), "{refused}");
}

#[tokio::test]
async fn provider_failures_fail_the_run_with_the_reason() {
    let env = env(vec![
        Turn::tools(&[("list_files", json!({}))]),
        Turn::Fail(LlmError::Auth {
            status: 401,
            message: "Incorrect API key".into(),
        }),
    ])
    .await;
    env.store.create("s", "S", Author::User).await.unwrap();
    let run = env.agent.start(request("s", "hi", false)).await.unwrap();
    let (run, events) = finish(&env.agent, &run).await;
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.message.unwrap().contains("Incorrect API key"));
    assert!(matches!(events.last(), Some(RunEvent::Done { status, .. }) if status == "failed"));
    assert!(run.finished_at.is_some());
}

#[tokio::test]
async fn start_validates_the_request() {
    let env = env(vec![]).await;
    env.store.create("s", "S", Author::User).await.unwrap();
    let invalid = |r: RunRequest| {
        let agent = env.agent.clone();
        async move { agent.start(r).await.unwrap_err() }
    };
    assert!(matches!(
        invalid(request("s", "   ", false)).await,
        AgentError::Invalid(_)
    ));
    assert!(matches!(
        invalid(request("Bad Name", "x", true)).await,
        AgentError::Invalid(_)
    ));
    assert!(
        matches!(
            invalid(request("s", "x", true)).await,
            AgentError::Invalid(_)
        ),
        "--new on an existing site"
    );
    assert!(
        matches!(
            invalid(request("admin", "x", true)).await,
            AgentError::Invalid(_)
        ),
        "reserved name"
    );
    assert!(
        matches!(
            invalid(request("nope", "x", false)).await,
            AgentError::Site(_)
        ),
        "unknown site"
    );
    let mut r = request("s", "x", false);
    r.provider = Some("missing".into());
    assert!(matches!(invalid(r).await, AgentError::Provider(_)));
    assert!(matches!(
        env.agent.get(999).await.unwrap_err(),
        AgentError::NotFound(999)
    ));
    assert!(
        env.agent.list(None, 10).await.unwrap().is_empty(),
        "rejected requests leave no run behind"
    );
}

/// A provider that blocks until released, to hold a run open. Each permit lets one
/// model call through, whether it is added before or after the call starts waiting.
#[derive(Debug)]
struct Gated {
    release: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl LlmProvider for Gated {
    fn id(&self) -> &str {
        "gated"
    }
    fn model(&self) -> &str {
        "gated-model"
    }
    async fn stream(&self, _request: ChatRequest) -> Result<EventStream, LlmError> {
        self.release.acquire().await.unwrap().forget();
        Turn::text("done").into_stream()
    }
}

trait IntoStream {
    fn into_stream(self) -> Result<EventStream, LlmError>;
}
impl IntoStream for Turn {
    fn into_stream(self) -> Result<EventStream, LlmError> {
        match self {
            Turn::Events(events) => Ok(Box::pin(futures_util::stream::iter(
                events.into_iter().map(Ok),
            ))),
            Turn::Fail(e) => Err(e),
        }
    }
}

async fn gated_env(extra: &str) -> (tempfile::TempDir, SiteStore, Agent, Arc<Gated>) {
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path());
    data.ensure().unwrap();
    let skills_dir = data.skills_dir();
    let db = sw_core::db::open(&data.db_file()).await.unwrap();
    sw_core::db::migrate(&db, &data.backups_dir())
        .await
        .unwrap();
    let metrics = sw_metrics::Metrics::open(db.clone(), &data.db_file(), 2000)
        .await
        .unwrap();
    let config = Arc::new(
        Config::from_toml(&format!("[domains]\nzones = [\"example.com\"]\n{extra}")).unwrap(),
    );
    let store = SiteStore::open(db, data, &config).await.unwrap();
    let gated = Arc::new(Gated {
        release: tokio::sync::Semaphore::new(0),
    });
    let agent = Agent::new(
        store.clone(),
        DomainChecker::new(),
        Skills::new(Some(skills_dir.clone())),
        metrics.clone(),
        config,
        Arc::new(Fixed(gated.clone())),
    )
    .await
    .unwrap();
    for name in ["a", "b", "c"] {
        store.create(name, name, Author::User).await.unwrap();
    }
    (dir, store, agent, gated)
}

#[tokio::test]
async fn one_run_per_site_and_a_cap_on_parallel_runs() {
    let (_dir, _store, agent, gated) = gated_env("[agent]\nmax_concurrent_runs = 2\n").await;
    let first = agent.start(request("a", "x", false)).await.unwrap();
    assert!(agent.is_busy("a"));
    let same_site = agent.start(request("a", "y", false)).await.unwrap_err();
    assert!(matches!(same_site, AgentError::Busy(_)), "{same_site}");

    let second = agent.start(request("b", "x", false)).await.unwrap();
    let too_many = agent.start(request("c", "x", false)).await.unwrap_err();
    assert!(matches!(too_many, AgentError::Busy(_)), "{too_many}");

    gated.release.add_permits(2);
    assert_eq!(finish(&agent, &first).await.0.status, RunStatus::Succeeded);
    assert_eq!(finish(&agent, &second).await.0.status, RunStatus::Succeeded);
    // Slots and the site lock are free again.
    let third = agent.start(request("a", "z", false)).await.unwrap();
    gated.release.add_permits(1);
    assert_eq!(finish(&agent, &third).await.0.status, RunStatus::Succeeded);
    assert_eq!(agent.list(Some("a"), 10).await.unwrap().len(), 2);
    assert_eq!(agent.list(None, 10).await.unwrap().len(), 3);
}

#[tokio::test]
async fn a_run_can_be_cancelled_while_waiting_for_the_model() {
    let (_dir, _store, agent, _gated) = gated_env("").await;
    let run = agent.start(request("a", "x", false)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(agent.cancel(run.id));
    let (run, events) = finish(&agent, &run).await;
    assert_eq!(run.status, RunStatus::Cancelled);
    assert!(matches!(events.last(), Some(RunEvent::Done { status, .. }) if status == "cancelled"));
    assert!(!agent.cancel(run.id), "a finished run cannot be cancelled");
    assert!(!agent.is_busy("a"));
}

#[tokio::test]
async fn runs_interrupted_by_a_restart_are_marked_failed() {
    let (_dir, store, agent, gated) = gated_env("").await;
    let run = agent.start(request("a", "x", false)).await.unwrap();
    // A new manager on the same database stands in for a restarted daemon.
    let config = Arc::new(Config::from_toml("[domains]\nzones = [\"example.com\"]\n").unwrap());
    let metrics = sw_metrics::Metrics::open(
        store.db().clone(),
        &DataDir::new(_dir.path()).db_file(),
        2000,
    )
    .await
    .unwrap();
    let restarted = Agent::new(
        store,
        DomainChecker::new(),
        Skills::new(None),
        metrics.clone(),
        config,
        Arc::new(Fixed(gated)),
    )
    .await
    .unwrap();
    let seen = restarted.get(run.id).await.unwrap();
    assert_eq!(seen.status, RunStatus::Failed);
    assert!(seen.message.unwrap().contains("restarted"));
}

#[tokio::test]
async fn skills_are_indexed_loaded_on_demand_and_user_skills_are_picked_up() {
    let env = env(vec![
        Turn::tools(&[
            ("load_skill", json!({"name": "house-style"})),
            (
                "read_skill_file",
                json!({"name": "house-style", "path": "references/palette.md"}),
            ),
            ("load_skill", json!({"name": "edit-site"})),
            ("load_skill", json!({"name": "no-such-skill"})),
            (
                "read_skill_file",
                json!({"name": "house-style", "path": "../../secrets.toml"}),
            ),
        ]),
        Turn::text("Loaded."),
    ])
    .await;
    env.store.create("s", "S", Author::User).await.unwrap();
    // A skill dropped into <data>/skills after start-up, with no restart.
    let dir = env._dir.path().join("skills/house-style");
    std::fs::create_dir_all(dir.join("references")).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: house-style\ndescription: Our house colours and tone. Use for every site.\n---\n\nAlways use teal headings.\n",
    )
    .unwrap();
    std::fs::write(dir.join("references/palette.md"), "teal: #0aa").unwrap();

    let run = env
        .agent
        .start(request("s", "restyle it", false))
        .await
        .unwrap();
    let (run, events) = finish(&env.agent, &run).await;
    assert_eq!(run.status, RunStatus::Succeeded);

    // The prompt carries the index only: names and descriptions, not instructions.
    let system = &env.provider.requests()[0].system;
    assert!(system.contains("- `house-style`: Our house colours and tone. Use for every site."));
    assert!(system.contains("- `create-site`:") && system.contains("- `setup-domain`:"));
    assert!(!system.contains("Always use teal headings"));
    assert!(
        !system.contains("Look before you edit"),
        "built-in instructions are loaded on demand too"
    );

    let results = tool_results(&events);
    assert_eq!(results[0].2["instructions"], "Always use teal headings.");
    assert_eq!(results[0].2["files"], json!(["references/palette.md"]));
    assert_eq!(results[1].2["content"], "teal: #0aa");
    // Events shorten long results for display; the model itself got the whole skill.
    assert!(!results[2].1);
    let full: Vec<String> = env.provider.requests()[1]
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::Tool { name, content, .. } if name == "load_skill" => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert!(full[1].contains("Look before you edit") && full[1].contains("Common requests"));
    assert!(
        results[3].1
            && results[3].2["error"]
                .as_str()
                .unwrap()
                .contains("available skills")
    );
    assert!(results[4].1, "skill files are confined to the skill folder");
}
