//! The daemon's side of backends: start workers, hand them requests, answer their host
//! calls and stop them when they misbehave or sit idle.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use crate::protocol::{FromWorker, MAX_FRAME_BYTES, RequestMsg, ResponseMsg, ToWorker};

/// How to start a worker process.
#[derive(Debug, Clone)]
pub struct WorkerCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// Limits that apply to every backend.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Longest a handler may run.
    pub request_timeout: Duration,
    /// JavaScript heap per worker.
    pub memory_bytes: u64,
    pub stack_bytes: u64,
    pub max_request_body: usize,
    pub max_response_body: usize,
    /// A worker with no requests for this long is stopped.
    pub idle: Duration,
    /// Most workers alive at once, across all sites.
    pub max_workers: usize,
    /// Longest a request waits for its site's worker to become free.
    pub queue_wait: Duration,
    /// Longest a request may take in all, including the time the daemon spends on its
    /// host calls (queries, outside requests, model calls), which `request_timeout`
    /// does not count.
    pub max_wall: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(10),
            memory_bytes: 64 * 1024 * 1024,
            stack_bytes: 512 * 1024,
            max_request_body: 1024 * 1024,
            max_response_body: 4 * 1024 * 1024,
            idle: Duration::from_secs(300),
            max_workers: 8,
            queue_wait: Duration::from_secs(10),
            max_wall: Duration::from_secs(90),
        }
    }
}

/// Which backend a request is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub site_id: i64,
    pub site: String,
    /// The version of the code: a release sha, or a digest of a draft. A worker is
    /// replaced when this changes.
    pub version: String,
    /// Drafts (an agent testing unpublished code) get their own worker, apart from the
    /// live one.
    pub draft: bool,
}

/// What backends may ask the daemon to do. Implemented by the daemon; every call names
/// the site it came from, and the implementation must confine it to that site.
pub trait Host: Send + Sync + 'static {
    fn call<'a>(
        &'a self,
        target: &'a Target,
        op: &'a str,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;

    /// A line of console output from the backend.
    fn log(&self, target: &Target, level: &str, message: &str);
}

/// An HTTP request for a backend.
#[derive(Debug, Clone)]
pub struct BackendRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

pub type BackendResponse = ResponseMsg;

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("the request or response is too large")]
    TooLarge,
    #[error("the backend is busy")]
    Busy,
    #[error("could not start the backend worker: {0}")]
    Spawn(String),
    /// The backend's code does not load: a syntax error, a missing file or export.
    #[error("the backend failed to load: {0}")]
    Load(String),
    #[error("the backend took too long to answer")]
    Timeout,
    /// The handler failed in a way it could not turn into a response.
    #[error("the backend failed: {0}")]
    Failed(String),
    #[error("the backend worker stopped unexpectedly: {0}")]
    Crashed(String),
}

struct Worker {
    version: String,
    /// Dropping the child kills it.
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Worker {
    async fn send(&mut self, message: &ToWorker) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(message)?;
        let len = u32::try_from(bytes.len())
            .ok()
            .filter(|_| bytes.len() <= MAX_FRAME_BYTES)
            .ok_or_else(|| std::io::Error::other("message too large"))?;
        self.stdin.write_all(&len.to_be_bytes()).await?;
        self.stdin.write_all(&bytes).await?;
        self.stdin.flush().await
    }

    async fn receive(&mut self) -> std::io::Result<FromWorker> {
        let len = self.stdout.read_u32().await? as usize;
        if len > MAX_FRAME_BYTES {
            return Err(std::io::Error::other("message too large"));
        }
        let mut bytes = vec![0u8; len];
        self.stdout.read_exact(&mut bytes).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

#[derive(Default)]
struct Slot {
    worker: Mutex<Option<Worker>>,
    /// Milliseconds since the manager started.
    last_used: AtomicU64,
    /// Position in the order of use, for choosing which idle worker to stop first.
    order: AtomicU64,
}

struct Inner {
    command: WorkerCommand,
    limits: Limits,
    host: Arc<dyn Host>,
    slots: StdMutex<HashMap<(i64, bool), Arc<Slot>>>,
    started: Instant,
    uses: AtomicU64,
}

/// Runs site backends. Cheap to clone.
#[derive(Clone)]
pub struct Backends {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Backends {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backends")
            .field("command", &self.inner.command)
            .field("limits", &self.inner.limits)
            .finish_non_exhaustive()
    }
}

impl Backends {
    pub fn new(command: WorkerCommand, limits: Limits, host: Arc<dyn Host>) -> Self {
        Self {
            inner: Arc::new(Inner {
                command,
                limits,
                host,
                slots: StdMutex::new(HashMap::new()),
                started: Instant::now(),
                uses: AtomicU64::new(0),
            }),
        }
    }

    pub fn limits(&self) -> &Limits {
        &self.inner.limits
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.inner.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn slots(&self) -> std::sync::MutexGuard<'_, HashMap<(i64, bool), Arc<Slot>>> {
        self.inner.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run one request through a site's backend, starting its worker if needed.
    /// `files` supplies the backend's source when a worker has to be started.
    pub async fn handle<F>(
        &self,
        target: &Target,
        files: F,
        request: BackendRequest,
    ) -> Result<BackendResponse, BackendError>
    where
        F: FnOnce() -> Result<BTreeMap<String, String>, String>,
    {
        let limits = &self.inner.limits;
        if request.body.len() > limits.max_request_body {
            return Err(BackendError::TooLarge);
        }
        let slot = self
            .slots()
            .entry((target.site_id, target.draft))
            .or_default()
            .clone();
        // One request at a time per site; the rest wait their turn, but not for ever.
        let mut guard = tokio::time::timeout(limits.queue_wait, slot.worker.lock())
            .await
            .map_err(|_| BackendError::Busy)?;
        slot.last_used.store(self.now_ms(), Ordering::Relaxed);
        slot.order.store(
            self.inner.uses.fetch_add(1, Ordering::Relaxed),
            Ordering::Relaxed,
        );

        if guard.as_ref().is_some_and(|w| w.version != target.version) {
            *guard = None; // new code: replace the worker
        }
        if guard.is_none() {
            self.make_room(&slot);
            let files = files().map_err(BackendError::Load)?;
            *guard = Some(self.start(target, files).await?);
        }
        let worker = guard.as_mut().ok_or(BackendError::Busy)?;

        let message = ToWorker::Request(RequestMsg {
            method: request.method,
            path: request.path,
            query: request.query,
            headers: request.headers,
            body: request.body,
            timeout_ms: u64::try_from(limits.request_timeout.as_millis()).unwrap_or(u64::MAX),
        });
        // The worker enforces the handler's own time limit and does not count the time
        // it waits for host calls. Here: the same budget (plus a second, for a worker
        // that cannot enforce it), extended by the time host calls actually took, and
        // an overall ceiling.
        let budget = limits.request_timeout + Duration::from_secs(1);
        let outcome = tokio::time::timeout(
            limits.max_wall,
            self.converse(target, worker, message, budget),
        )
        .await;
        slot.last_used.store(self.now_ms(), Ordering::Relaxed);
        match outcome {
            Ok(Ok(response)) if response.body.len() > limits.max_response_body => {
                Err(BackendError::TooLarge)
            }
            Ok(Ok(response)) => Ok(response),
            Ok(Err((error, keep))) => {
                if !keep {
                    *guard = None;
                }
                Err(error)
            }
            Err(_) => {
                *guard = None;
                Err(BackendError::Timeout)
            }
        }
    }

    /// Send a message and serve the worker's host calls until it answers. On failure
    /// the flag says whether the worker can be kept.
    async fn converse(
        &self,
        target: &Target,
        worker: &mut Worker,
        message: ToWorker,
        budget: Duration,
    ) -> Result<BackendResponse, (BackendError, bool)> {
        let crashed = |e: std::io::Error| (BackendError::Crashed(e.to_string()), false);
        worker.send(&message).await.map_err(crashed)?;
        let mut deadline = tokio::time::Instant::now() + budget;
        loop {
            let frame = tokio::time::timeout_at(deadline, worker.receive())
                .await
                .map_err(|_| (BackendError::Timeout, false))?;
            match frame.map_err(crashed)? {
                FromWorker::Response(response) => return Ok(response),
                FromWorker::Failed { message, fatal } => {
                    let error = if message.contains("took too long") {
                        BackendError::Timeout
                    } else {
                        BackendError::Failed(message)
                    };
                    return Err((error, !fatal));
                }
                FromWorker::Log { level, message } => {
                    self.inner.host.log(target, &level, &message);
                }
                FromWorker::HostCall { op, args } => {
                    let asked = tokio::time::Instant::now();
                    let answer = self.inner.host.call(target, &op, args).await;
                    deadline += asked.elapsed();
                    let reply = match answer {
                        Ok(value) => ToWorker::HostReply {
                            ok: Some(value),
                            error: None,
                        },
                        Err(error) => ToWorker::HostReply {
                            ok: None,
                            error: Some(error),
                        },
                    };
                    worker.send(&reply).await.map_err(crashed)?;
                }
                FromWorker::Ready => {
                    return Err((
                        BackendError::Crashed("unexpected message from the worker".into()),
                        false,
                    ));
                }
            }
        }
    }

    /// Check that a backend's code loads, in a worker that is then discarded.
    pub async fn check(
        &self,
        target: &Target,
        files: BTreeMap<String, String>,
    ) -> Result<(), BackendError> {
        self.start(target, files).await.map(|_| ())
    }

    /// Start a worker and load the backend into it.
    async fn start(
        &self,
        target: &Target,
        files: BTreeMap<String, String>,
    ) -> Result<Worker, BackendError> {
        let command = &self.inner.command;
        let mut child = Command::new(&command.program)
            .args(&command.args)
            // The worker gets nothing from the daemon's environment.
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| BackendError::Spawn(e.to_string()))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(BackendError::Spawn("the worker has no pipes".into()));
        };
        let mut worker = Worker {
            version: target.version.clone(),
            _child: child,
            stdin,
            stdout: BufReader::new(stdout),
        };
        let init = ToWorker::Init {
            files,
            memory_bytes: self.inner.limits.memory_bytes,
            stack_bytes: self.inner.limits.stack_bytes,
        };
        let load = async {
            worker
                .send(&init)
                .await
                .map_err(|e| BackendError::Spawn(e.to_string()))?;
            loop {
                match worker.receive().await {
                    Ok(FromWorker::Ready) => return Ok(()),
                    Ok(FromWorker::Failed { message, .. }) => {
                        return Err(BackendError::Load(message));
                    }
                    Ok(FromWorker::Log { level, message }) => {
                        self.inner.host.log(target, &level, &message);
                    }
                    // Nothing may reach outside while the code is only being loaded.
                    Ok(FromWorker::HostCall { .. }) => {
                        let reply = ToWorker::HostReply {
                            ok: None,
                            error: Some(
                                "not available while the backend is loading; do it inside a handler"
                                    .into(),
                            ),
                        };
                        worker
                            .send(&reply)
                            .await
                            .map_err(|e| BackendError::Crashed(e.to_string()))?;
                    }
                    Ok(FromWorker::Response(_)) => {
                        return Err(BackendError::Crashed(
                            "unexpected message from the worker".into(),
                        ));
                    }
                    Err(e) => return Err(BackendError::Crashed(e.to_string())),
                }
            }
        };
        match tokio::time::timeout(Duration::from_secs(10), load).await {
            Ok(Ok(())) => Ok(worker),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(BackendError::Load("loading took too long".into())),
        }
    }

    /// Before starting a worker: if the limit is reached, stop the one that has been
    /// idle longest. Workers in the middle of a request are left alone.
    fn make_room(&self, starting: &Arc<Slot>) {
        let slots: Vec<Arc<Slot>> = self.slots().values().cloned().collect();
        let mut idle: Vec<(u64, Arc<Slot>)> = Vec::new();
        let mut alive = 0usize;
        for slot in slots {
            if Arc::ptr_eq(&slot, starting) {
                continue;
            }
            match slot.worker.try_lock() {
                Ok(guard) if guard.is_some() => {
                    alive += 1;
                    idle.push((slot.order.load(Ordering::Relaxed), slot.clone()));
                }
                Ok(_) => {}
                Err(_) => alive += 1, // busy
            }
        }
        idle.sort_by_key(|(order, _)| *order);
        let mut idle = idle.into_iter();
        while alive + 1 > self.inner.limits.max_workers {
            let Some((_, slot)) = idle.next() else { break };
            if let Ok(mut guard) = slot.worker.try_lock()
                && guard.take().is_some()
            {
                alive -= 1;
            }
        }
    }

    /// Stop workers that have been idle for longer than the limit. Call periodically.
    pub fn reap_idle(&self) -> usize {
        let cutoff = self
            .now_ms()
            .saturating_sub(u64::try_from(self.inner.limits.idle.as_millis()).unwrap_or(u64::MAX));
        let mut stopped = 0;
        let mut slots = self.slots();
        slots.retain(|_, slot| {
            if slot.last_used.load(Ordering::Relaxed) > cutoff {
                return true;
            }
            match slot.worker.try_lock() {
                Ok(mut guard) => {
                    if guard.take().is_some() {
                        stopped += 1;
                    }
                    // Keep the slot if someone else is about to use it.
                    Arc::strong_count(slot) > 1
                }
                Err(_) => true,
            }
        });
        stopped
    }

    /// Stop a site's workers, for example when the site is deleted or renamed.
    pub async fn stop(&self, site_id: i64) {
        let slots: Vec<Arc<Slot>> = self
            .slots()
            .iter()
            .filter(|((id, _), _)| *id == site_id)
            .map(|(_, slot)| slot.clone())
            .collect();
        for slot in slots {
            *slot.worker.lock().await = None;
        }
    }

    /// How many workers are alive right now.
    pub fn alive(&self) -> usize {
        self.slots()
            .values()
            .filter(|slot| slot.worker.try_lock().map_or(true, |g| g.is_some()))
            .count()
    }
}

struct NoHost;

impl Host for NoHost {
    fn call<'a>(
        &'a self,
        _target: &'a Target,
        op: &'a str,
        _args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
        Box::pin(async move { Err(format!("unknown operation {op}")) })
    }

    fn log(&self, _target: &Target, _level: &str, _message: &str) {}
}

/// Start a worker, run a trivial backend in it and stop it again: a check that this
/// system can run backends at all (the worker starts, and its jail can be installed).
pub async fn self_test(command: WorkerCommand) -> Result<(), BackendError> {
    let backends = Backends::new(command, Limits::default(), Arc::new(NoHost));
    let target = Target {
        site_id: 0,
        site: "self-test".into(),
        version: "self-test".into(),
        draft: true,
    };
    let files = BTreeMap::from([(
        crate::worker::ENTRY.to_owned(),
        "import { text } from 'sitewright'; export default () => text('ok');".to_owned(),
    )]);
    let response = backends
        .handle(
            &target,
            move || Ok(files),
            BackendRequest {
                method: "GET".into(),
                path: "/api/self-test".into(),
                query: String::new(),
                headers: Vec::new(),
                body: String::new(),
            },
        )
        .await?;
    if response.body == "ok" {
        Ok(())
    } else {
        Err(BackendError::Failed(
            "unexpected answer from the worker".into(),
        ))
    }
}
