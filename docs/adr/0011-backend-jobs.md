# ADR-0011: Background jobs and scheduled tasks for site backends

- Status: accepted
- Date: 2026-11-17
- Builds on: ADR-0006 (site backends)

## Context

Matthew asked that site backends can schedule tasks and run background jobs, and that
these are compatible with what the backend may already do, including calling AI models
("agents") through `llm.chat`.

Backends so far only ran inside a visitor's request: 10 seconds of handler time, one
request at a time per site. A slow model call blocked the site's API for everyone, and
nothing could happen on a timetable.

## Decision

1. **Declared in code, run by the daemon.** A backend declares job handlers with
   `jobs.define(name, fn)` and recurring tasks with `jobs.schedule(name, cron, fn)` at
   the top level of its modules. The worker reports what was declared when the code
   loads (`Ready { tasks }`); declaring later throws. A handler queues a job with
   `jobs.enqueue(name, payload, { delaySeconds, attempts })`, a host call like any other.
   Schedules are therefore versioned, published and rolled back with the code, and the
   owner reads them in the site's history.
2. **Same host calls, same site, same checks.** A job or scheduled run is a new message
   to the worker (`Task`), but everything it does outside the jail goes through the
   existing `Host::call` with the site's `Target`. Database, secrets, approved hosts,
   AI access with its per-minute and monthly limits and usage accounting, and the
   rules for visitor accounts apply unchanged. No new capability and no new owner
   setting was added; turning a capability off for a site turns it off for its jobs.
3. **A separate background worker per site.** The worker slot key gains a
   `background` flag next to `draft`, so tasks run in their own jailed process with the
   same seccomp filter and memory limit, and never queue behind or in front of visitor
   requests. A site's tasks run one at a time.
4. **Queue in Sitewright's database.** `site_jobs` holds queued jobs and the record of
   each scheduled run, with retries (30 s × 4ⁿ back-off) and a week of history. It
   lives outside the site's own database so that site SQL cannot read or forge it.
   Error messages are stored with secret values redacted.
5. **Scheduling.** A daemon loop runs every ten seconds over the live releases with a
   backend. Five-field cron in UTC, parsed by a small built-in parser (no new
   dependency). Next run times are kept in memory; runs missed while the daemon was
   down, or while the site's previous task was still running, are skipped rather than
   caught up, so a restart cannot cause a burst of model calls. Interrupted jobs are
   re-queued at start-up.
6. **Limits.** 30 s of a task's own run time (`backends.job_timeout_secs`), 5 minutes
   in all; 64 KB payloads; 1000 queued jobs per site; 20 schedules and 50 job kinds per
   backend. `backends.jobs = false` turns the feature off server-wide. A publish whose
   schedules do not parse is refused.
7. **Drafts.** The agent's `test_backend` can run a job or schedule once against the
   draft; jobs a draft enqueues are reported to the agent and never queued for real.

## Why

- Running tasks through the same `Host` implementation is what makes them "compatible
  with the backend's permissions" by construction: there is one place that decides what
  a site may do, and tasks cannot bypass it.
- A second worker costs a few megabytes and only exists while a site has background
  work (it is reaped when idle like any worker). Running tasks in the request worker
  would have reintroduced the problem jobs are meant to solve.
- Skipping missed runs is the cautious choice for a feature that can spend money on
  every run.

## Consequences

- A site with schedules keeps its background worker alive as often as the schedule
  fires; with `max_workers = 8` on the dev VPS, frequent schedules on many sites will
  push idle workers out sooner.
- Visitors can still cause cost indirectly (a request that enqueues a job that calls a
  model), but those calls count against the same per-site AI limits as before.
- There is no per-site switch for background work. The owner can roll back, ask the
  agent to remove a schedule, or turn jobs off for the server. A per-site switch can be
  added later if needed.
- Timing is best-effort to about ten seconds; this is not a precise scheduler.

## Alternatives considered

- **A schedule file (`backend/schedules.json`)** read by the daemon without starting a
  worker: simpler to discover, but splits each task from its code and lets the two
  disagree. Discovery by loading the code once per release is cheap.
- **Running tasks in the request worker:** no extra process, but a long task blocks
  every visitor of the site.
- **Queue in the site's database:** moves with the site, but site SQL could read and
  rewrite the queue, and a failed publish's database restore would lose jobs.
- **The `cron` crate:** fine, but a 5-field parser is about 150 lines with tests, and
  avoids a dependency for `cargo deny` to track.
