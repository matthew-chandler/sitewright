# Skills

The agent has two kinds of capability:

- **Tools** are actions Rust code performs, with hard guardrails: write a file, publish,
  roll back. The model cannot do anything a tool does not allow.
- **Skills** are instructions: how to plan a new site, how to make a careful edit, what to
  explain before a deletion. They guide judgement; they grant no extra power.

Skills use the open [Agent Skills](https://agentskills.io) format, so a skill written for
Sitewright is an ordinary folder you could use elsewhere, and vice versa.

## How they are used

The system prompt carries only the **index**: each skill's name and description. When a
request matches a description, the model calls `load_skill(name)` and receives the full
instructions; if the skill bundles files, it can read them with
`read_skill_file(name, path)`. This keeps the prompt small however many skills exist, and
means instructions are loaded only when they are relevant.

## Built-in skills

| Skill | What it covers |
| --- | --- |
| `create-site` | Plan the pages, build a fast, accessible, mobile-first site with no external dependencies, validate, publish. Bundles a starter page, a starter stylesheet, a pre-publish checklist and common page patterns. |
| `edit-site` | Read first, make the smallest change that meets the request, keep the existing style, validate, publish with a clear message; undo with rollback. |
| `delete-site` | Explain what deletion does (offline at once, 7-day recovery, DNS untouched), then raise the confirmation only the user can answer. |
| `setup-domain` | Explain the DNS options, read `check_domain` results, give the exact record to add; wildcard override rules and Cloudflare notes. |

| `design-dashboard` | Look at what the site does, choose metrics that fit it, write and test a SQL query and a Vega-Lite chart per panel, save the dashboard. |

| `export-site` | Pick the right export format (plain files or a Docker build context), build the archive, and explain how to use it. |

Built-in skills live in [`skills/`](../skills) in the repository and are compiled into the
binary. They update with the binary. CI validates each one with the reference validator
(`npx skills-ref validate`).

## Writing your own

Create a folder in `<data>/skills/` named after the skill, containing a `SKILL.md`:

```text
<data>/skills/
  house-style/
    SKILL.md
    references/
      palette.md
```

```markdown
---
name: house-style
description: Our brand colours, fonts and tone of voice. Use when creating or restyling any site.
---

# House style

- Headings are teal (`#0a7d7d`); body text is near-black on off-white.
- Friendly, short sentences. No exclamation marks.
- See `references/palette.md` for the full palette.
```

Rules:

- `name`: 1 to 64 lowercase letters, digits and single hyphens, and **the same as the
  folder name**.
- `description`: up to 1024 characters. Say what the skill does **and when to use it**:
  the model chooses skills from descriptions alone.
- The body is Markdown. Write it as instructions to a capable colleague: steps, rules,
  and the reasons behind them. Keep it under a few hundred lines; put long material in
  `references/` and mention the file so the model knows to read it.
- Bundled files must be text, at most 200 KB each, with ordinary names (letters, digits,
  `-`, `_`, `.`). Symbolic links are refused.

A skill takes effect on the **next run**; no restart is needed. A user skill with the
same name as a built-in **replaces** it, which is how you customise the built-in
behaviour. A skill that fails validation is skipped and reported in the daemon's log.

Check a skill before installing it:

```sh
npx skills-ref validate ./house-style
```

## What a skill cannot do

A skill cannot add tools, widen the file sandbox, skip validation, or let the agent delete
a site without confirmation: those limits are enforced in Rust, not in the prompt.
Treat skills you did not write like any other instructions you give the agent: read them
first.
