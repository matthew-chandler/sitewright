# Roadmap after v1

Status: **approved by Matthew on 2026-11-16**, including the recommendations under
"Decisions" and the change to the hard rule about backends and the network. Progress is
tracked in `docs/STATUS.md`.
`PLAN.md` remains the v1 brief; this file covers what comes next.

## Summary

| Release | Theme | Your request | Size |
|---|---|---|---|
| 1.2 | Portal polish and site rename | 3, 2 | small |
| 1.3 | Named models and GitHub Copilot | 1 (and groundwork for 4) | medium |
| 1.4 | Backends: runtime and site database | 4 | large |
| 1.5 | Backends: secrets and visitor login | 4 | medium |
| 1.6 | Backends: LLM access and dashboards over site data | 4 | medium |
| 1.7 | Backends: other frameworks | 4 | medium, optional |

The order is smallest and safest first. 1.3 comes before the backend work because the
"named model" idea it introduces is what lets you later say "use the new agent" to a site.
Each release is one or more PRs, tagged only when you ask.

Before any of this: the move to `*.machandler.com` still needs the nginx step in
`docs/STATUS.md`.

## Decisions

Matthew approved the roadmap as written, so the recommendation in each item stands.

1. **Default backend runtime (1.4).** I recommend JavaScript handlers run by a small
   engine built into the Sitewright binary (QuickJS), not a separate framework per site.
   The dev VPS has 965 MB of RAM, one CPU and nine other sites; a Node, Deno or Python
   process per site costs 40–80 MB each, and the built-in engine costs a few MB in total.
   The price: no npm packages, and backends can only do what Sitewright exposes to them.
   See [1.4](#14-backends-runtime-and-site-database).
2. **Relaxing a hard rule (1.4).** `AGENTS.md` says the site agent must never gain a
   shell tool or a network tool. Backends keep that literally true, but agent-written
   code will now *run* on the server and can call out to the network. I propose the rule
   becomes: no shell tool, no network tool, and backend code reaches only the hosts you
   approve per site. Approved 2026-11-16; the ADR is written with 1.4.
3. **How to reach Copilot (1.3).** There are two routes and neither is ideal; see
   [1.3](#13-named-models-and-github-copilot). I recommend a one-day spike before
   committing to either.
4. **What "hold the user's API keys and login" means (1.5).** I read it as two things:
   the backend can use third-party API keys that you store in the portal, and a site
   can have its own visitor accounts with sign-in. Tell me if you meant something else.
5. **What "add a new agent" means (1.3, 1.6).** I read "agent" as a named model: a
   provider plus a model plus optional settings, added in the portal under a name such
   as `fast` or `copilot-claude`. Tell me if you meant something more, such as a named
   agent with its own instructions and tools.

## 1.2 Portal polish and site rename

**Run page**

- Show when the run started, and a clock of elapsed time that keeps running until the
  run ends. The clock is computed from the stored start time, so it survives a reload.
- Add a "Back to site" button next to "Cancel". Runs already execute on the server and
  store their events, so leaving the page does not stop them. The site page shows a
  "running" badge that links back to the live run.
- Ctrl+Enter (and Cmd+Enter) submits every prompt box.

**Agent messages**

- The system prompt tells the agent to write Markdown and to end with what it did, not
  with a question or an offer, because there is no reply box.
- Messages are rendered as Markdown on the server (`pulldown-cmark`), with raw HTML in
  the message dropped so a message cannot inject markup into the portal. Text streams
  in plain form and is replaced by the rendered version when the message completes.

**Rename a site and its subdomain**

- A "Rename" form on the site's settings: new name, with the same validation and
  reserved-name checks as site creation.
- Renaming moves the site directory, updates run history (runs are keyed by site name
  today), and makes `<new>.<zone>` the primary hostname.
- The old hostname stays attached and redirects to the new one unless you untick it, so
  existing links keep working. Custom hostnames are untouched.
- Also available as `sitewright site rename <old> <new>`.

**Done when:** the phone checklist gains steps for each item above and they pass.

## 1.3 Named models and GitHub Copilot

**Named models**

- Settings gains a "Models" list. Each entry has a name you choose, a provider, a model
  and optional settings. One is the default for building sites.
- The prompt box gets a model picker, and the agent gains a read-only `list_models`
  tool that returns names and descriptions only, never keys.
- Existing `[llm]` configuration is migrated to one entry called `default`.

**GitHub Copilot as a provider**

Sign-in would be the GitHub device flow: the portal shows a code, you enter it at
`github.com/login/device`, and Sitewright stores the resulting token encrypted, like
other provider secrets. What happens after sign-in depends on the route:

| Route | How it works | For | Against |
|---|---|---|---|
| A. Copilot's chat API directly | Exchange the GitHub token for a short-lived Copilot token, then call an OpenAI-style endpoint. Fits the existing adapter. | Light; no extra software on the VPS. | Not a public API. Community tools that do this use VS Code's client ID and a private token endpoint, and GitHub could block it or object. |
| B. GitHub's Copilot SDK | The official way to use a Copilot subscription from your own app. | Supported by GitHub. | It drives the Copilot CLI as a helper process (Node), which is heavy for this VPS, and it brings its own agent loop, which may not fit Sitewright's tools. |

What I found is from a short web search, not from testing, and the SDK reached general
availability after my training data ends. The spike answers: does route A work with an
OAuth app registered for Sitewright (not VS Code's ID); can route B run as a plain
model call with Sitewright's own tools; and what each costs in memory. I would not ship
route A with a borrowed client ID in a public project without you choosing that
knowingly, since it is your GitHub account at stake.

Either way: the model list comes from your subscription, and the run page shows that
Copilot usage counts against your premium-request allowance, not dollars.

**Done when:** you can sign in with GitHub from your phone, pick a Copilot model, and
build a site with it; signing out removes the stored token.

## Backends (1.4 to 1.7)

### Design in one page

- **A backend is files in the site's repository**, under `backend/`, written by the
  agent with the file tools it already has. It is versioned, published and rolled back
  together with the site.
- **Requests to `/api/*` on a site go to its backend.** Everything else is static, as
  today.
- **The backend can only use what Sitewright hands it:** its own database, its own
  secrets, the LLM gateway, and outbound requests to approved hosts. It has no
  filesystem, no process spawning and no raw sockets.
- **The engine runs in a separate worker process** started by the daemon from the same
  binary, with memory and CPU-time limits per request. Provider keys and the master
  key never enter that process; it asks the daemon over a local socket. A bug in the
  JavaScript engine then cannot read your keys.
- **Limits per site:** request time, memory, database size, requests per minute and LLM
  spend, all with defaults in `config.toml`.

On this VPS I cannot rely on the kernel for isolation: it runs kernel 5.4 with the
older cgroup layout and `devtest` has no root, so per-process memory caps through
systemd and Landlock are not available. That is the main reason for a built-in engine
with its own limits over one OS process per site.

### 1.4 Backends: runtime and site database

- ADR for the runtime choice and the changed hard rule, with a written threat model.
- Spike first: QuickJS (`rquickjs`) in the static musl build, measured on the VPS.
- Handler API, kept close to the web-standard `Request`/`Response` shape so models
  write it well, plus a small router. A new built-in skill teaches the agent the API.
- One SQLite database per site (`site.db`), with migrations in `backend/migrations/`
  applied at publish and rolled back with the release.
- New agent tools: run a backend request against the draft, read backend logs, inspect
  the site database schema. Still no shell tool.
- Publish health check also calls a backend health route. A failing backend blocks the
  publish, as a broken page does today.
- Portal: a "Backend" section per site with status, recent errors and logs.
- Backup, export and import include the site database.

**Done when:** "add a guestbook to hikes" produces a working form that stores entries,
survives a restart, and rolls back cleanly.

### 1.5 Backends: secrets and visitor login

- **Site secrets.** You add them in the portal (name and value). They are stored
  like provider keys, in a file only the daemon's user can read (there is no master
  key to encrypt them with; an earlier draft of this roadmap said otherwise). The agent sees names only, and values are never shown
  again, never written to the repository, and never sent to a model.
- **Outbound requests.** `fetch` works only for hosts on the site's allow-list. When
  the agent needs a new host it raises a request that you approve in the portal, the
  same way `delete_site` asks for confirmation today. Private and loopback addresses
  are always refused.
- **Visitor login.** A built-in module for accounts on a site: email and password
  (Argon2), sessions in secure cookies, sign-up, sign-in, sign-out, and rate limits.
  Built in, so the agent does not hand-write password handling for every site.
  Password reset by email needs a mail provider and is left for later.

**Done when:** a site can call a third-party API with a stored key that appears in no
file, log or model request, and a members-only page works end to end.

### 1.6 Backends: LLM access and dashboards over site data

- **LLM gateway.** Backend code calls `llm.chat({ model: "fast", ... })`. The daemon
  makes the real request with its own credentials, so no backend ever holds a provider
  key. `model` is one of the names from 1.3.
- **Controls per site:** which named models it may use, a monthly spend or request
  cap, and a rate limit. Usage is recorded per site and shown in the portal. A visitor
  spamming a chatbot then stops at the cap and does not run up your bill.
- **"Use the new agent."** The site records which named model it uses in
  `backend/sitewright.toml`. When you add a model in the portal and tell the site
  agent to switch to it, the agent finds it with `list_models` and changes that one
  line.
- **Dashboards.** The dashboard sandbox gains read-only access to the site's own
  database, so the agent can chart backend data (sign-ups, guestbook entries, LLM
  usage) next to traffic. It stays read-only and limited to that one site.

**Done when:** a site with a small chatbot works with no key configured for it, stops
at its cap, and has a dashboard of its usage.

### 1.7 Backends: other frameworks (optional)

For when you ask for something specific, such as FastAPI or Express.

- The site declares an external service: a start command and a port. Sitewright runs
  it as a `systemd --user` unit, proxies `/api/*` to it, starts it on first request
  and stops it when idle to save memory.
- It gets the same secrets and LLM gateway through environment variables and a local
  URL with a per-site token, so the provider keys still never leave the daemon.
- This is ordinary code running as the service user, with far weaker isolation than
  the built-in engine. So it is off by default, enabled per site by you in the portal,
  and the runtime (Node, Python) must already be installed on the server.

I would decide whether to build this after living with 1.4 to 1.6 for a while.

## Not included

- Password reset email, OAuth sign-in for visitors, scheduled jobs and file uploads
  for backends. Each is a natural follow-up; none was asked for.
- A reply box for conversing with the agent mid-run. The 1.2 prompt change removes the
  questions instead.

## Carried over from v1

Offline copy of the release signing key, README screenshots, `.claude/settings.json`,
clean-up of the `.dev` hostnames, and publishing 1.1.0. Details are in
`docs/STATUS.md`.

## Added after approval

- **Containerized sites** (requested 2026-11-16, released as 1.7.0): see
  `docs/containers.md` and ADR-0007. The version number 1.7 was used for this; "other
  backend frameworks" above remains optional and unbuilt, and would be a later release.

**Copilot (1.3), decided 2026-11-16:** Matthew chose the Copilot SDK route,
tentatively. Built as provider kind `copilot` in version 1.8.0; see ADR-0008.
