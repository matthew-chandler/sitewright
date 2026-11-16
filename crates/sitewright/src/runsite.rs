//! `sitewright run-site`: serve one packaged site on its own, with no daemon.
//!
//! This is what a containerized site runs. It reads the site's files and its
//! [`SiteManifest`], prepares the site's database (applying the backend's migrations),
//! and then answers requests exactly as the daemon would for that site: static files,
//! and the jailed backend worker for `/api`. There is no admin UI, no agent, no
//! metrics and no TLS; put a reverse proxy in front for HTTPS.
//!
//! Configuration comes from the environment (see `docs/containers.md`):
//! `PORT`, `PUBLIC_HTTPS`, `TRUSTED_PROXIES`, the site's secrets by name, and
//! `LLM_<MODEL>_API_KEY` for each AI model the site uses.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use axum::extract::{ConnectInfo, Request, State};
use axum::response::Response;
use sw_backend::{MANIFEST_FILE, SiteBackends, SiteManifest, WorkerCommand};
use sw_core::config::{BackendsConfig, ServerConfig};
use sw_core::{DataDir, SiteName};
use sw_metrics::VisitorHasher;
use sw_sites::{ReleaseHooks, ReleasePlan};

/// Where things are, and where to listen.
#[derive(Debug, Clone, clap::Args)]
pub struct RunSiteArgs {
    /// The directory holding the site's files (index.html, backend/, ...).
    #[arg(long, default_value = "/site")]
    pub site_dir: PathBuf,
    /// The site's manifest. Default: site.json beside the site directory.
    #[arg(long)]
    pub manifest: Option<PathBuf>,
    /// Where the site keeps its database. Mount a volume here.
    #[arg(long, default_value = "/data")]
    pub data_dir: PathBuf,
    /// Address to listen on. Default: 0.0.0.0 on $PORT, or 8080.
    #[arg(long)]
    pub listen: Option<SocketAddr>,
}

/// The one site of a standalone process.
const SITE_ID: i64 = 1;

#[derive(Clone)]
struct Standalone {
    backends: SiteBackends,
    name: Arc<SiteName>,
    release: Arc<str>,
    root: Arc<Path>,
    has_backend: bool,
    hasher: Arc<VisitorHasher>,
    proxies: Arc<ServerConfig>,
}

fn env_flag(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => default,
    }
}

/// Load the manifest, prepare the data directory and the site's database, and return
/// the router and the address to serve it on.
pub async fn prepare(args: &RunSiteArgs) -> anyhow::Result<(axum::Router, SocketAddr)> {
    let manifest_path = args.manifest.clone().unwrap_or_else(|| {
        args.site_dir
            .parent()
            .unwrap_or(Path::new("/"))
            .join(MANIFEST_FILE)
    });
    let manifest: SiteManifest = serde_json::from_slice(
        &std::fs::read(&manifest_path)
            .with_context(|| format!("cannot read {}", manifest_path.display()))?,
    )
    .with_context(|| format!("{} is not a valid site manifest", manifest_path.display()))?;
    let name = SiteName::parse(&manifest.name).context("the manifest's site name is not valid")?;
    if !args.site_dir.join("index.html").is_file() {
        bail!("{} has no index.html", args.site_dir.display());
    }

    let data = DataDir::new(&args.data_dir);
    data.ensure().with_context(|| {
        format!(
            "cannot use the data directory {} (is the volume writable by this user?)",
            args.data_dir.display()
        )
    })?;
    let db = sw_core::db::open(&data.db_file())
        .await
        .context("cannot open the database")?;
    sw_core::db::migrate(&db, &data.backups_dir())
        .await
        .context("cannot prepare the database")?;
    let now = sw_core::time::now_ms();
    sqlx::query(
        "INSERT INTO sites (id, name, title, status, created_at, updated_at) VALUES (?1, ?2, ?3, 'active', ?4, ?4)
         ON CONFLICT (id) DO UPDATE SET name = excluded.name, title = excluded.title, updated_at = excluded.updated_at",
    )
    .bind(SITE_ID)
    .bind(name.as_str())
    .bind(&manifest.title)
    .bind(now)
    .execute(&db)
    .await
    .context("cannot record the site")?;

    let config = BackendsConfig {
        secure_cookies: env_flag("PUBLIC_HTTPS", true),
        ..BackendsConfig::default()
    };
    let command = WorkerCommand {
        program: std::env::current_exe().context("cannot find this program's own path")?,
        args: vec![sw_backend::WORKER_ARG.to_owned()],
    };
    let backends = SiteBackends::standalone(
        &config,
        data.clone(),
        db.clone(),
        command,
        manifest.secrets.clone(),
    );
    backends
        .apply_manifest(SITE_ID, &manifest)
        .await
        .map_err(|e| anyhow::anyhow!("cannot apply the site's settings: {e}"))?;

    // The same steps as publishing inside Sitewright: the code must load, and the
    // database is migrated (after a copy is taken).
    let plan = ReleasePlan {
        site_id: SITE_ID,
        site: name.clone(),
        sha: manifest.release.clone(),
        root: args.site_dir.clone(),
    };
    backends
        .prepare(&plan)
        .await
        .map_err(|e| anyhow::anyhow!("the site's backend cannot start: {e}"))?;
    backends.commit(&plan).await;

    for secret in &manifest.secrets {
        if std::env::var(secret).map_or(true, |v| v.is_empty()) {
            tracing::warn!(
                secret,
                "the site uses this secret, but the environment variable is not set"
            );
        }
    }
    if let Some(llm) = &manifest.llm {
        for model in &llm.models {
            let var = format!(
                "{}_API_KEY",
                sw_backend::model_env_prefix(&model.provider.id)
            );
            if std::env::var(&var).map_or(true, |v| v.is_empty()) {
                tracing::warn!(model = %model.provider.id, variable = %var,
                    "the site may use this AI model, but its key is not set");
            }
        }
    }

    let trusted_proxies = std::env::var("TRUSTED_PROXIES")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse()
                .with_context(|| format!("TRUSTED_PROXIES: {s:?} is not a network like 10.0.0.0/8"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let state = Standalone {
        backends,
        name: Arc::new(name),
        release: manifest.release.as_str().into(),
        root: args.site_dir.as_path().into(),
        has_backend: manifest.backend,
        hasher: Arc::new(VisitorHasher::load(db).await),
        proxies: Arc::new(ServerConfig {
            trusted_proxies,
            ..ServerConfig::default()
        }),
    };
    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8080);
    let listen = args
        .listen
        .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], port)));
    tracing::info!(site = %manifest.name, release = %manifest.release, backend = manifest.backend, %listen, "serving");
    let router = axum::Router::new()
        .fallback(handle)
        .layer(tower_http::compression::CompressionLayer::new())
        .with_state(state);
    Ok((router, listen))
}

async fn handle(
    State(state): State<Standalone>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
) -> Response {
    let user_agent = request
        .headers()
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let ip = sw_web::client_ip::client_ip(&state.proxies, peer, request.headers());
    let visitor = state.hasher.hash(ip, user_agent, SITE_ID);
    let release = sw_web::site::Release {
        site_id: SITE_ID,
        site: &state.name,
        sha: &state.release,
        root: &state.root,
        backend: state.has_backend,
    };
    sw_web::site::respond(&state.backends, &release, request, &visitor).await
}

/// Serve until the process is told to stop (SIGTERM from a container runtime, Ctrl+C).
pub async fn run(args: RunSiteArgs) -> anyhow::Result<()> {
    let (router, listen) = prepare(&args).await?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot listen on {listen}"))?;
    let stop = async {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async { match term.as_mut() { Some(t) => { t.recv().await; } None => std::future::pending().await } } => {}
        }
    };
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(stop)
    .await
    .context("the server stopped unexpectedly")
}

/// `sitewright import-data`: put a database downloaded from Sitewright into a packaged
/// site's data directory, before its first start.
#[derive(Debug, Clone, clap::Args)]
pub struct ImportDataArgs {
    /// The database file downloaded from Sitewright (`<site>-data.db`).
    pub file: PathBuf,
    /// The site's manifest.
    #[arg(long, default_value = "/site.json")]
    pub manifest: PathBuf,
    /// The site's data directory (the mounted volume).
    #[arg(long, default_value = "/data")]
    pub data_dir: PathBuf,
    /// Replace data that is already there.
    #[arg(long)]
    pub force: bool,
    /// Give the data to this numeric user and group afterwards, when run as root. The
    /// packaged site runs as 65532.
    #[arg(long, default_value_t = 65532)]
    pub owner: u32,
}

pub fn import_data(args: &ImportDataArgs) -> anyhow::Result<()> {
    let manifest: SiteManifest = serde_json::from_slice(
        &std::fs::read(&args.manifest)
            .with_context(|| format!("cannot read {}", args.manifest.display()))?,
    )
    .context("the manifest is not valid")?;
    let name = SiteName::parse(&manifest.name).context("the manifest's site name is not valid")?;
    let header = {
        use std::io::Read;
        let mut buf = [0u8; 16];
        std::fs::File::open(&args.file)
            .and_then(|mut f| f.read_exact(&mut buf))
            .with_context(|| format!("cannot read {}", args.file.display()))?;
        buf
    };
    if &header != b"SQLite format 3\0" {
        bail!(
            "{} is not a database downloaded from Sitewright",
            args.file.display()
        );
    }
    let data = DataDir::new(&args.data_dir);
    let target = data.site_db(&name);
    if target.exists() && !args.force {
        bail!(
            "{} already has data for this site; pass --force to replace it",
            args.data_dir.display()
        );
    }
    let dir = data.site_data(&name);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    for suffix in ["-wal", "-shm"] {
        let mut stale = target.clone().into_os_string();
        stale.push(suffix);
        let _ = std::fs::remove_file(stale);
    }
    std::fs::copy(&args.file, &target)
        .with_context(|| format!("cannot write {}", target.display()))?;
    // Run as root (to write into a fresh volume), hand everything to the site's user.
    let mut pending = vec![args.data_dir.clone()];
    while let Some(path) = pending.pop() {
        let _ = std::os::unix::fs::chown(&path, Some(args.owner), Some(args.owner));
        if path.is_dir()
            && let Ok(entries) = std::fs::read_dir(&path)
        {
            pending.extend(entries.flatten().map(|e| e.path()));
        }
    }
    println!(
        "Imported the data of {} into {}.",
        manifest.name,
        args.data_dir.display()
    );
    Ok(())
}
