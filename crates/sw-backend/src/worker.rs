//! The backend worker process: a JavaScript engine in a jail.
//!
//! One worker serves one site. It reads the backend's source from its first message,
//! locks itself down (see [`crate::sandbox`]), loads the code and then answers requests
//! one at a time. It holds no secrets and opens no files; anything a backend needs from
//! the outside world is a `HostCall` message that the daemon answers.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::rc::Rc;
use std::time::{Duration, Instant};

use rquickjs::loader::{Loader, Resolver};
use rquickjs::module::Declared;
use rquickjs::{CatchResultExt, Context, Ctx, Exception, Function, Module, Object, Runtime, Value};

use crate::protocol::{
    Declared as Tasks, FromWorker, RequestMsg, ResponseMsg, TaskMsg, ToWorker, read_frame,
    write_frame,
};
use crate::sandbox;

/// The module backends import their API from.
const API_MODULE: &str = "sitewright";
/// The argument that makes the `sitewright` binary run as a worker.
pub const WORKER_ARG: &str = "__backend-worker";
/// The backend's entry point.
pub const ENTRY: &str = "backend/index.js";
const PRELUDE: &str = include_str!("prelude.js");
const BOOTSTRAP: &str = "\
import app from 'backend/index.js';
import { __dispatch, __runTask, __declared } from 'sitewright';
globalThis.__handle = (raw) => __dispatch(app, raw);
globalThis.__task = (raw) => __runTask(raw);
globalThis.__declared = () => __declared();
";
/// How long loading the backend's modules may take.
const LOAD_TIMEOUT: Duration = Duration::from_secs(5);

/// The worker's two pipes.
struct Channel<R, W> {
    input: R,
    output: W,
}

impl<R: Read, W: Write> Channel<R, W> {
    fn send(&mut self, message: &FromWorker) -> io::Result<()> {
        write_frame(&mut self.output, message)
    }

    fn receive(&mut self) -> io::Result<Option<ToWorker>> {
        read_frame(&mut self.input)
    }

    /// Ask the daemon to do something and wait for its answer.
    fn host_call(
        &mut self,
        op: String,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.send(&FromWorker::HostCall { op, args })
            .map_err(|e| format!("could not reach the server: {e}"))?;
        match self.receive() {
            Ok(Some(ToWorker::HostReply {
                error: Some(error), ..
            })) => Err(error),
            Ok(Some(ToWorker::HostReply { ok, .. })) => Ok(ok.unwrap_or(serde_json::Value::Null)),
            Ok(Some(_)) => Err("unexpected message from the server".into()),
            Ok(None) => Err("the server closed the connection".into()),
            Err(e) => Err(format!("could not read the server's answer: {e}")),
        }
    }
}

/// Imports resolve to the API module or to files under `backend/`.
struct BackendResolver;

impl Resolver for BackendResolver {
    fn resolve(
        &mut self,
        _ctx: &Ctx<'_>,
        base: &str,
        name: &str,
        _attributes: Option<rquickjs::loader::ImportAttributes<'_>>,
    ) -> rquickjs::Result<String> {
        if name == API_MODULE {
            return Ok(name.to_owned());
        }
        let refused = || rquickjs::Error::new_resolving(base, name);
        let joined = if name.starts_with("./") || name.starts_with("../") {
            let dir = base.rsplit_once('/').map_or("", |(dir, _)| dir);
            format!("{dir}/{name}")
        } else if name.starts_with("backend/") {
            name.to_owned()
        } else {
            return Err(refused());
        };
        let mut parts: Vec<&str> = Vec::new();
        for part in joined.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop().ok_or_else(refused)?;
                }
                other => parts.push(other),
            }
        }
        let path = parts.join("/");
        if !path.starts_with("backend/") {
            return Err(refused());
        }
        Ok(path)
    }
}

struct BackendLoader {
    files: BTreeMap<String, String>,
}

impl Loader for BackendLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<rquickjs::loader::ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, Declared>> {
        let source = if name == API_MODULE {
            PRELUDE
        } else {
            self.files
                .get(name)
                .map(String::as_str)
                .ok_or_else(|| rquickjs::Error::new_loading(name))?
        };
        Module::declare(ctx.clone(), name, source)
    }
}

/// Describe the pending JavaScript exception.
fn describe(error: rquickjs::CaughtError<'_>) -> String {
    match error {
        rquickjs::CaughtError::Exception(e) => {
            let message = e.message().unwrap_or_else(|| "error".into());
            match e.stack() {
                Some(stack) if !stack.trim().is_empty() => {
                    format!("{message}\n{}", stack.trim_end())
                }
                _ => message,
            }
        }
        rquickjs::CaughtError::Value(v) => v
            .as_string()
            .and_then(|s| s.to_string().ok())
            .unwrap_or_else(|| "a non-error value was thrown".into()),
        rquickjs::CaughtError::Error(e) => e.to_string(),
    }
}

/// Run the worker on this process's stdin and stdout. Never returns.
pub fn worker_main() -> ! {
    let channel = Channel {
        input: BufReader::new(io::stdin().lock()),
        output: BufWriter::new(io::stdout().lock()),
    };
    let code = match run(channel) {
        Ok(()) => 0,
        Err(_) => 1,
    };
    std::process::exit(code)
}

fn run<R: Read + 'static, W: Write + 'static>(mut channel: Channel<R, W>) -> io::Result<()> {
    let Some(ToWorker::Init {
        files,
        memory_bytes,
        stack_bytes,
    }) = channel.receive()?
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected an init message",
        ));
    };
    let fatal = |channel: &mut Channel<R, W>, message: String| {
        channel.send(&FromWorker::Failed {
            message,
            fatal: true,
        })
    };

    // Lock down before the first byte of site code is parsed.
    if let Err(e) = sandbox::enter(memory_bytes) {
        return fatal(&mut channel, format!("could not start the sandbox: {e}"));
    }

    let (runtime, context) =
        match Runtime::new().and_then(|rt| Ok((rt.clone(), Context::full(&rt)?))) {
            Ok(pair) => pair,
            Err(e) => {
                return fatal(
                    &mut channel,
                    format!("could not start the JavaScript engine: {e}"),
                );
            }
        };
    runtime.set_memory_limit(usize::try_from(memory_bytes).unwrap_or(usize::MAX));
    runtime.set_max_stack_size(usize::try_from(stack_bytes).unwrap_or(256 * 1024));
    let deadline: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
    let timed_out = Rc::new(Cell::new(false));
    {
        let (deadline, timed_out) = (deadline.clone(), timed_out.clone());
        runtime.set_interrupt_handler(Some(Box::new(move || {
            let over = deadline.get().is_some_and(|d| Instant::now() >= d);
            if over {
                timed_out.set(true);
            }
            over
        })));
    }
    runtime.set_loader(BackendResolver, BackendLoader { files });

    let channel = Rc::new(RefCell::new(channel));

    // Load: install the host functions, then import the backend.
    deadline.set(Some(Instant::now() + LOAD_TIMEOUT));
    let loaded: Result<Tasks, String> = context.with(|ctx| {
        let host = Object::new(ctx.clone()).map_err(|e| e.to_string())?;
        let ch = channel.clone();
        let call_deadline = deadline.clone();
        let call = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>, op: String, args: String| -> rquickjs::Result<String> {
                let args = serde_json::from_str(&args)
                    .map_err(|_| Exception::throw_message(&ctx, "arguments are not valid JSON"))?;
                // Time spent waiting for the daemon (a query, an outside request, a
                // model call) is not the handler's own: each of those has its own
                // limit, enforced by the daemon. Push the deadline back by the wait.
                let asked = Instant::now();
                let answer = ch.borrow_mut().host_call(op, args);
                if let Some(d) = call_deadline.get() {
                    call_deadline.set(Some(d + asked.elapsed()));
                }
                match answer {
                    Ok(value) => Ok(value.to_string()),
                    Err(message) => Err(Exception::throw_message(&ctx, &message)),
                }
            },
        )
        .map_err(|e| e.to_string())?;
        let ch = channel.clone();
        let log = Function::new(ctx.clone(), move |level: String, message: String| {
            // Logging must never break a request.
            let _ = ch.borrow_mut().send(&FromWorker::Log { level, message });
        })
        .map_err(|e| e.to_string())?;
        let random = Function::new(
            ctx.clone(),
            |ctx: Ctx<'_>, n: usize| -> rquickjs::Result<String> {
                let mut bytes = vec![0u8; n.min(1024)];
                getrandom::fill(&mut bytes)
                    .map_err(|_| Exception::throw_message(&ctx, "no randomness available"))?;
                Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
            },
        )
        .map_err(|e| e.to_string())?;
        (|| -> rquickjs::Result<()> {
            host.set("call", call)?;
            host.set("log", log)?;
            host.set("random", random)?;
            ctx.globals().set("__host", host)
        })()
        .map_err(|e| e.to_string())?;

        let promise = Module::evaluate(ctx.clone(), "sitewright:bootstrap", BOOTSTRAP)
            .catch(&ctx)
            .map_err(describe)?;
        promise.finish::<Value>().catch(&ctx).map_err(describe)?;
        // Declaring tasks ends with loading; what was declared goes to the daemon.
        let declared: Function<'_> = ctx
            .globals()
            .get("__declared")
            .catch(&ctx)
            .map_err(describe)?;
        let text: String = declared.call(()).catch(&ctx).map_err(describe)?;
        serde_json::from_str::<Tasks>(&text).map_err(|e| e.to_string())
    });
    deadline.set(None);
    let tasks = match loaded {
        Ok(tasks) => tasks,
        Err(message) => {
            let message = if timed_out.get() {
                "loading the backend took too long".to_owned()
            } else {
                message
            };
            return fatal(&mut channel.borrow_mut(), message);
        }
    };
    channel.borrow_mut().send(&FromWorker::Ready { tasks })?;

    loop {
        let work = match channel.borrow_mut().receive()? {
            Some(ToWorker::Request(request)) => Work::Request(request),
            Some(ToWorker::Task(task)) => Work::Task(task),
            Some(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expected a request",
                ));
            }
            None => return Ok(()),
        };
        let timeout_ms = match &work {
            Work::Request(request) => request.timeout_ms,
            Work::Task(task) => task.timeout_ms,
        };
        deadline.set(Some(Instant::now() + Duration::from_millis(timeout_ms)));
        let result = context.with(|ctx| match &work {
            Work::Request(request) => handle(&ctx, request).map(FromWorker::Response),
            Work::Task(task) => run_task(&ctx, task),
        });
        deadline.set(None);
        let reply = match result {
            Ok(reply) => reply,
            // After a timeout or running out of memory the engine's state is not
            // trustworthy; the daemon starts a fresh worker.
            Err(_) if timed_out.get() => FromWorker::Failed {
                message: "the request took too long".into(),
                fatal: true,
            },
            Err(message) => FromWorker::Failed {
                fatal: message.contains("out of memory"),
                message,
            },
        };
        let stop = matches!(reply, FromWorker::Failed { fatal: true, .. });
        channel.borrow_mut().send(&reply)?;
        if stop {
            return Ok(());
        }
        runtime.run_gc();
    }
}

enum Work {
    Request(RequestMsg),
    Task(TaskMsg),
}

/// Run a background task. A handler that throws is an ordinary outcome (`error` set),
/// not a failure of the worker.
fn run_task(ctx: &Ctx<'_>, task: &TaskMsg) -> Result<FromWorker, String> {
    let raw = serde_json::json!({
        "kind": task.kind,
        "name": task.name,
        "payload": task.payload,
        "info": task.info,
    })
    .to_string();
    let runner: Function<'_> = ctx.globals().get("__task").catch(ctx).map_err(describe)?;
    let pending: rquickjs::Promise<'_> = runner.call((raw,)).catch(ctx).map_err(describe)?;
    let out: String = pending.finish().catch(ctx).map_err(describe)?;
    #[derive(serde::Deserialize)]
    struct Outcome {
        error: Option<String>,
    }
    let outcome: Outcome = serde_json::from_str(&out)
        .map_err(|e| format!("the task returned an invalid result: {e}"))?;
    Ok(FromWorker::TaskDone {
        error: outcome.error,
    })
}

fn handle(ctx: &Ctx<'_>, request: &RequestMsg) -> Result<ResponseMsg, String> {
    let raw = serde_json::json!({
        "method": request.method,
        "path": request.path,
        "query": request.query,
        "headers": request.headers,
        "body": request.body,
    })
    .to_string();
    let handler: Function<'_> = ctx.globals().get("__handle").catch(ctx).map_err(describe)?;
    let pending: rquickjs::Promise<'_> = handler.call((raw,)).catch(ctx).map_err(describe)?;
    let out: String = pending.finish().catch(ctx).map_err(describe)?;
    serde_json::from_str(&out).map_err(|e| format!("the backend returned an invalid response: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(base: &str, name: &str) -> Option<String> {
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        ctx.with(|ctx| BackendResolver.resolve(&ctx, base, name, None).ok())
    }

    #[test]
    fn imports_stay_inside_the_backend_directory() {
        assert_eq!(
            resolve("backend/index.js", "sitewright").as_deref(),
            Some("sitewright")
        );
        assert_eq!(
            resolve("backend/index.js", "./lib/util.js").as_deref(),
            Some("backend/lib/util.js")
        );
        assert_eq!(
            resolve("backend/lib/util.js", "../db.js").as_deref(),
            Some("backend/db.js")
        );
        assert_eq!(
            resolve("sitewright:bootstrap", "backend/index.js").as_deref(),
            Some("backend/index.js")
        );
        for outside in [
            "../index.html",
            "../../etc/passwd",
            "fs",
            "/etc/passwd",
            "https://x.test/a.js",
        ] {
            assert_eq!(resolve("backend/index.js", outside), None, "{outside}");
        }
        assert_eq!(resolve("backend/lib/util.js", "../../secrets.js"), None);
    }
}
