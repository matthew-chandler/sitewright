//! GitHub Copilot, through GitHub's Copilot SDK (ADR-0008).
//!
//! Copilot is not a plain chat API. GitHub's supported route is its runtime, a separate
//! program that runs its own agent loop and calls back for tools. This adapter makes
//! that look like any other [`LlmProvider`]:
//!
//! - The first call of a conversation starts the runtime, opens a session with
//!   Sitewright's system prompt and tools (and none of the runtime's own), and sends
//!   the prompt.
//! - When the model calls a tool, the runtime asks us to run it. We do not: we emit the
//!   call, end the response with [`StopReason::ToolUse`] and park the session with the
//!   runtime still waiting.
//! - The agent runs the tool as usual and calls [`LlmProvider::stream`] again. The new
//!   request ends with the tool results; their call ids find the parked session, the
//!   results are handed to the runtime, and the response continues.
//!
//! The runtime is started by this module, not by the SDK, so the SDK is built without
//! its launcher (which needs OpenSSL). It runs with an empty environment, its own home
//! directory and an empty working directory, and is stopped when no conversation uses
//! it.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use github_copilot_sdk::handler::{PermissionHandler, PermissionResult};
use github_copilot_sdk::session::Session;
use github_copilot_sdk::subscription::{EventSubscription, RecvErrorKind};
use github_copilot_sdk::tool::ToolHandler;
use github_copilot_sdk::types::{PermissionRequestData, PermissionRequestKind, RequestId};
use github_copilot_sdk::{
    Client, SessionConfig, SessionId, SystemMessageConfig, Tool, ToolInvocation, ToolResult,
    ToolResultExpanded,
};
use sw_core::config::ProviderConfig;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use crate::{
    ChatRequest, Event, EventStream, LlmError, LlmProvider, Message, StopReason, ToolCall, Usage,
};

/// Environment variable naming the runtime program, as the SDK itself uses.
pub const RUNTIME_ENV: &str = "COPILOT_CLI_PATH";
/// The variable the token is passed to the runtime in. Never on the command line.
const TOKEN_ENV: &str = "COPILOT_SDK_AUTH_TOKEN";
/// After a tool call arrives, how long to wait for more calls from the same model turn.
const SETTLE: Duration = Duration::from_millis(150);
/// How long the runtime may take to start and answer the handshake.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// A parked session nobody came back for is closed after this long.
const PARKED_TTL: Duration = Duration::from_secs(20 * 60);
/// How long a second conversation waits for the single runtime slot.
const SLOT_WAIT: Duration = Duration::from_secs(120);
/// Placeholder for the tool allowlist when a request has no tools, so the list is never
/// empty (an empty list could be read as "no restriction").
const NO_TOOLS: &str = "sitewright_no_tools";

static DEFAULT_RUNTIME: OnceLock<PathBuf> = OnceLock::new();

/// The runtime is large (hundreds of megabytes of memory), so only one runs at a time
/// in this process.
fn slot() -> Arc<Semaphore> {
    static SLOT: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SLOT.get_or_init(|| Arc::new(Semaphore::new(1))).clone()
}

/// Tell this module where `sitewright copilot install` puts the runtime. Called once at
/// start-up; later calls are ignored.
pub fn set_default_runtime(path: PathBuf) {
    let _ = DEFAULT_RUNTIME.set(path);
}

/// Where the runtime program is expected: `COPILOT_CLI_PATH` if set, else the default.
pub fn runtime_path() -> Option<PathBuf> {
    std::env::var_os(RUNTIME_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| DEFAULT_RUNTIME.get().cloned())
}

/// A connection to a running runtime.
pub struct Connection {
    pub client: Client,
    /// Anything that must live as long as the connection (the child process).
    pub guard: Box<dyn Send + Sync>,
    /// The runtime's process id, when it is a child of ours.
    pub pid: Option<u32>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

/// Starts the runtime. The real one launches a process; tests supply streams.
#[async_trait]
pub trait Connector: Send + Sync {
    async fn connect(&self) -> Result<Connection, LlmError>;
}

/// Launches the runtime program as a child process.
struct ProcessConnector {
    /// A GitHub token, or `None` to use the sign-in stored in the runtime's home.
    token: Option<String>,
    /// Without a token: fail early, with directions, when nobody is signed in.
    require_sign_in: bool,
}

/// The runtime's home directory, where it keeps a stored sign-in.
fn home_of(program: &Path) -> PathBuf {
    program
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("home")
}

#[async_trait]
impl Connector for ProcessConnector {
    async fn connect(&self) -> Result<Connection, LlmError> {
        let program = runtime_path().ok_or_else(not_installed)?;
        if !program.is_file() {
            return Err(not_installed());
        }
        let home = home_of(&program);
        let work = home.join("work");
        std::fs::create_dir_all(&work)
            .map_err(|e| LlmError::Config(format!("cannot create {}: {e}", work.display())))?;

        prepare_home(&home)?;

        let mut command = tokio::process::Command::new(&program);
        command
            .args(["--server", "--stdio", "--no-auto-update"])
            .env_clear()
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_STATE_HOME", home.join(".state"))
            .env("PATH", "/usr/bin:/bin")
            .env("COPILOT_TELEMETRY_DISABLED", "true")
            .current_dir(&work)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(token) = &self.token {
            command
                .args(["--auth-token-env", TOKEN_ENV, "--no-auto-login"])
                .env(TOKEN_ENV, token);
        }
        let mut child = command.spawn().map_err(|e| {
            LlmError::Config(format!(
                "cannot start the Copilot runtime {}: {e}",
                program.display()
            ))
        })?;
        let pid = child.id();
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(LlmError::Protocol(
                "the Copilot runtime has no pipes".into(),
            ));
        };

        // Keep the last lines the runtime wrote to stderr, to explain a failed start.
        let tail: Arc<Mutex<VecDeque<String>>> = Arc::default();
        let tail_writer = tail.clone();
        let token = self.token.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = match &token {
                    Some(token) => line.replace(token.as_str(), "[token]"),
                    None => line,
                };
                tracing::debug!(target: "sw_llm::copilot", "runtime: {line}");
                if let Ok(mut tail) = tail_writer.lock() {
                    if tail.len() == 12 {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
            }
        });

        let mut connected = connect_streams(stdout, stdin, work).await;
        if let (Ok(client), true) = (&connected, self.require_sign_in) {
            match client.get_auth_status().await {
                Ok(status) if status.is_authenticated => {}
                Ok(_) => connected = Err(not_signed_in()),
                Err(e) => connected = Err(sdk_error(e)),
            }
        }
        match connected {
            Ok(client) => Ok(Connection {
                client,
                guard: Box::new(child),
                pid,
            }),
            Err(e) => {
                let said = tail
                    .lock()
                    .map(|t| t.iter().cloned().collect::<Vec<_>>().join(" | "))
                    .unwrap_or_default();
                let _ = child.start_kill();
                Err(if said.is_empty() {
                    e
                } else {
                    LlmError::Protocol(format!("{e}. The runtime said: {said}"))
                })
            }
        }
    }
}

/// Build a client over the given streams and complete the handshake.
pub async fn connect_streams(
    reader: impl AsyncRead + Unpin + Send + 'static,
    writer: impl AsyncWrite + Unpin + Send + 'static,
    cwd: PathBuf,
) -> Result<Client, LlmError> {
    let client = Client::from_streams(reader, writer, cwd).map_err(sdk_error)?;
    match tokio::time::timeout(START_TIMEOUT, client.verify_protocol_version()).await {
        Ok(Ok(())) => Ok(client),
        Ok(Err(e)) => Err(sdk_error(e)),
        Err(_) => Err(LlmError::Timeout(START_TIMEOUT)),
    }
}

fn not_installed() -> LlmError {
    LlmError::Config(
        "the Copilot runtime is not installed; run `sitewright copilot install` on the server"
            .into(),
    )
}

/// The runtime's user settings file, inside its home.
const SETTINGS_FILE: &str = ".copilot/settings.json";
/// Lets the runtime keep a sign-in in its own files when there is no system keychain,
/// which a server never has. Without it the runtime completes the sign-in with GitHub
/// and then refuses to save it. Setting name from GitHub's Copilot CLI configuration
/// reference.
const SETTINGS: &str = "{\n  \"storeTokenPlaintext\": true\n}\n";

/// Make the runtime's home private and give it its settings, unless it already has
/// some.
fn prepare_home(home: &Path) -> Result<(), LlmError> {
    use std::os::unix::fs::PermissionsExt;
    let problem = |e: std::io::Error| LlmError::Config(format!("{}: {e}", home.display()));
    std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700)).map_err(problem)?;
    let file = home.join(SETTINGS_FILE);
    if !file.exists() {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(problem)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .map_err(problem)?;
        }
        std::fs::write(&file, SETTINGS).map_err(problem)?;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).map_err(problem)?;
    }
    Ok(())
}

fn not_signed_in() -> LlmError {
    LlmError::Auth {
        status: 401,
        message: "nobody is signed in to GitHub Copilot; sign in under Settings, or run `sitewright copilot login`".into(),
    }
}

fn sdk_error(e: github_copilot_sdk::Error) -> LlmError {
    LlmError::Protocol(format!("Copilot runtime: {e}"))
}

/// One started runtime. Dropped, and with it the process, when the last conversation
/// that uses it ends.
struct Running {
    client: Client,
    pid: Option<u32>,
    _guard: Box<dyn Send + Sync>,
    _slot: OwnedSemaphorePermit,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.client.force_stop();
    }
}

/// What the agent answered for one tool call.
struct ToolOutcome {
    content: String,
    is_error: bool,
}

/// A tool call the runtime is waiting on.
struct Invocation {
    call: ToolCall,
    reply: oneshot::Sender<ToolOutcome>,
}

/// Hands every tool call to the conversation instead of running it.
struct Forward {
    calls: mpsc::UnboundedSender<Invocation>,
}

#[async_trait]
impl ToolHandler for Forward {
    async fn call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<ToolResult, github_copilot_sdk::Error> {
        let (reply, answer) = oneshot::channel();
        let call = ToolCall {
            id: invocation.tool_call_id,
            name: invocation.tool_name,
            arguments: invocation.arguments,
        };
        let failure = |text: &str| {
            Ok(ToolResult::Expanded(ToolResultExpanded::new(
                text, "failure",
            )))
        };
        if self.calls.send(Invocation { call, reply }).is_err() {
            return failure("The run has ended.");
        }
        match answer.await {
            Ok(outcome) if outcome.is_error => failure(&outcome.content),
            Ok(outcome) => Ok(ToolResult::Text(outcome.content)),
            Err(_) => failure("The run ended before this tool finished."),
        }
    }
}

/// Approves Sitewright's own tools and refuses everything else the runtime might try:
/// shell commands, file access, URLs, MCP servers, memory, hooks.
struct OnlyOurTools;

#[async_trait]
impl PermissionHandler for OnlyOurTools {
    async fn handle(
        &self,
        _session: SessionId,
        _request: RequestId,
        data: PermissionRequestData,
    ) -> PermissionResult {
        if matches!(data.kind, Some(PermissionRequestKind::CustomTool)) {
            PermissionResult::approve_once()
        } else {
            tracing::warn!(target: "sw_llm::copilot", kind = ?data.kind, "refused a runtime permission request");
            PermissionResult::reject(Some(
                "This is not available here. Use only the listed tools.".to_owned(),
            ))
        }
    }
}

/// A live session with the runtime.
struct Conversation {
    session: Session,
    events: EventSubscription,
    calls: mpsc::UnboundedReceiver<Invocation>,
    /// Keeps `calls` open when the request has no tools (and so no handler holds it).
    _calls_open: mpsc::UnboundedSender<Invocation>,
    /// Calls handed to the agent and not yet answered, by call id.
    replies: HashMap<String, oneshot::Sender<ToolOutcome>>,
    /// Whether text for the current assistant message arrived as deltas.
    saw_delta: bool,
    _runtime: Arc<Running>,
}

struct Parked {
    conversation: Conversation,
    since: Instant,
}

#[derive(Default)]
struct State {
    /// Call id to the parked conversation waiting for it.
    waiting: HashMap<String, u64>,
    parked: HashMap<u64, Parked>,
    next: u64,
    runtime: Weak<Running>,
}

struct Shared {
    connector: Box<dyn Connector>,
    state: Mutex<State>,
    /// Serializes runtime start-up so two first calls do not start two runtimes.
    starting: tokio::sync::Mutex<()>,
}

/// GitHub Copilot as a provider.
pub struct Copilot {
    id: String,
    model: String,
    idle: Duration,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Copilot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Copilot")
            .field("id", &self.id)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl Copilot {
    /// With a token (the provider's key), sign in with it. Without one, use the GitHub
    /// sign-in stored by `sitewright copilot login` or the Settings page.
    pub fn new(config: &ProviderConfig, token: Option<String>) -> Result<Self, LlmError> {
        let token = token.filter(|t| !t.trim().is_empty());
        let require_sign_in = token.is_none();
        Ok(Self::with_connector(
            config,
            Box::new(ProcessConnector {
                token,
                require_sign_in,
            }),
        ))
    }

    /// Use a custom way of reaching the runtime. Tests use this with in-memory streams.
    pub fn with_connector(config: &ProviderConfig, connector: Box<dyn Connector>) -> Self {
        Self {
            id: config.id.clone(),
            model: config.model.clone(),
            idle: Duration::from_secs(config.timeout_secs.max(1)),
            shared: Arc::new(Shared {
                connector,
                state: Mutex::default(),
                starting: tokio::sync::Mutex::default(),
            }),
        }
    }

    /// The process id of the runtime, while one is running. For memory reporting.
    pub fn runtime_pid(&self) -> Option<u32> {
        let state = self.shared.state.lock().ok()?;
        state.runtime.upgrade().and_then(|r| r.pid)
    }

    /// Start (or reuse) the runtime and ask it which models the account may use.
    pub async fn models(&self) -> Result<Vec<String>, LlmError> {
        let runtime = self.shared.runtime().await?;
        let models = runtime.client.list_models().await.map_err(sdk_error)?;
        Ok(models.into_iter().map(|m| m.id).collect())
    }
}

/// Take the single runtime slot and start a runtime.
async fn start(connector: &dyn Connector) -> Result<Arc<Running>, LlmError> {
    let permit = tokio::time::timeout(SLOT_WAIT, slot().acquire_owned())
        .await
        .map_err(|_| {
            LlmError::Config(
                "another Copilot conversation is running; only one runs at a time".into(),
            )
        })?
        .map_err(|_| LlmError::Config("the Copilot slot is closed".into()))?;
    let connection = connector.connect().await?;
    Ok(Arc::new(Running {
        client: connection.client,
        pid: connection.pid,
        _guard: connection.guard,
        _slot: permit,
    }))
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn runtime(&self) -> Result<Arc<Running>, LlmError> {
        let _starting = self.starting.lock().await;
        if let Some(running) = self.lock().runtime.upgrade() {
            return Ok(running);
        }
        let running = start(self.connector.as_ref()).await?;
        self.lock().runtime = Arc::downgrade(&running);
        Ok(running)
    }

    /// Take the parked conversation that is waiting for `call_id`.
    fn unpark(&self, call_id: &str) -> Option<Conversation> {
        let mut state = self.lock();
        let key = state.waiting.remove(call_id)?;
        state.waiting.retain(|_, k| *k != key);
        state.parked.remove(&key).map(|p| p.conversation)
    }

    fn park(&self, conversation: Conversation) {
        let mut state = self.lock();
        let key = state.next;
        state.next += 1;
        for id in conversation.replies.keys() {
            state.waiting.insert(id.clone(), key);
        }
        state.parked.insert(
            key,
            Parked {
                conversation,
                since: Instant::now(),
            },
        );
    }

    /// Close parked conversations nobody came back for.
    fn reap(&self) {
        let stale: Vec<Parked> = {
            let mut state = self.lock();
            let old: Vec<u64> = state
                .parked
                .iter()
                .filter(|(_, p)| p.since.elapsed() > PARKED_TTL)
                .map(|(k, _)| *k)
                .collect();
            state.waiting.retain(|_, k| !old.contains(k));
            old.iter().filter_map(|k| state.parked.remove(k)).collect()
        };
        drop(stale);
    }
}

#[async_trait]
impl LlmProvider for Copilot {
    fn id(&self) -> &str {
        &self.id
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        self.shared.reap();

        // Tool results at the end of the request continue a parked conversation.
        let results: Vec<(&str, &str, bool)> = request
            .messages
            .iter()
            .rev()
            .map_while(|m| match m {
                Message::Tool {
                    call_id,
                    content,
                    is_error,
                    ..
                } => Some((call_id.as_str(), content.as_str(), *is_error)),
                _ => None,
            })
            .collect();
        let resumed = results
            .first()
            .and_then(|(call_id, _, _)| self.shared.unpark(call_id));

        let conversation = match resumed {
            Some(mut conversation) => {
                for (call_id, content, is_error) in results {
                    if let Some(reply) = conversation.replies.remove(call_id) {
                        let _ = reply.send(ToolOutcome {
                            content: content.to_owned(),
                            is_error,
                        });
                    }
                }
                // Anything still unanswered will never be: fail it so the runtime moves on.
                for (_, reply) in conversation.replies.drain() {
                    let _ = reply.send(ToolOutcome {
                        content: "No result was returned for this call.".into(),
                        is_error: true,
                    });
                }
                conversation
            }
            None => self.open(&request).await?,
        };

        let (events, receiver) = mpsc::channel(64);
        tokio::spawn(pump(conversation, events, self.shared.clone(), self.idle));
        Ok(Box::pin(futures_util::stream::unfold(
            receiver,
            |mut receiver| async move { receiver.recv().await.map(|item| (item, receiver)) },
        )))
    }
}

impl Copilot {
    /// Open a session for a new conversation and send its prompt.
    async fn open(&self, request: &ChatRequest) -> Result<Conversation, LlmError> {
        let runtime = self.shared.runtime().await?;
        let (calls_in, calls) = mpsc::unbounded_channel();
        let handler: Arc<dyn ToolHandler> = Arc::new(Forward {
            calls: calls_in.clone(),
        });
        let tools: Vec<Tool> = request
            .tools
            .iter()
            .map(|t| {
                Tool::new(t.name.clone())
                    .with_description(t.description.clone())
                    .with_parameters(t.parameters.clone())
                    .with_overrides_built_in_tool(true)
                    .with_handler(handler.clone())
            })
            .collect();
        let mut allowed: Vec<String> = request.tools.iter().map(|t| t.name.clone()).collect();
        if allowed.is_empty() {
            allowed.push(NO_TOOLS.into());
        }
        let config = SessionConfig::default()
            .with_model(self.model.clone())
            .with_streaming(true)
            .with_system_message(
                SystemMessageConfig::new()
                    .with_mode("replace")
                    .with_content(request.system.clone()),
            )
            .with_tools(tools)
            .with_available_tools(allowed)
            .with_enable_skills(false)
            .with_enable_session_telemetry(false)
            .with_permission_handler(Arc::new(OnlyOurTools));
        let session = runtime
            .client
            .create_session(config)
            .await
            .map_err(sdk_error)?;
        let events = session.subscribe();
        session
            .send(prompt_of(&request.messages))
            .await
            .map_err(sdk_error)?;
        Ok(Conversation {
            session,
            events,
            calls,
            _calls_open: calls_in,
            replies: HashMap::new(),
            saw_delta: false,
            _runtime: runtime,
        })
    }
}

/// The text to send as the user's message. A fresh conversation is just the prompt. If
/// the request carries earlier turns that no live session knows about (the session was
/// lost), they are replayed as a transcript so the model can pick up where it was.
fn prompt_of(messages: &[Message]) -> String {
    if let [Message::User { content }] = messages {
        return content.clone();
    }
    let mut out = String::from(
        "Continue this work. The conversation so far, including tool calls already made and their results:\n",
    );
    for message in messages {
        match message {
            Message::User { content } => out.push_str(&format!("\n[user]\n{content}\n")),
            Message::Assistant {
                text, tool_calls, ..
            } => {
                out.push_str(&format!("\n[assistant]\n{text}\n"));
                for call in tool_calls {
                    out.push_str(&format!("(called {} with {})\n", call.name, call.arguments));
                }
            }
            Message::Tool {
                name,
                content,
                is_error,
                ..
            } => {
                let label = if *is_error { "error" } else { "result" };
                out.push_str(&format!("\n[{label} of {name}]\n{content}\n"));
            }
        }
    }
    out
}

type Sink = mpsc::Sender<Result<Event, LlmError>>;

/// Why one response ended.
enum End {
    /// Tool calls were emitted; the conversation waits for their results.
    Park,
    /// The conversation is over (finished, failed or abandoned).
    Close,
}

/// Forward one response's worth of runtime events, then park or close the conversation.
async fn pump(mut conversation: Conversation, sink: Sink, shared: Arc<Shared>, idle: Duration) {
    match respond(&mut conversation, &sink, idle).await {
        End::Park => shared.park(conversation),
        End::Close => {
            // Stop any work in flight; dropping the session then releases the runtime.
            let _ =
                tokio::time::timeout(Duration::from_secs(5), conversation.session.abort()).await;
        }
    }
}

async fn respond(conversation: &mut Conversation, sink: &Sink, idle: Duration) -> End {
    let mut usage = Usage::default();
    let mut called = false;
    loop {
        // Once a tool call has arrived, wait only briefly for the rest of the batch.
        let wait = if called { SETTLE } else { idle };
        tokio::select! {
            _ = sink.closed() => return End::Close,
            invocation = conversation.calls.recv() => {
                let Some(Invocation { call, reply }) = invocation else {
                    let _ = sink.send(Err(LlmError::Protocol("the Copilot session ended".into()))).await;
                    return End::Close;
                };
                conversation.replies.insert(call.id.clone(), reply);
                called = true;
                if sink.send(Ok(Event::ToolCall(call))).await.is_err() {
                    return End::Close;
                }
            }
            event = conversation.events.recv() => {
                let event = match event {
                    Ok(event) => event,
                    Err(e) if matches!(e.kind(), RecvErrorKind::Lagged(_)) => continue,
                    Err(_) => {
                        let _ = sink.send(Err(LlmError::Protocol("the Copilot runtime stopped".into()))).await;
                        return End::Close;
                    }
                };
                // Only the main agent's output is the answer.
                if event.agent_id.is_some() {
                    continue;
                }
                let data = &event.data;
                let out = match event.event_type.as_str() {
                    "assistant.message_delta" => {
                        conversation.saw_delta = true;
                        text_of(data, &["deltaContent", "delta"]).map(Event::TextDelta)
                    }
                    "assistant.message" => {
                        let streamed = std::mem::take(&mut conversation.saw_delta);
                        if streamed { None } else { text_of(data, &["content"]).map(Event::TextDelta) }
                    }
                    "assistant.usage" => {
                        usage.input_tokens += count(data, "inputTokens");
                        usage.output_tokens += count(data, "outputTokens");
                        Some(Event::Usage(usage))
                    }
                    "session.idle" if !called => {
                        let _ = sink.send(Ok(Event::Stop(StopReason::EndTurn))).await;
                        return End::Close;
                    }
                    "session.error" => {
                        let message = text_of(data, &["message"]).unwrap_or_else(|| "unknown error".into());
                        let _ = sink.send(Err(classify(data, message))).await;
                        return End::Close;
                    }
                    _ => None,
                };
                if let Some(out) = out
                    && sink.send(Ok(out)).await.is_err()
                {
                    return End::Close;
                }
            }
            _ = tokio::time::sleep(wait) => {
                if called {
                    let _ = sink.send(Ok(Event::Stop(StopReason::ToolUse))).await;
                    return End::Park;
                }
                let _ = sink.send(Err(LlmError::Timeout(idle))).await;
                return End::Close;
            }
        }
    }
}

fn text_of(data: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| data.get(k).and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn count(data: &serde_json::Value, key: &str) -> u64 {
    data.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// Map a `session.error` to the closest [`LlmError`].
fn classify(data: &serde_json::Value, message: String) -> LlmError {
    let kind = data
        .get("errorType")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if kind.contains("auth") {
        LlmError::Auth {
            status: 401,
            message,
        }
    } else if kind.contains("rate") || kind.contains("quota") {
        LlmError::RateLimited {
            message,
            retry_after: None,
        }
    } else {
        LlmError::Protocol(format!("Copilot: {message}"))
    }
}

// ---- Signing in with a GitHub account ----------------------------------------------
//
// The runtime is GitHub's own client, so it can run GitHub's device sign-in itself:
// it gives a short code, the owner enters it at github.com, and the runtime stores the
// resulting credentials in its home directory. Sitewright never sees them.

/// Where a sign-in stands.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SignIn {
    /// No sign-in has been started in this process.
    Idle,
    /// Waiting for the owner to enter `user_code` at `verification_uri`.
    Waiting {
        user_code: String,
        verification_uri: String,
    },
    Done {
        /// The GitHub user name, when the runtime reported it.
        login: Option<String>,
    },
    Failed {
        message: String,
    },
}

fn sign_in_cell() -> &'static Mutex<SignIn> {
    static STATE: OnceLock<Mutex<SignIn>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(SignIn::Idle))
}

fn set_sign_in(state: SignIn) {
    *sign_in_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = state;
}

/// The latest sign-in state.
pub fn sign_in_state() -> SignIn {
    sign_in_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Start a device sign-in and return the code to show. The rest happens in the
/// background; follow it with [`sign_in_state`]. If one is already waiting, its code is
/// returned again.
pub async fn sign_in_start() -> Result<SignIn, LlmError> {
    let current = sign_in_state();
    if matches!(current, SignIn::Waiting { .. }) {
        return Ok(current);
    }
    let connector = ProcessConnector {
        token: None,
        require_sign_in: false,
    };
    sign_in_with(&connector).await
}

/// [`sign_in_start`] over any connector. Tests use this.
pub async fn sign_in_with(connector: &dyn Connector) -> Result<SignIn, LlmError> {
    use github_copilot_sdk::rpc::{
        AuthLoginAdvanceRequest, AuthLoginBeginOptions, AuthLoginMethod, AuthLoginResultStatus,
        AuthLoginStep, LoginProviderKind,
    };
    let runtime = start(connector).await?;
    // The sign-in calls belong to a session. This one can do nothing else.
    let session = runtime
        .client
        .create_session(
            SessionConfig::default()
                .with_available_tools([NO_TOOLS])
                .with_enable_skills(false)
                .with_enable_session_telemetry(false)
                .with_permission_handler(Arc::new(OnlyOurTools)),
        )
        .await
        .map_err(sdk_error)?;
    let begun = session
        .rpc()
        .accounts()
        .login()
        .begin_with_options(
            AuthLoginBeginOptions::new(LoginProviderKind::GitHubDotCom)
                .method(AuthLoginMethod::DeviceCode),
        )
        .await
        .map_err(sdk_error)?;
    let (waiting, lifetime) = match begun.step {
        AuthLoginStep::DeviceCode(code) => (
            SignIn::Waiting {
                user_code: code.user_code,
                verification_uri: code.verification_uri,
            },
            Duration::from_secs(code.expires_in.clamp(60, 1800) as u64),
        ),
        AuthLoginStep::Error(e) => return Err(LlmError::Protocol(e.message)),
        _ => {
            return Err(LlmError::Protocol(
                "the Copilot runtime did not offer a device code".into(),
            ));
        }
    };
    set_sign_in(waiting.clone());

    let flow_id = begun.flow_id;
    tokio::spawn(async move {
        // Holds the runtime (and the single slot) until the owner finishes or the code
        // expires.
        let _runtime = runtime;
        let advance = async {
            let mut consented = false;
            let mut input: Option<String> = None;
            loop {
                let step = session
                    .rpc()
                    .accounts()
                    .login()
                    .advance(AuthLoginAdvanceRequest {
                        flow_id: flow_id.clone(),
                        input: input.take(),
                    })
                    .await;
                match step {
                    Ok(AuthLoginStep::Completed(done)) => match done.result.status {
                        AuthLoginResultStatus::Completed => {
                            return SignIn::Done {
                                login: done.result.login,
                            };
                        }
                        // A server has no keychain, so the runtime asks before keeping
                        // the credentials in a file. That file is in the owner-only
                        // data directory, like Sitewright's other keys: agree, once.
                        AuthLoginResultStatus::NeedsPlaintextConsent if !consented => {
                            consented = true;
                            input = Some("yes".to_owned());
                        }
                        other => {
                            return SignIn::Failed {
                                message: format!(
                                    "GitHub accepted the code, but the runtime could not finish signing in ({other:?})"
                                ),
                            };
                        }
                    },
                    Ok(AuthLoginStep::Error(e)) => return SignIn::Failed { message: e.message },
                    Ok(_) => tokio::time::sleep(Duration::from_secs(2)).await,
                    Err(e) => {
                        return SignIn::Failed {
                            message: sdk_error(e).to_string(),
                        };
                    }
                }
            }
        };
        let outcome = tokio::time::timeout(lifetime, advance)
            .await
            .unwrap_or(SignIn::Failed {
                message: "the code expired before it was entered; start again".into(),
            });
        set_sign_in(outcome);
    });
    Ok(waiting)
}

/// Who is signed in, by starting the runtime briefly and asking. `Ok(None)` means
/// nobody is.
pub async fn account() -> Result<Option<String>, LlmError> {
    let connector = ProcessConnector {
        token: None,
        require_sign_in: false,
    };
    let runtime = start(&connector).await?;
    let status = runtime.client.get_auth_status().await.map_err(sdk_error)?;
    Ok(status
        .is_authenticated
        .then(|| status.login.unwrap_or_else(|| "a GitHub account".into())))
}

/// A model the signed-in account may use.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ModelChoice {
    /// What goes in a provider's `model` field.
    pub id: String,
    /// The name GitHub shows for it.
    pub name: String,
}

fn models_cell() -> &'static Mutex<Option<Vec<ModelChoice>>> {
    static MODELS: OnceLock<Mutex<Option<Vec<ModelChoice>>>> = OnceLock::new();
    MODELS.get_or_init(|| Mutex::new(None))
}

/// The models of the stored sign-in. The first call (and every call with `refresh`)
/// starts the runtime to ask; the answer is then remembered until the daemon restarts
/// or the owner signs out.
pub async fn available_models(refresh: bool) -> Result<Vec<ModelChoice>, LlmError> {
    let connector = ProcessConnector {
        token: None,
        require_sign_in: true,
    };
    available_models_with(&connector, refresh).await
}

/// [`available_models`] over any connector. Tests use this.
pub async fn available_models_with(
    connector: &dyn Connector,
    refresh: bool,
) -> Result<Vec<ModelChoice>, LlmError> {
    let remembered = models_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let (Some(models), false) = (remembered, refresh) {
        return Ok(models);
    }
    let runtime = start(connector).await?;
    let mut models: Vec<ModelChoice> = runtime
        .client
        .list_models()
        .await
        .map_err(sdk_error)?
        .into_iter()
        .map(|m| ModelChoice {
            id: m.id,
            name: m.name,
        })
        .collect();
    models.sort_by_key(|m| m.name.to_lowercase());
    *models_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(models.clone());
    Ok(models)
}

/// Forget the stored sign-in by removing the runtime's home directory.
pub fn sign_out() -> std::io::Result<()> {
    set_sign_in(SignIn::Idle);
    *models_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    let Some(program) = runtime_path() else {
        return Ok(());
    };
    match std::fs::remove_dir_all(home_of(&program)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_conversation_sends_the_prompt_unchanged() {
        let messages = vec![Message::User {
            content: "Make a page".into(),
        }];
        assert_eq!(prompt_of(&messages), "Make a page");
    }

    #[test]
    fn a_lost_session_is_replayed_as_a_transcript() {
        let messages = vec![
            Message::User {
                content: "Make a page".into(),
            },
            Message::Assistant {
                text: "Writing it.".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "write_file".into(),
                    arguments: serde_json::json!({"path": "index.html"}),
                }],
                raw: None,
            },
            Message::Tool {
                call_id: "c1".into(),
                name: "write_file".into(),
                content: "wrote 10 bytes".into(),
                is_error: false,
            },
        ];
        let prompt = prompt_of(&messages);
        assert!(prompt.contains("[user]\nMake a page"));
        assert!(prompt.contains("(called write_file with {\"path\":\"index.html\"})"));
        assert!(prompt.contains("[result of write_file]\nwrote 10 bytes"));
    }

    #[test]
    fn the_runtime_home_is_private_and_allows_a_stored_sign_in() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("sw-copilot-home-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).expect("create");
        prepare_home(&dir).expect("prepare");
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join(SETTINGS_FILE)).expect("settings"),
        )
        .expect("json");
        assert_eq!(settings["storeTokenPlaintext"], true);
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(SETTINGS_FILE)), 0o600);
        // Settings the owner changed are left alone.
        std::fs::write(dir.join(SETTINGS_FILE), "{}").expect("write");
        prepare_home(&dir).expect("prepare again");
        assert_eq!(
            std::fs::read_to_string(dir.join(SETTINGS_FILE)).expect("read"),
            "{}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_errors_are_classified() {
        let data = serde_json::json!({"errorType": "authentication"});
        assert!(matches!(
            classify(&data, "no".into()),
            LlmError::Auth { .. }
        ));
        let data = serde_json::json!({"errorType": "rate_limit"});
        assert!(matches!(
            classify(&data, "slow".into()),
            LlmError::RateLimited { .. }
        ));
        assert!(matches!(
            classify(&serde_json::json!({}), "x".into()),
            LlmError::Protocol(_)
        ));
    }
}
