# ADR-0006: Site backends run as JavaScript in a jailed worker process

- Status: accepted
- Date: 2026-11-16

## Context

Sites so far are static files. The roadmap adds backends: code the agent writes that
answers a site's `/api/*` requests, stores data, and later holds secrets and calls LLMs.
That means running agent-written code on the server, which v1 deliberately never did.

The first server this runs on has 965 MB of RAM, one CPU, nine unrelated production
sites, an unprivileged service user, kernel 5.4 (no Landlock) and the older cgroup
layout (no per-service memory limits for user units).

## Decision

1. **One built-in runtime.** A backend is JavaScript (ES modules) under `backend/` in the
   site's repository, entry point `backend/index.js`, run by QuickJS (`rquickjs`)
   compiled into the Sitewright binary. There is no per-site framework, no package
   manager and no `node_modules`.
2. **A worker process per site, started on demand.** The daemon starts the same binary
   with a fixed argument. A worker serves one request at a time, keeps its JavaScript
   state between requests, and is stopped when idle, when the code changes, or when it
   exceeds its time or memory limit.
3. **The worker jails itself before it parses any site code.** It sets resource limits,
   then installs a seccomp filter that allows only reading and writing its two pipes,
   memory management, the clock, randomness and exiting. Every other system call fails:
   no files, no sockets, no processes, no signals.
4. **Everything else is a message to the daemon** ("host call"): SQL on the site's own
   database now; secrets, outbound requests to approved hosts, and LLM access later.
   The daemon knows which site a worker belongs to and confines each call to it. The
   worker never holds provider keys, the master key, or another site's data.
5. **The hard rule changes.** It was "the agent must never gain a shell tool or a
   network tool". It is now: the agent has no shell tool and no network tool; code it
   writes runs only inside the jail; and that code reaches the network only through the
   daemon, only to hosts the owner approved for that site. Matthew approved this on
   2026-11-16.

## Why

- **Memory.** A Node, Deno or Python process costs 40 to 80 MB per site before it does
  anything. A worker here is a 1.7 MB static program with a small heap. On this server
  that is the difference between a handful of backends and none.
- **Isolation that does not need root.** With no Landlock and no cgroup delegation, the
  usual ways to confine a process as an unprivileged user are unavailable. seccomp is
  available, and a runtime with no I/O of its own makes a tight allowlist practical: the
  engine needs about thirty system calls. A general-purpose runtime needs hundreds, and
  its permission flags would be the only barrier.
- **A separate process, not a thread.** QuickJS is C. If agent-written code finds a
  memory-safety bug in it, the damage is limited to a process that has no secrets and no
  system calls worth having. In-process, the same bug would expose every key.
- **Capabilities are explicit and reviewable.** Each thing a backend can do is one
  operation in one `match`, checked against the site. Adding a capability is a visible
  code change, not a default of the runtime.
- **One runtime for the agent to learn.** The API follows web-standard shapes
  (`Request`, `Response`, `Headers`, a small router), and a built-in skill documents it.

## Consequences

- No npm ecosystem. Backends are limited to what the `sitewright` module offers. This is
  the main cost, accepted for the reasons above; ADR-worthy exceptions go through the
  "other frameworks" path on the roadmap, which is off by default and far less isolated.
- QuickJS is an interpreter: fine for form handlers and small APIs, wrong for heavy
  computation. Handlers have a time limit (10 s by default).
- Bodies are UTF-8 text. Binary uploads and downloads are not supported yet.
- One request at a time per site. Requests queue briefly, then get a 503.
- Site SQL is written at run time, so it cannot use the compile-time-checked `sqlx`
  macros that the rest of the code base requires. It runs on a separate connection with
  its own guards instead.
- The jail is Linux-only (x86_64 and aarch64), like the releases.
- `seccomp` filters apply to the thread that installs them, so the worker is recognised
  in `main` before the async runtime starts any thread.

## Alternatives considered

- **Deno or Node per site**: familiar and full-featured, but too heavy here and reliant
  on runtime-level permission flags.
- **QuickJS inside the daemon**: lightest, but one engine bug away from the keys.
- **WebAssembly**: strong isolation, but the agent has no compiler and no shell to run
  one.
- **A declarative backend only** (collections and rules, no code): safest, but cannot
  express the "call an LLM, then store the result" handlers the roadmap asks for.
