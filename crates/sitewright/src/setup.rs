//! `sitewright init` (write a starter configuration) and `sitewright doctor` (check an
//! installation and say what needs attention).

use std::io::{BufRead, IsTerminal, Write};
use std::os::unix::fs::PermissionsExt;

use anyhow::{Context, bail};
use clap::Args;
use serde_json::Value;
use sw_core::config::Loaded;
use sw_core::names::normalize_hostname;
use sw_core::{Config, Secrets};

use crate::client::AdminClient;

#[derive(Debug, Args)]
pub struct InitArgs {
    /// The DNS zone sites live under, for example `example.com` (sites become
    /// `<name>.example.com`).
    #[arg(long)]
    pub zone: Option<String>,
    /// Hostname of the admin UI. Default: `admin.<zone>`.
    #[arg(long)]
    pub admin_host: Option<String>,
    /// `proxy` (behind nginx, Caddy or Cloudflare) or `standalone` (built-in HTTPS).
    #[arg(long)]
    pub mode: Option<String>,
    /// Email address for the certificate authority (standalone mode).
    #[arg(long)]
    pub email: Option<String>,
    /// Replace an existing configuration file.
    #[arg(long)]
    pub force: bool,
}

fn ask(question: &str, default: &str) -> anyhow::Result<String> {
    if !std::io::stdin().is_terminal() {
        return Ok(default.to_owned());
    }
    if default.is_empty() {
        print!("{question}: ");
    } else {
        print!("{question} [{default}]: ");
    }
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    let answer = answer.trim();
    Ok(if answer.is_empty() {
        default.to_owned()
    } else {
        answer.to_owned()
    })
}

/// Build the configuration text for the given answers.
pub fn starter_config(zone: &str, admin_host: &str, mode: &str, email: Option<&str>) -> String {
    let mut out = String::from(
        "# Sitewright configuration. Every key is documented in docs/configuration.md.\n\n[server]\n",
    );
    if mode == "standalone" {
        out.push_str(
            "# Standalone: Sitewright listens on ports 80 and 443 itself and gets certificates\n\
             # from Let's Encrypt for the admin host and every site.\n\
             mode = \"standalone\"\nlisten = \"0.0.0.0:80\"\n",
        );
    } else {
        out.push_str(
            "# Proxy: a web server in front (nginx, Caddy, Cloudflare) terminates TLS and\n\
             # forwards requests, with the Host header, to this address.\n\
             mode = \"proxy\"\nlisten = \"127.0.0.1:8080\"\n\
             # The proxies whose X-Forwarded-For header is believed.\n\
             trusted_proxies = [\"127.0.0.1/32\", \"::1/128\"]\n",
        );
    }
    out.push_str(&format!(
        "\n[domains]\n# Sites are served at <name>.{zone}\nzones = [\"{zone}\"]\nadmin_host = \"{admin_host}\"\npublic_scheme = \"https\"\n"
    ));
    if mode == "standalone" {
        out.push_str("\n[tls]\nhttps_listen = \"0.0.0.0:443\"\n");
        match email {
            Some(email) => out.push_str(&format!("acme_contact = \"{email}\"\n")),
            None => out.push_str("# acme_contact = \"you@example.com\"   # optional: expiry warnings from Let's Encrypt\n"),
        }
    }
    out.push_str(
        "\n# Add a model in the admin UI (Settings), or here; see docs/providers.md.\n\
         # [[llm.providers]]\n# id = \"openai\"\n# kind = \"openai_compat\"\n# base_url = \"https://api.openai.com/v1\"\n\
         # model = \"MODEL_ID\"\n# key_ref = \"openai\"      # the key goes in secrets.toml under [keys]\n\n\
         [agent]\n# The most the agent may spend per day, in US dollars (needs [[llm.prices]]).\ndaily_spend_cap_usd = 5.0\n",
    );
    out
}

/// `sitewright init`: ask a few questions and write `config.toml` in the data directory.
pub fn init(loaded: &Loaded, args: InitArgs) -> anyhow::Result<()> {
    let data = &loaded.data_dir;
    let path = data.config_file();
    if path.exists() && !args.force {
        bail!(
            "{} already exists; edit it, or pass --force to replace it",
            path.display()
        );
    }

    let zone = match args.zone {
        Some(zone) => zone,
        None => ask(
            "Domain your sites will live under (sites become <name>.<domain>)",
            "",
        )?,
    };
    let zone = normalize_hostname(&zone)
        .filter(|z| z.contains('.'))
        .with_context(|| format!("{zone:?} is not a domain name such as example.com"))?;
    let admin_host = match args.admin_host {
        Some(host) => host,
        None => ask("Hostname for the admin UI", &format!("admin.{zone}"))?,
    };
    let admin_host = normalize_hostname(&admin_host)
        .with_context(|| format!("{admin_host:?} is not a hostname"))?;
    let mode = match args.mode {
        Some(mode) => mode,
        None => ask(
            "Mode: 'proxy' (behind nginx, Caddy or Cloudflare) or 'standalone' (built-in HTTPS)",
            "proxy",
        )?,
    };
    if mode != "proxy" && mode != "standalone" {
        bail!("the mode must be 'proxy' or 'standalone', not {mode:?}");
    }
    let email = match (&args.email, mode.as_str()) {
        (Some(email), _) => Some(email.clone()),
        (None, "standalone") => Some(ask(
            "Email for Let's Encrypt expiry notices (optional)",
            "",
        )?)
        .filter(|e| !e.is_empty()),
        _ => None,
    };

    let text = starter_config(&zone, &admin_host, &mode, email.as_deref());
    Config::from_toml(&text).context("the generated configuration is not valid (this is a bug)")?;
    data.ensure()
        .with_context(|| format!("cannot create {}", data.root().display()))?;
    std::fs::write(&path, &text).with_context(|| format!("cannot write {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))?;

    println!("Wrote {}.\n", path.display());
    println!("Next steps:");
    println!("  1. DNS: point *.{zone} and {admin_host} at this server (docs/dns-and-tls.md).");
    if mode == "proxy" {
        println!("  2. Put your web server in front: examples are in deploy/ (nginx, Caddy).");
    } else {
        println!("  2. Make sure ports 80 and 443 reach this server.");
    }
    println!("  3. Start the daemon:   systemctl enable --now sitewright");
    println!("  4. Enroll a passkey:   sitewright admin invite");
    println!("  5. Check everything:   sitewright doctor");
    Ok(())
}

struct Report {
    problems: usize,
    warnings: usize,
}

impl Report {
    fn ok(&self, text: impl AsRef<str>) {
        println!("  ok    {}", text.as_ref());
    }
    fn warn(&mut self, text: impl AsRef<str>) {
        self.warnings += 1;
        println!("  warn  {}", text.as_ref());
    }
    fn fail(&mut self, text: impl AsRef<str>) {
        self.problems += 1;
        println!("  FAIL  {}", text.as_ref());
    }
}

/// `sitewright doctor`: check files and permissions directly, and everything else by
/// asking the running daemon.
pub async fn doctor(loaded: &Loaded) -> anyhow::Result<()> {
    let mut report = Report {
        problems: 0,
        warnings: 0,
    };
    let data = &loaded.data_dir;
    println!("Sitewright {} doctor\n", env!("CARGO_PKG_VERSION"));

    println!("Files");
    match &loaded.source {
        Some(path) => report.ok(format!("configuration: {}", path.display())),
        None => report
            .warn("no configuration file found; using built-in defaults (run `sitewright init`)"),
    }
    match std::fs::metadata(data.root()) {
        Ok(meta) if meta.permissions().mode() & 0o077 == 0 => report.ok(format!(
            "data directory: {} (owner-only)",
            data.root().display()
        )),
        Ok(meta) => report.fail(format!(
            "data directory {} is accessible to other users (mode {:o}); run: chmod 700 {}",
            data.root().display(),
            meta.permissions().mode() & 0o777,
            data.root().display()
        )),
        Err(_) => report.fail(format!(
            "data directory {} does not exist or is not readable by you",
            data.root().display()
        )),
    }
    match Secrets::load(&data.secrets_file()) {
        Ok(secrets) if secrets.keys.is_empty() => report.ok("secrets.toml: no keys stored"),
        Ok(secrets) => report.ok(format!(
            "secrets.toml: {} key(s), owner-only",
            secrets.keys.len()
        )),
        Err(error) => report.fail(error.to_string()),
    }

    println!("\nDaemon");
    let client = AdminClient::new(data)?;
    let status: Value = match client.get("/v1/admin/status").await {
        Ok(status) => status,
        Err(error) => {
            report.fail(error.to_string());
            return finish(report);
        }
    };
    let text = |v: &Value| v.as_str().unwrap_or("").to_owned();
    report.ok(format!(
        "running, version {}, {} mode",
        text(&status["version"]),
        text(&status["mode"])
    ));
    if status["version"].as_str() != Some(env!("CARGO_PKG_VERSION")) {
        report.warn(format!(
            "the daemon runs {} but this binary is {}; restart the daemon",
            text(&status["version"]),
            env!("CARGO_PKG_VERSION")
        ));
    }
    if let Some(version) = status["update_available"].as_str() {
        report.warn(format!(
            "version {version} is available; run `sitewright update`"
        ));
    }

    println!("\nSign-in");
    report.ok(format!("admin UI: {}", text(&status["admin_origin"])));
    if !text(&status["admin_origin"]).starts_with("https://")
        && !text(&status["admin_origin"]).contains("localhost")
    {
        report.fail("the admin UI is not served over HTTPS, so passkeys will not work; set domains.public_scheme or domains.admin_url");
    }
    match status["passkeys"].as_u64() {
        Some(0) | None => report.warn("no passkey is enrolled; run `sitewright admin invite` and open the link on your device"),
        Some(n) => report.ok(format!("{n} passkey(s) enrolled")),
    }

    println!("\nModels");
    let providers = status["providers"].as_array().cloned().unwrap_or_default();
    if providers.is_empty() {
        report.warn("no LLM provider is configured; add one in Settings or in config.toml (docs/providers.md)");
    }
    for provider in &providers {
        let label = format!(
            "{} ({}, {})",
            text(&provider["id"]),
            text(&provider["kind"]),
            text(&provider["model"])
        );
        if provider["needs_key"].as_bool() == Some(true)
            && provider["has_key"].as_bool() != Some(true)
        {
            report.fail(format!("{label}: its API key is missing from secrets.toml"));
        } else {
            report.ok(label);
        }
    }
    report.ok(format!(
        "spent today: ${:.2} of ${:.2}",
        status["spent_today_usd"].as_f64().unwrap_or(0.0),
        status["daily_cap_usd"].as_f64().unwrap_or(0.0)
    ));

    println!("\nBackends");
    if !loaded.config.backends.enabled {
        report.ok("turned off ([backends] enabled = false)");
    } else {
        match crate::daemon::worker_command() {
            Ok(command) => match sw_backend::self_test(command).await {
                Ok(()) => report.ok("a sandboxed worker starts and answers"),
                Err(error) => report.fail(format!(
                    "site backends cannot run here: {error}. They need Linux with seccomp; \
                     set [backends] enabled = false to silence this"
                )),
            },
            Err(error) => report.fail(format!("site backends cannot run here: {error:#}")),
        }
    }

    println!("\nSites");
    let sites = status["sites"].as_array().cloned().unwrap_or_default();
    if sites.is_empty() {
        report.ok("no sites yet");
    }
    for site in &sites {
        let name = text(&site["name"]);
        match (site["published"].as_bool(), site["status"].as_str()) {
            (Some(false), _) => report.warn(format!("{name}: not published")),
            (_, Some("down")) => {
                report.fail(format!("{name}: down (two uptime checks in a row failed)"))
            }
            (_, Some("degraded")) => report.warn(format!(
                "{name}: degraded (slow, a failed check, or a certificate close to expiry)"
            )),
            (_, Some("up")) => report.ok(format!("{name}: up")),
            _ => report.ok(format!("{name}: published, not checked yet")),
        }
    }

    if let Some(certificates) = status["certificates"].as_array() {
        println!("\nCertificates");
        if certificates.is_empty() {
            report.ok("no hostnames need certificates yet");
        }
        for host in certificates {
            let hostname = text(&host["hostname"]);
            match (host["certificate"].as_object(), host["last_error"].as_str()) {
                (Some(_), None) => report.ok(hostname),
                (Some(_), Some(error)) => report.warn(format!(
                    "{hostname}: has a certificate, but renewal failed: {error}"
                )),
                (None, Some(error)) => report.fail(format!("{hostname}: {error}")),
                (None, None) => {
                    report.warn(format!("{hostname}: waiting for its first certificate"))
                }
            }
        }
    } else if status["mode"] == "standalone" {
        println!("\nCertificates");
        report
            .warn("the certificate manager has not started yet (is the ACME directory reachable?)");
    }
    finish(report)
}

fn finish(report: Report) -> anyhow::Result<()> {
    println!();
    match (report.problems, report.warnings) {
        (0, 0) => println!("Everything looks good."),
        (0, w) => println!("No problems, {w} thing(s) to look at."),
        (p, w) => {
            println!("{p} problem(s), {w} warning(s).");
            bail!("doctor found problems");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_configs_are_valid_for_both_modes() {
        let proxy = starter_config("example.com", "admin.example.com", "proxy", None);
        let config = Config::from_toml(&proxy).unwrap();
        assert_eq!(config.domains.admin_origin(), "https://admin.example.com");
        assert_eq!(config.server.listen.to_string(), "127.0.0.1:8080");
        assert!(config.server.is_trusted_proxy("127.0.0.1".parse().unwrap()));

        let standalone = starter_config(
            "example.com",
            "panel.example.com",
            "standalone",
            Some("me@example.com"),
        );
        let config = Config::from_toml(&standalone).unwrap();
        assert_eq!(config.server.mode, sw_core::config::ServeMode::Standalone);
        assert_eq!(config.tls.acme_contact.as_deref(), Some("me@example.com"));
        assert_eq!(
            config.tls.acme_directory,
            sw_core::config::LETS_ENCRYPT_PRODUCTION
        );
        assert_eq!(config.domains.admin_host(), "panel.example.com");
    }
}
