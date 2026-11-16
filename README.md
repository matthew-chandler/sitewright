# Sitewright

[![CI](https://github.com/matthew-chandler/sitewright/actions/workflows/ci.yml/badge.svg)](https://github.com/matthew-chandler/sitewright/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**Describe a website from your phone; have it live on its own subdomain a minute later, on
a server you own.**

Sitewright is a single self-hosted binary. It serves your sites, hosts a mobile-first
admin app with passkey sign-in, and runs an AI agent that builds and edits sites (static
pages, plus a small sandboxed backend when a site needs one) through a small set of
guarded tools. It works with any LLM provider (OpenAI-compatible,
Anthropic, Gemini) and any DNS host. No Docker, no database server, no build step.

```text
You, on your phone:   "a hiking log with my favorite Washington trails"
One minute later:     https://hikes.example.com
```

## What it does

- **Prompt to site.** The agent plans the pages, writes HTML and CSS, validates links and
  sizes, and publishes. Follow-up prompts edit the site.
- **Every release is a git commit.** One tap rolls back; history is never lost.
- **Safe by construction.** The agent has no shell and no network access. It can only
  call tools that are confined to one site, and it cannot delete a site: it can only ask
  you to confirm by typing the site's name. Backend code it writes runs with no file,
  network or process access of its own.
- **Backends when a site needs one.** A guestbook, a sign-up sheet, members-only pages,
  a page that asks an AI model: the agent writes server-side JavaScript that runs in a
  seccomp-jailed worker per site, with its own SQLite database, built-in visitor
  accounts, and access to your API keys, approved outside hosts and AI models only as
  far as you allow. A worker uses about 4 MB. See [Backends](docs/backends.md).
- **Any model, any registrar.** Adapters for OpenAI-compatible APIs (OpenAI, Azure OpenAI,
  OpenRouter, Groq, Ollama), Anthropic and Gemini. You add one wildcard DNS record at
  whatever DNS host you use; Sitewright verifies it.
- **Proxy or standalone.** Run behind nginx, Caddy or Cloudflare, or let Sitewright bind
  ports 80 and 443 and get Let's Encrypt certificates itself.
- **An installable app.** The admin UI is a PWA with passkey sign-in (no passwords), live
  agent progress, release history, settings and dashboards.
- **Privacy-friendly metrics.** Request logs without IP addresses, a cookie-free beacon,
  uptime checks, a standard dashboard, and a dashboard the agent designs for each site.
- **Skills.** The agent's know-how is written as [Agent Skills](https://agentskills.io);
  drop your own into a folder to change how it works.
- **Signed self-updates** with rollback, and one-command export of any site as plain files
  or a Docker build context.

## Quickstart

On a Linux server (x86_64 or aarch64) with systemd:

```sh
curl -fsSL https://github.com/matthew-chandler/sitewright/releases/latest/download/install.sh | sudo sh
sudo -u sitewright sitewright init      # your domain, proxy or standalone
sudo systemctl enable --now sitewright
sudo sitewright admin invite            # open the printed link on your phone
```

Add a wildcard DNS record for your domain ([docs/dns-and-tls.md](docs/dns-and-tls.md)),
add a model under **Settings → Models**, then tap **New site**.

Full instructions: [docs/install.md](docs/install.md).

## Honest limits of v1

- **Static sites only.** No server-side code, databases, logins or form handlers on the
  sites themselves.
- **One admin.** No teams or roles.
- **Linux hosts only.**
- **The agent cannot browse the web or use images you upload.** Graphics are SVG and CSS
  it writes itself.
- **Small local models are best effort.** The agent needs reliable tool calling; small
  models often cannot build a whole site.
- **DNS is manual.** Sitewright tells you the record to add and checks it; it does not
  call any DNS provider's API.

## Documentation

| | |
| --- | --- |
| [Install](docs/install.md) | Install paths, first-time setup |
| [Configuration](docs/configuration.md) | Every setting and its default |
| [DNS and TLS](docs/dns-and-tls.md) | Wildcards, proxy and standalone mode, Cloudflare |
| [Providers](docs/providers.md) | Setting up each LLM provider |
| [Skills](docs/skills.md) | Built-in skills and writing your own |
| [Metrics](docs/metrics.md) | What is collected, privacy, dashboards |
| [Backends](docs/backends.md) | Server-side code and a database per site, and how they are sandboxed |
| [Operations](docs/operations.md) | Backup, moving servers, updates, troubleshooting |
| [Security](docs/security.md) | Threat model |
| [Architecture](docs/architecture.md) | How it is put together |
| [Development](docs/development.md), [Testing](docs/testing.md) | Building and testing |
| [Decisions](docs/adr/) | Why things are the way they are |

## Verifying releases

Release archives are signed with [minisign](https://jedisct1.github.io/minisign/).
`sitewright update` verifies the signature and checksum itself before installing. To check
a download by hand:

```sh
minisign -Vm sitewright-x86_64-unknown-linux-musl.tar.gz -P RWTPQ7yYcfU++y7W1z77ZcllL+32prmfxxhMwcPmGzxfGKEfl+faUAYO
```

Public key: `RWTPQ7yYcfU++y7W1z77ZcllL+32prmfxxhMwcPmGzxfGKEfl+faUAYO`

## Building from source

```sh
./scripts/dev-setup.sh                                             # toolchain, in your home directory
cargo run -- serve --config dev/local.toml                        # admin: http://localhost:8080
cargo run -- --config dev/local.toml site import hello example    # site:  http://hello.localhost:8080
cargo nextest run --workspace
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state otherwise, any
contribution intentionally submitted for inclusion in this project by you, as defined in
the Apache-2.0 license, shall be dual licensed as above, without any additional terms or
conditions.
