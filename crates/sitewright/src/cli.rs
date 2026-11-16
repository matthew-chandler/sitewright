//! The command line: `serve` runs the daemon; everything else talks to a running daemon
//! over its admin socket, so the CLI never touches the database behind its back.

use std::io::{BufRead, Write};
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};
use sw_core::Config;
use sw_core::config::Loaded;

use crate::client::AdminClient;

#[derive(Debug, Parser)]
#[command(name = "sitewright", version, about, propagate_version = true)]
pub struct Cli {
    #[command(flatten)]
    pub global: Global,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Args)]
pub struct Global {
    /// Configuration file. Default: `config.toml` in the data directory, then /etc/sitewright.
    #[arg(long, global = true, env = "SITEWRIGHT_CONFIG", value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Data directory. Default: the config's `data_dir`, then /var/lib/sitewright.
    #[arg(long, global = true, env = "SITEWRIGHT_DATA_DIR", value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the daemon.
    Serve {
        /// Listen address, overriding `server.listen` in the config.
        #[arg(long, env = "SITEWRIGHT_LISTEN")]
        listen: Option<SocketAddr>,
    },
    /// Write a starter configuration file.
    Init(crate::setup::InitArgs),
    /// Check the installation and report anything that needs attention.
    Doctor,
    /// Serve one packaged site on its own, without the daemon. This is what a
    /// containerized site runs; see docs/containers.md.
    RunSite(crate::runsite::RunSiteArgs),
    /// Put a database downloaded from Sitewright into a packaged site's data directory.
    ImportData(crate::runsite::ImportDataArgs),
    /// Manage sites.
    #[command(subcommand)]
    Site(SiteCommand),
    /// Manage what a site's backend may use: secrets and outside hosts.
    #[command(subcommand)]
    Backend(BackendCommand),
    /// Administer the daemon.
    #[command(subcommand)]
    Admin(AdminCommand),
    /// Download a site's live release as an archive.
    Export {
        site: String,
        /// `folder`: the files, for any static host. `context`: a Docker build context.
        #[arg(long, default_value = "folder")]
        format: String,
        /// Where to write the archive. Default: `<site>.tar.gz` or
        /// `<site>-docker-context.tar.gz` in the current directory.
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// Update to the latest release, after verifying its signature. Keeps the previous
    /// version for `--rollback`.
    Update {
        /// Only report whether a newer release exists.
        #[arg(long)]
        check: bool,
        /// Go back to the version that was running before the last update, including the
        /// database as it was before that update's migrations.
        #[arg(long, conflicts_with = "check")]
        rollback: bool,
    },
    /// Ask the agent to build or change a site.
    Prompt {
        /// Create the site in this run (the name must be free).
        #[arg(long)]
        new: bool,
        /// LLM provider id from the configuration. Default: `llm.default`.
        #[arg(long)]
        provider: Option<String>,
        /// Site name.
        site: String,
        /// What you want, in plain language.
        prompt: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum AdminCommand {
    /// Print a one-time link for enrolling a passkey. Use it for the first sign-in, and
    /// again whenever every passkey is lost.
    Invite,
}

#[derive(Debug, Subcommand)]
pub enum SiteCommand {
    /// List sites.
    List {
        /// Include deleted sites that can still be restored.
        #[arg(long)]
        deleted: bool,
    },
    /// Create an empty site.
    Create {
        name: String,
        /// Human-readable title. Default: the name.
        #[arg(long)]
        title: Option<String>,
    },
    /// Show a site's hostnames and release history.
    Info { name: String },
    /// Publish a directory of static files as a site, creating the site if needed.
    Import {
        name: String,
        /// Directory holding index.html and the rest of the site.
        dir: PathBuf,
        #[arg(long)]
        title: Option<String>,
        /// Release message.
        #[arg(long, short)]
        message: Option<String>,
    },
    /// Switch a site back to an earlier release.
    Rollback {
        name: String,
        /// Commit sha (or prefix) of the release. Default: the one before the current release.
        #[arg(long)]
        to: Option<String>,
    },
    /// Delete a site. It stays recoverable for a few days.
    Delete {
        name: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Restore a deleted site.
    Restore { name: String },
    /// Rename a site. Its address becomes `<new-name>.<zone>`.
    Rename {
        name: String,
        new_name: String,
        /// Drop the old address instead of redirecting it to the new one.
        #[arg(long)]
        no_redirect: bool,
    },
    /// Change a site's title.
    Title { name: String, title: String },
    /// Mark a site as containerized, and print how to get its image.
    Containerize {
        name: String,
        /// Stop containerizing the site.
        #[arg(long)]
        off: bool,
    },
    /// Download a site's container image, its setup guide and (if it has any) its data.
    Image {
        name: String,
        /// Directory to write the files into. Default: the current directory.
        #[arg(long, short, default_value = ".")]
        output: PathBuf,
        /// Leave out the copy of the site's database.
        #[arg(long)]
        no_data: bool,
    },
    /// Attach, detach or promote a site's hostnames.
    #[command(subcommand)]
    Hostname(HostnameCommand),
}

#[derive(Debug, Subcommand)]
pub enum BackendCommand {
    /// List the names of a site's secrets. Values cannot be read back.
    Secrets { site: String },
    /// Store a secret for a site's backend. The value is read from standard input, so
    /// it does not end up in your shell history.
    SetSecret { site: String, name: String },
    /// Remove a secret.
    RemoveSecret { site: String, name: String },
    /// List the outside hosts a site's backend may reach or has asked for.
    Hosts { site: String },
    /// Allow a site's backend to send HTTPS requests to a host.
    AllowHost { site: String, host: String },
    /// Remove a host from a site's list.
    RemoveHost { site: String, host: String },
    /// Show or change whether a site's backend may use AI models, and how much.
    Ai {
        site: String,
        /// Allow the site's backend to use AI models.
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        /// Stop the site's backend from using AI models.
        #[arg(long)]
        disable: bool,
        /// Comma-separated names of the models it may use. Empty: the default model.
        #[arg(long, value_delimiter = ',')]
        models: Option<Vec<String>>,
        /// Most requests per calendar month.
        #[arg(long)]
        monthly: Option<i64>,
        /// Most requests per minute.
        #[arg(long)]
        per_minute: Option<i64>,
    },
}

#[derive(Debug, Subcommand)]
pub enum HostnameCommand {
    /// Serve the site on another hostname too.
    Add {
        site: String,
        hostname: String,
        /// Make it the primary hostname. The previous primary stays attached.
        #[arg(long)]
        primary: bool,
    },
    /// Stop serving the site on a hostname. The primary hostname cannot be removed.
    Remove { site: String, hostname: String },
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    // A packaged site has no configuration file and no daemon to talk to.
    match cli.command {
        Command::RunSite(args) => {
            init_tracing();
            return crate::runsite::run(args).await;
        }
        Command::ImportData(args) => return crate::runsite::import_data(&args),
        _ => {}
    }
    let loaded = Config::load(cli.global.config.as_deref(), cli.global.data_dir.as_deref())?;
    match cli.command {
        Command::Serve { listen } => {
            init_tracing();
            let daemon = crate::daemon::start(loaded, listen).await?;
            daemon.run_until_signal().await
        }
        Command::RunSite(_) | Command::ImportData(_) => {
            unreachable!("handled before the configuration is loaded")
        }
        Command::Init(args) => crate::setup::init(&loaded, args),
        Command::Doctor => crate::setup::doctor(&loaded).await,
        Command::Site(command) => site(&loaded, command).await,
        Command::Backend(command) => backend(&loaded, command).await,
        Command::Admin(AdminCommand::Invite) => {
            let client = AdminClient::new(&loaded.data_dir)?;
            let invite: Value = client.post("/v1/admin/invite", &json!({})).await?;
            println!(
                "Open this link on the device that should hold the passkey:\n\n  {}\n",
                str_of(&invite["url"])
            );
            println!(
                "It works once and expires in {} minutes.",
                invite["valid_minutes"]
            );
            Ok(())
        }
        Command::Update { check, rollback } => crate::update::run(&loaded, check, rollback).await,
        Command::Export {
            site,
            format,
            output,
        } => {
            let client = AdminClient::new(&loaded.data_dir)?;
            let bytes = client
                .get_bytes(&format!("/v1/sites/{site}/export?format={format}"))
                .await?;
            let output = output.unwrap_or_else(|| {
                PathBuf::from(if format == "context" {
                    format!("{site}-docker-context.tar.gz")
                } else {
                    format!("{site}.tar.gz")
                })
            });
            std::fs::write(&output, &bytes)
                .with_context(|| format!("cannot write {}", output.display()))?;
            println!("Wrote {} ({} bytes).", output.display(), bytes.len());
            if format == "context" {
                println!(
                    "Build it with: tar -xzf {} && cd {site} && docker build -t {site} .",
                    output.display()
                );
            }
            Ok(())
        }
        Command::Prompt {
            new,
            provider,
            site,
            prompt,
        } => {
            crate::prompt::run(
                &AdminClient::new(&loaded.data_dir)?,
                &site,
                &prompt,
                new,
                provider.as_deref(),
            )
            .await
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if std::env::var("SITEWRIGHT_LOG_FORMAT").is_ok_and(|v| v == "json") {
        fmt().with_env_filter(filter).json().init();
    } else {
        fmt().with_env_filter(filter).init();
    }
}

async fn site(loaded: &Loaded, command: SiteCommand) -> anyhow::Result<()> {
    let client = AdminClient::new(&loaded.data_dir)?;
    match command {
        SiteCommand::List { deleted } => {
            let sites: Vec<Value> = client.get(&format!("/v1/sites?deleted={deleted}")).await?;
            if sites.is_empty() {
                println!("No sites yet. Create one with `sitewright site import <name> <dir>`.");
            }
            for s in sites {
                let state = match (s["status"].as_str(), s["current_release"].is_null()) {
                    (Some("deleted"), _) => "deleted",
                    (_, true) => "unpublished",
                    _ => "live",
                };
                println!(
                    "{:<24} {:<12} {}",
                    str_of(&s["name"]),
                    state,
                    str_of(&s["url"])
                );
            }
        }
        SiteCommand::Create { name, title } => {
            let info: Value = client
                .post("/v1/sites", &json!({ "name": name, "title": title }))
                .await?;
            println!(
                "Created {name}. It will be served at {} once published.",
                str_of(&info["url"])
            );
        }
        SiteCommand::Info { name } => {
            let info: Value = client.get(&format!("/v1/sites/{name}")).await?;
            print_info(&info);
        }
        SiteCommand::Import {
            name,
            dir,
            title,
            message,
        } => {
            let files = sw_sites::import::read_dir(&dir)
                .with_context(|| format!("cannot read {}", dir.display()))?;
            if files.is_empty() {
                bail!("{} contains no files", dir.display());
            }
            let archive = sw_sites::import::pack(&files)?;
            let mut query = Vec::new();
            if let Some(title) = &title {
                query.push(("title", title.as_str()));
            }
            if let Some(message) = &message {
                query.push(("message", message.as_str()));
            }
            let report: Value = client
                .put_bytes(&format!("/v1/sites/{name}/import"), &query, archive)
                .await?;
            for skipped in report["skipped"].as_array().into_iter().flatten() {
                eprintln!("skipped {}: {}", str_of(&skipped[0]), str_of(&skipped[1]));
            }
            let publish = &report["publish"];
            let sha = short(str_of(&publish["release"]["sha"]));
            if publish["changed"].as_bool() == Some(true) {
                println!(
                    "Published {name} ({} files, release {sha}): {}",
                    report["files"],
                    str_of(&publish["url"])
                );
            } else {
                println!(
                    "No changes: {name} is already live at release {sha}: {}",
                    str_of(&publish["url"])
                );
            }
        }
        SiteCommand::Rollback { name, to } => {
            let release: Value = client
                .post(
                    &format!("/v1/sites/{name}/rollback"),
                    &json!({ "release": to }),
                )
                .await?;
            println!(
                "{name} now serves release {} ({}).",
                short(str_of(&release["sha"])),
                str_of(&release["message"])
            );
        }
        SiteCommand::Delete { name, yes } => {
            if !yes {
                // Check it exists before asking.
                let _: Value = client.get(&format!("/v1/sites/{name}")).await?;
                print!(
                    "This takes {name} offline. It can be restored for a few days, then it is gone for good.\n\
                     Type the site name to confirm: "
                );
                std::io::stdout().flush()?;
                let mut answer = String::new();
                std::io::stdin().lock().read_line(&mut answer)?;
                if answer.trim() != name {
                    bail!("the name did not match; nothing was deleted");
                }
            }
            let _: Value = client
                .post(
                    &format!("/v1/sites/{name}/delete"),
                    &json!({ "confirm": name }),
                )
                .await?;
            println!("Deleted {name}. Undo with `sitewright site restore {name}`.");
        }
        SiteCommand::Hostname(HostnameCommand::Add {
            site,
            hostname,
            primary,
        }) => {
            let info: Value = client
                .post(
                    &format!("/v1/sites/{site}/hostnames"),
                    &json!({ "hostname": hostname, "primary": primary }),
                )
                .await?;
            print_info(&info);
        }
        SiteCommand::Hostname(HostnameCommand::Remove { site, hostname }) => {
            let info: Value = client
                .delete(
                    &format!("/v1/sites/{site}/hostnames"),
                    &json!({ "hostname": hostname }),
                )
                .await?;
            print_info(&info);
        }
        SiteCommand::Containerize { name, off } => {
            let _: Value = client
                .post(
                    &format!("/v1/sites/{name}/containerize"),
                    &json!({ "on": !off }),
                )
                .await?;
            if off {
                println!("{name} is no longer containerized.");
            } else {
                println!(
                    "{name} is containerized. Get its image and setup guide with:\n  sitewright site image {name}"
                );
            }
        }
        SiteCommand::Image {
            name,
            output,
            no_data,
        } => {
            std::fs::create_dir_all(&output)
                .with_context(|| format!("cannot create {}", output.display()))?;
            let mut parts = vec![
                ("image", format!("{name}-image.tar")),
                ("guide", format!("{name}-SETUP.md")),
            ];
            if !no_data {
                parts.push(("data", format!("{name}-data.db")));
            }
            for (part, file_name) in parts {
                let fetched = client
                    .get_bytes(&format!("/v1/sites/{name}/container/{part}"))
                    .await;
                let bytes = match fetched {
                    Ok(bytes) => bytes,
                    // A site without a backend has no data to copy.
                    Err(_) if part == "data" => continue,
                    Err(e) => return Err(e),
                };
                let path = output.join(&file_name);
                std::fs::write(&path, &bytes)
                    .with_context(|| format!("cannot write {}", path.display()))?;
                println!("Wrote {} ({} bytes).", path.display(), bytes.len());
            }
            println!("Next: read {name}-SETUP.md. In short: docker load -i {name}-image.tar");
        }
        SiteCommand::Rename {
            name,
            new_name,
            no_redirect,
        } => {
            let info: Value = client
                .post(
                    &format!("/v1/sites/{name}/rename"),
                    &json!({ "name": new_name, "redirect": !no_redirect }),
                )
                .await?;
            print_info(&info);
        }
        SiteCommand::Title { name, title } => {
            let _: Value = client
                .post(
                    &format!("/v1/sites/{name}/title"),
                    &json!({ "title": title }),
                )
                .await?;
            println!("Renamed {name} to {title:?}.");
        }
        SiteCommand::Restore { name } => {
            let _: Value = client
                .post(&format!("/v1/sites/{name}/restore"), &json!({}))
                .await?;
            println!("Restored {name}.");
        }
    }
    Ok(())
}

fn print_info(info: &Value) {
    let site = &info["site"];
    println!("{} ({})", str_of(&site["name"]), str_of(&site["title"]));
    println!("  url:       {}", str_of(&info["url"]));
    for h in info["hostnames"].as_array().into_iter().flatten() {
        let primary = if h["is_primary"].as_bool() == Some(true) {
            " (primary)"
        } else {
            ""
        };
        println!("  hostname:  {}{primary}", str_of(&h["hostname"]));
    }
    let current = site["current_release"].as_i64();
    let releases = info["releases"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if releases.is_empty() {
        println!("  releases:  none (not published yet)");
    }
    for r in releases {
        let live = if r["id"].as_i64() == current {
            "*"
        } else {
            " "
        };
        println!(
            "  {live} {}  {:<6} {}",
            short(str_of(&r["sha"])),
            str_of(&r["author"]),
            str_of(&r["message"])
        );
    }
}

fn str_of(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

async fn backend(loaded: &Loaded, command: BackendCommand) -> anyhow::Result<()> {
    let client = AdminClient::new(&loaded.data_dir)?;
    let print_secrets = |names: &Value| {
        let names: Vec<&str> = names
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if names.is_empty() {
            println!("No secrets.");
        }
        for name in names {
            println!("{name}");
        }
    };
    let print_hosts = |hosts: &Value| {
        let hosts = hosts.as_array().cloned().unwrap_or_default();
        if hosts.is_empty() {
            println!("No hosts.");
        }
        for host in hosts {
            let status = if host["approved"] == true {
                "approved"
            } else {
                "requested"
            };
            println!("{:<40} {status}", str_of(&host["host"]));
        }
    };
    match command {
        BackendCommand::Secrets { site } => {
            let names: Value = client.get(&format!("/v1/sites/{site}/secrets")).await?;
            print_secrets(&names);
        }
        BackendCommand::SetSecret { site, name } => {
            use std::io::{BufRead, IsTerminal};
            if std::io::stdin().is_terminal() {
                eprint!("Value for {name} (it will be visible as you type): ");
            }
            let mut value = String::new();
            std::io::stdin().lock().read_line(&mut value)?;
            let value = value.trim_end_matches(['\n', '\r']);
            let names: Value = client
                .post(
                    &format!("/v1/sites/{site}/secrets"),
                    &json!({ "name": name, "value": value }),
                )
                .await?;
            print_secrets(&names);
        }
        BackendCommand::RemoveSecret { site, name } => {
            let names: Value = client
                .delete(
                    &format!("/v1/sites/{site}/secrets"),
                    &json!({ "name": name }),
                )
                .await?;
            print_secrets(&names);
        }
        BackendCommand::Hosts { site } => {
            let hosts: Value = client
                .get(&format!("/v1/sites/{site}/backend-hosts"))
                .await?;
            print_hosts(&hosts);
        }
        BackendCommand::AllowHost { site, host } => {
            let hosts: Value = client
                .post(
                    &format!("/v1/sites/{site}/backend-hosts"),
                    &json!({ "host": host }),
                )
                .await?;
            print_hosts(&hosts);
        }
        BackendCommand::Ai {
            site,
            enable,
            disable,
            models,
            monthly,
            per_minute,
        } => {
            let path = format!("/v1/sites/{site}/llm");
            let changes =
                enable || disable || models.is_some() || monthly.is_some() || per_minute.is_some();
            let view: Value = if changes {
                let mut body = serde_json::Map::new();
                if enable || disable {
                    body.insert("enabled".into(), json!(enable));
                }
                if let Some(models) = models {
                    let models: Vec<String> =
                        models.into_iter().filter(|m| !m.is_empty()).collect();
                    body.insert("models".into(), json!(models));
                }
                if let Some(n) = monthly {
                    body.insert("monthly_requests".into(), json!(n));
                }
                if let Some(n) = per_minute {
                    body.insert("per_minute".into(), json!(n));
                }
                client.post(&path, &Value::Object(body)).await?
            } else {
                client.get(&path).await?
            };
            let settings = &view["settings"];
            println!(
                "AI models: {}",
                if settings["enabled"] == true {
                    "allowed"
                } else {
                    "not allowed"
                }
            );
            let names: Vec<&str> = settings["models"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            println!(
                "Models:    {}",
                if names.is_empty() {
                    "the default model".to_owned()
                } else {
                    names.join(", ")
                }
            );
            println!(
                "Limits:    {} per month, {} per minute",
                settings["monthly_requests"], settings["per_minute"]
            );
            println!(
                "Used:      {} requests this month ({} tokens in, {} out)",
                view["usage"]["requests"],
                view["usage"]["input_tokens"],
                view["usage"]["output_tokens"]
            );
        }
        BackendCommand::RemoveHost { site, host } => {
            let hosts: Value = client
                .delete(
                    &format!("/v1/sites/{site}/backend-hosts"),
                    &json!({ "host": host }),
                )
                .await?;
            print_hosts(&hosts);
        }
    }
    Ok(())
}
