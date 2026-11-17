# ADR-0008: GitHub Copilot through its SDK

- Status: accepted, experimental (Matthew chose the SDK route "tentatively", 2026-11-16)
- Not yet run against the real Copilot runtime. See "What is not verified".

## Context

Matthew wants to build sites with the models his GitHub Copilot subscription includes.

- GitHub Models, the hosted chat API, was retired on 2026-07-30.
- Copilot's own chat API is supported only for named partner tools. Other tools reach
  it by borrowing VS Code's client id; a public project should not.
- The supported route is the Copilot SDK: a library that drives GitHub's Copilot
  runtime, a 209 MB program with its own agent loop and its own tools (shell, files,
  web). There is a Rust crate, `github-copilot-sdk`.

Sitewright's rules still apply: the site agent has no shell tool and no network tool,
the binary never links OpenSSL, and the dev VPS has under 600 MB of free memory.

## Decision

**Copilot is one more provider kind (`copilot`), not a second agent engine.** The agent
loop, limits, events, tools and UI are unchanged.

The runtime wants to run tools itself and loop. The adapter (`sw-llm/src/copilot.rs`)
turns that inside out:

1. The first model call of a run starts the runtime and opens one session with
   Sitewright's system prompt (mode `replace`) and Sitewright's tools.
2. When the model calls a tool, the runtime asks the SDK to run it. The adapter does
   not answer. It emits the call, ends the response with "tool use", and parks the
   session with the runtime waiting.
3. The agent runs the tool as it does for every provider and makes the next model
   call. That request ends with tool results; their call ids find the parked session
   and the results are handed to the runtime, which continues.

If a session is lost (the runtime died), the next call opens a new one and replays the
conversation as text.

**The runtime gets none of its own powers.**

- The session's tool allowlist contains only Sitewright's tool names (or a placeholder
  when a request has no tools, so the list is never empty).
- The permission handler approves only requests of kind "custom tool" and rejects
  shell, read, write, URL, MCP, memory and hook requests.
- Skills and session telemetry are off.
- The process starts with an empty environment (only `HOME`, a minimal `PATH` and the
  token), its own home directory under `<data_dir>/copilot/home`, and an empty working
  directory.

This keeps the hard rule in `AGENTS.md` as written, but by configuration of a program
we do not control rather than by construction. That is weaker than the backend jail
(ADR-0006) and is the main reason this is marked experimental.

**Sitewright starts the runtime itself.** The SDK is built with
`default-features = false`, which removes its launcher and downloader. Those need the
system's OpenSSL, which this project bans. The adapter spawns the process and gives the
SDK its pipes.

**The runtime is installed separately.** `sitewright copilot install` downloads the
exact release the SDK version was built for from `github/copilot-cli`, checks its
SHA-256 against a value pinned in the source, and unpacks only the program into
`<data_dir>/copilot/`. It is never bundled in a release and never downloaded by the
daemon on its own.

**One runtime at a time, only while needed.** It starts on the first call and is
killed when the last conversation using it ends. A second conversation waits for the
slot (two minutes, then an error).

**The token is a provider key.** A fine-grained personal access token with the
"Copilot Requests" permission, stored like any other provider key and passed to the
runtime in an environment variable, never on the command line.

## Consequences

- The SDK turns on `serde_json`'s `preserve_order` for the whole build, so JSON objects
  keep the order they were built in instead of being sorted. Two tests that looked at
  the start of a long JSON string were adjusted. Nothing else depended on the order.
- Cost shows as $0: Copilot is billed by GitHub as premium requests, not per token.
  Token counts are still recorded, and `agent.max_run_tokens` still applies.
- A Copilot provider can be put on a site's AI model list for backends, but each call
  then starts the runtime. Not recommended on a small server.
- Containerized sites cannot use a Copilot model: the image has no runtime.
- The binary grows by the SDK and its dependencies.

## What is not verified

I was not permitted to execute the runtime, so everything about the real program is
taken from the SDK's source and tests, not observed:

- that `--server --stdio --auth-token-env` starts it as the SDK's own launcher does;
- that the tool allowlist and `replace` system prompt behave as documented;
- its memory use;
- any real model call.

`sitewright copilot check` exercises all four and prints the memory numbers. Until it
has passed on the VPS, treat this as untested.

## Alternatives considered

- **A second engine that lets the runtime own the loop.** More code, a second set of
  limits and events, and the runtime's loop would decide when a run ends.
- **Calling Copilot's chat API directly.** Not supported for this project.
- **The SDK's own launcher.** Needs OpenSSL.
