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
    /// Manage sites.
    #[command(subcommand)]
    Site(SiteCommand),
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
    /// Attach, detach or promote a site's hostnames.
    #[command(subcommand)]
    Hostname(HostnameCommand),
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
    let loaded = Config::load(cli.global.config.as_deref(), cli.global.data_dir.as_deref())?;
    match cli.command {
        Command::Serve { listen } => {
            init_tracing();
            let daemon = crate::daemon::start(loaded, listen).await?;
            daemon.run_until_signal().await
        }
        Command::Init(args) => crate::setup::init(&loaded, args),
        Command::Doctor => crate::setup::doctor(&loaded).await,
        Command::Site(command) => site(&loaded, command).await,
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
