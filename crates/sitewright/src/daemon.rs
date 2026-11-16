//! Starting and stopping the daemon: database, site store, listeners and background jobs.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use sw_agent::{Agent, Skills};
use sw_core::DataDir;
use sw_core::config::{Loaded, ServeMode};
use sw_metrics::uptime::{Target, UptimeOptions};
use sw_metrics::{Metrics, RequestLog, VisitorHasher};
use sw_sites::{DomainChecker, SiteStore};
use sw_web::AppState;
use sw_web::auth::Auth;
use sw_web::providers::Providers;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// A running daemon. Dropping it does not stop it; call [`Daemon::shutdown`].
#[derive(Debug)]
pub struct Daemon {
    /// The address the public (plain HTTP) listener is bound to.
    pub addr: SocketAddr,
    /// The address the HTTPS listener is bound to, in standalone mode.
    pub https_addr: Option<SocketAddr>,
    pub data: DataDir,
    pub state: AppState,
    cancel: CancellationToken,
    servers: Vec<JoinHandle<()>>,
    log_writer: JoinHandle<()>,
}

static WORKER_PROGRAM: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Use another program as the backend worker. The daemon normally starts its own
/// executable; tests run inside a test binary, so they point this at the real one.
pub fn set_worker_program(program: std::path::PathBuf) {
    let _ = WORKER_PROGRAM.set(program);
}

/// How to start a backend worker: this program, with the worker argument.
pub(crate) fn worker_command() -> anyhow::Result<sw_backend::WorkerCommand> {
    let program = match WORKER_PROGRAM.get() {
        Some(program) => program.clone(),
        None => std::env::current_exe().context("cannot find this program's own path")?,
    };
    Ok(sw_backend::WorkerCommand {
        program,
        args: vec![sw_backend::WORKER_ARG.to_owned()],
    })
}

/// Start the daemon with the given configuration. `listen` overrides `server.listen`.
pub async fn start(loaded: Loaded, listen: Option<SocketAddr>) -> anyhow::Result<Daemon> {
    let Loaded {
        config,
        data_dir: data,
        source,
    } = loaded;
    data.ensure()
        .with_context(|| format!("cannot create data directory {}", data.root().display()))?;
    match &source {
        Some(path) => {
            tracing::info!(config = %path.display(), data_dir = %data.root().display(), "starting")
        }
        None => {
            tracing::info!(data_dir = %data.root().display(), "starting with built-in defaults (no config file)")
        }
    }

    // A rollback may have asked for the pre-migration database to be put back.
    match sw_update::restore_database_if_requested(&data.backups_dir(), &data.db_file()) {
        Ok(Some(backup)) => {
            tracing::warn!(backup = %backup.display(), "restored the database from before the rolled-back update")
        }
        Ok(None) => {}
        Err(error) => tracing::error!(%error, "could not restore the database after a rollback"),
    }
    let db = sw_core::db::open(&data.db_file())
        .await
        .context("cannot open the database")?;
    let migrated = sw_core::db::migrate(&db, &data.backups_dir())
        .await
        .context("cannot migrate the database")?;
    if let Some(backup) = migrated
        && let Err(error) = sw_update::record_migration(&data.backups_dir(), &backup)
    {
        tracing::warn!(%error, "could not record the pre-migration backup");
    }

    let config = Arc::new(config);
    let store = SiteStore::open(db.clone(), data.clone(), &config)
        .await
        .context("cannot open the site store")?;
    let hasher = Arc::new(VisitorHasher::load(db.clone()).await);
    let (request_log, log_writer) = RequestLog::spawn(db.clone());
    let domains = DomainChecker::new();
    let metrics = Metrics::open(db.clone(), &data.db_file(), config.metrics.slow_ms)
        .await
        .context("cannot open the metrics sandbox")?;
    let providers = Providers::new(config.clone(), db.clone(), data.secrets_file());
    let backend = sw_backend::SiteBackends::new(&config.backends, data.clone(), worker_command()?);
    store.set_release_hooks(Arc::new(backend.clone()));
    let agent = Agent::new(
        store.clone(),
        domains.clone(),
        Skills::new(Some(data.skills_dir())),
        metrics.clone(),
        config.clone(),
        Arc::new(providers.clone()),
    )
    .await
    .context("cannot start the agent")?;
    agent.set_backends(backend.clone());
    let auth = Auth::new(
        db.clone(),
        &config.domains.admin_host(),
        &config.domains.admin_origin(),
    )
    .context("cannot set up authentication")?;
    let state = AppState {
        admin_host: config.domains.admin_host().into(),
        admin_origin: config.domains.admin_origin().into(),
        config: config.clone(),
        store: store.clone(),
        request_log,
        hasher,
        metrics: metrics.clone(),
        agent,
        backend: backend.clone(),
        domains,
        providers,
        auth: Arc::new(auth),
        tls: Arc::default(),
        restart: Arc::default(),
        update_available: Arc::default(),
    };

    let listen = listen.unwrap_or(config.server.listen);
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot listen on {listen}"))?;
    let addr = listener.local_addr()?;
    let socket_path = data.admin_socket();
    let socket = sw_web::socket::bind(&socket_path)
        .await
        .with_context(|| format!("cannot bind admin socket {}", socket_path.display()))?;

    let cancel = CancellationToken::new();
    let mut servers = Vec::new();

    let public = sw_web::public_router(state.clone());
    install_health_check(&store, public.clone());
    let mut https_addr = None;
    let plain = if config.server.mode == ServeMode::Standalone {
        // Standalone: terminate TLS here. The plain listener answers ACME challenges and
        // (unless told otherwise) redirects everything else to HTTPS.
        let challenges = sw_tls::Challenges::default();
        let certs = sw_tls::CertStore::open(&data.certs_dir())
            .context("cannot open the certificate store")?;
        let https = TcpListener::bind(config.tls.https_listen)
            .await
            .with_context(|| format!("cannot listen on {} for HTTPS", config.tls.https_listen))?;
        let bound = https.local_addr()?;
        https_addr = Some(bound);
        let tls_config = sw_tls::serve::server_config(certs.clone());
        servers.push(tokio::spawn(sw_tls::serve::serve_https(
            https,
            tls_config,
            public.clone(),
            cancel.clone(),
        )));
        servers.push(tokio::spawn(run_certificates(
            state.clone(),
            data.clone(),
            certs,
            challenges.clone(),
            cancel.clone(),
        )));
        let redirect_port = config
            .tls
            .redirect_http
            .then(|| config.domains.public_port.unwrap_or(bound.port()));
        tracing::info!(https = %bound, directory = %config.tls.acme_directory, "standalone mode: serving HTTPS");
        sw_tls::serve::http_app(challenges, redirect_port, public)
    } else {
        public
    };
    let token = cancel.clone();
    servers.push(tokio::spawn(async move {
        let service = plain.into_make_service_with_connect_info::<SocketAddr>();
        if let Err(error) = axum::serve(listener, service)
            .with_graceful_shutdown(token.cancelled_owned())
            .await
        {
            tracing::error!(%error, "public listener failed");
        }
    }));

    let api = sw_web::api::socket_router(state.clone());
    let token = cancel.clone();
    servers.push(tokio::spawn(async move {
        if let Err(error) = sw_web::socket::serve(socket, api, token.cancelled_owned()).await {
            tracing::error!(%error, "admin socket failed");
        }
        let _ = std::fs::remove_file(&socket_path);
    }));

    // Stop backend workers that have had no requests for a while.
    let token = cancel.clone();
    let idle_backends = backend.clone();
    servers.push(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = token.cancelled() => return,
                _ = ticker.tick() => {
                    let stopped = idle_backends.reap_idle();
                    if stopped > 0 {
                        tracing::debug!(stopped, "stopped idle backend workers");
                    }
                }
            }
        }
    }));

    let token = cancel.clone();
    let purge_store = store.clone();
    servers.push(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tokio::select! {
                _ = token.cancelled() => return,
                _ = ticker.tick() => match purge_store.purge_expired().await {
                    Ok(purged) if !purged.is_empty() => tracing::info!(?purged, "purged deleted sites"),
                    Ok(_) => {}
                    Err(error) => tracing::error!(%error, "purging deleted sites failed"),
                },
            }
        }
    }));

    // Updates: look for a newer release daily; install it only if configured to.
    if config.updates.check || config.updates.auto {
        servers.push(tokio::spawn(watch_for_updates(
            state.clone(),
            config.updates.auto,
            cancel.clone(),
        )));
    }

    // Metrics housekeeping: keep today's rollups fresh, prune old raw rows hourly.
    let token = cancel.clone();
    let (rollup_db, retention_days) = (db.clone(), config.metrics.retention_days);
    servers.push(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        let mut ticks: u64 = 0;
        loop {
            tokio::select! {
                _ = token.cancelled() => return,
                _ = ticker.tick() => {
                    if let Err(error) = sw_metrics::rollup::rollup_recent(&rollup_db).await {
                        tracing::error!(%error, "refreshing rollups failed");
                    }
                    if ticks.is_multiple_of(60) {
                        match sw_metrics::rollup::prune(&rollup_db, retention_days).await {
                            Ok(0) => {}
                            Ok(rows) => tracing::info!(rows, "pruned old metrics rows"),
                            Err(error) => tracing::error!(%error, "pruning metrics failed"),
                        }
                    }
                    ticks += 1;
                }
            }
        }
    }));

    // Uptime checks: fetch every published site's front page on a timer.
    if config.metrics.uptime_interval_secs > 0 {
        let resolve_to = config.metrics.uptime_resolve_local.then(|| loopback(addr));
        let (routes_store, domains_config) = (store.clone(), config.domains.clone());
        let targets: sw_metrics::uptime::Targets = Arc::new(move || {
            routes_store
                .routes()
                .sites()
                .into_iter()
                .map(|route| Target {
                    site_id: route.site_id,
                    url: match resolve_to {
                        Some(local) => {
                            format!("http://{}:{}/", route.primary_hostname, local.port())
                        }
                        None => format!("{}/", domains_config.public_url(&route.primary_hostname)),
                    },
                    hostname: route.primary_hostname.clone(),
                })
                .collect()
        });
        let options = UptimeOptions {
            interval: Duration::from_secs(config.metrics.uptime_interval_secs),
            slow_ms: config.metrics.slow_ms,
            resolve_to,
        };
        servers.push(sw_metrics::uptime::spawn(
            db.clone(),
            targets,
            options,
            cancel.clone(),
        ));
    }

    tracing::info!(%addr, admin_host = %state.admin_host, sites = state.store.routes().len(), "listening");
    Ok(Daemon {
        addr,
        https_addr,
        data,
        state,
        cancel,
        servers,
        log_writer,
    })
}

/// Keep certificates in step with the served hostnames. Runs until cancelled. The ACME
/// account is opened here, with retries, so a certificate authority that is unreachable
/// at start-up delays certificates but never the daemon.
async fn run_certificates(
    state: AppState,
    data: DataDir,
    certs: sw_tls::CertStore,
    challenges: sw_tls::Challenges,
    cancel: CancellationToken,
) {
    let tls = state.config.tls.clone();
    let account_file = data.acme_dir().join("account.json");
    let acme = loop {
        let opened = sw_tls::Acme::open(
            &account_file,
            &tls.acme_directory,
            tls.acme_contact.as_deref(),
            tls.acme_root_cert.as_ref(),
            challenges.clone(),
        );
        tokio::select! {
            _ = cancel.cancelled() => return,
            result = opened => match result {
                Ok(acme) => break acme,
                Err(error) => {
                    tracing::warn!(%error, "cannot reach the ACME directory yet; retrying in a minute");
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                    }
                }
            },
        }
    };

    // Before asking for a certificate, make sure the name really arrives here over HTTP.
    let precheck: Option<sw_tls::manager::Precheck> = tls.precheck.then(|| {
        let domains = state.domains.clone();
        let check: sw_tls::manager::Precheck = Arc::new(move |hostname: String| {
            let domains = domains.clone();
            Box::pin(async move {
                let result = domains.check(&hostname, 80).await;
                if result.reaches_this_server {
                    Ok(())
                } else {
                    Err(result.detail)
                }
            })
        });
        check
    });
    let options = sw_tls::ManagerOptions {
        scan_interval: Duration::from_secs(tls.scan_interval_secs.max(1)),
        precheck,
        ..sw_tls::ManagerOptions::default()
    };
    let manager = sw_tls::Manager::new(acme, certs, options);
    let _ = state.tls.set(manager.clone());

    // Only names that belong to a site (and, if wanted, the admin host) get certificates.
    let (store, admin_host, include_admin) = (
        state.store.clone(),
        state.admin_host.to_string(),
        tls.include_admin_host,
    );
    let hostnames: sw_tls::manager::Hostnames = Arc::new(move || {
        let (store, admin_host) = (store.clone(), admin_host.clone());
        Box::pin(async move {
            let mut names = store.all_hostnames().await.unwrap_or_default();
            if include_admin && !names.contains(&admin_host) {
                names.push(admin_host);
            }
            names
        })
    });
    let _ = manager.spawn(hostnames, cancel).await;
}

/// Check for a newer release five minutes after start and then once a day. With `auto`,
/// install patch and minor releases and ask for a restart.
async fn watch_for_updates(state: AppState, auto: bool, cancel: CancellationToken) {
    let mut wait = Duration::from_secs(300);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(wait) => {}
        }
        wait = Duration::from_secs(24 * 3600);
        let Ok(current) = semver::Version::parse(sw_update::CURRENT_VERSION) else {
            return;
        };
        let checked = async {
            let updater = sw_update::Updater::official()?;
            let Some(release) = updater.latest().await? else {
                return Ok(None);
            };
            if !release.is_newer_than(&current) {
                return Ok(None);
            }
            if auto && release.auto_installable_from(&current) {
                let binary = updater.download_verified(&release).await?;
                sw_update::install(&binary, &std::env::current_exe()?)?;
                return Ok::<_, sw_update::UpdateError>(Some((release, true)));
            }
            Ok(Some((release, false)))
        }
        .await;
        match checked {
            Ok(Some((release, true))) => {
                tracing::info!(version = %release.version, "installed an update; restarting");
                sw_core::db::audit(
                    state.store.db(),
                    "system",
                    "update.install",
                    Some(&release.version.to_string()),
                    None,
                )
                .await;
                state.restart.notify_one();
                return;
            }
            Ok(Some((release, false))) => {
                tracing::info!(version = %release.version, "a newer release is available");
                *state
                    .update_available
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(release.version.to_string());
            }
            Ok(None) => {
                *state
                    .update_available
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = None
            }
            Err(error) => tracing::warn!(%error, "update check failed"),
        }
    }
}

/// The address to reach our own listener at.
fn loopback(addr: SocketAddr) -> SocketAddr {
    if addr.ip().is_unspecified() {
        SocketAddr::from((Ipv4Addr::LOCALHOST, addr.port()))
    } else {
        addr
    }
}

/// After each publish, request the site's front page from the same application that
/// serves visitors, exactly as a proxy would present it. A release that cannot be served
/// is rolled back. The request goes straight into the router rather than over the
/// network, so it works the same in proxy and standalone mode.
fn install_health_check(store: &SiteStore, app: axum::Router) {
    use tower::ServiceExt;
    store.set_health_check(Arc::new(move |_route, hostname| {
        let app = app.clone();
        Box::pin(async move {
            let mut request = axum::http::Request::builder()
                .uri("/")
                .header("host", &hostname)
                .header(
                    "user-agent",
                    concat!("sitewright-healthcheck/", env!("CARGO_PKG_VERSION")),
                )
                .body(axum::body::Body::empty())
                .map_err(|e| format!("could not build the health check request: {e}"))?;
            let local = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
            request
                .extensions_mut()
                .insert(axum::extract::ConnectInfo(local));
            let response = tokio::time::timeout(Duration::from_secs(5), app.oneshot(request))
                .await
                .map_err(|_| {
                    format!("the front page of {hostname} did not answer within 5 seconds")
                })?
                .unwrap_or_else(|never| match never {});
            if !response.status().is_success() {
                return Err(format!(
                    "the front page of {hostname} returned {}",
                    response.status()
                ));
            }
            Ok(())
        })
    }));
}

impl Daemon {
    /// Stop accepting requests, finish the ones in flight and flush the request log.
    pub async fn shutdown(self) {
        self.cancel.cancel();
        for server in self.servers {
            let _ = server.await;
        }
        let Daemon {
            state, log_writer, ..
        } = self;
        let db = state.store.db().clone();
        // Write out what is queued, then stop the writer. (Waiting for every handle to
        // drop is not an option: the publish health check keeps the application alive.)
        if tokio::time::timeout(Duration::from_secs(5), state.request_log.flush())
            .await
            .is_err()
        {
            tracing::warn!("request log did not flush in time");
        }
        log_writer.abort();
        drop(state);
        db.close().await;
    }

    /// Run until SIGINT or SIGTERM, then shut down cleanly.
    pub async fn run_until_signal(self) -> anyhow::Result<()> {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        let restart = self.state.restart.clone();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
            _ = restart.notified() => tracing::info!("restart requested"),
        }
        tracing::info!("shutting down");
        self.shutdown().await;
        Ok(())
    }
}
