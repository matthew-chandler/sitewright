//! Starting and stopping the daemon: database, site store, listeners and background jobs.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use sw_agent::{Agent, ProviderSource};
use sw_core::DataDir;
use sw_core::config::Loaded;
use sw_llm::LlmProvider;
use sw_metrics::{RequestLog, VisitorHasher};
use sw_sites::{DomainChecker, SiteStore};
use sw_web::AppState;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// A running daemon. Dropping it does not stop it; call [`Daemon::shutdown`].
#[derive(Debug)]
pub struct Daemon {
    /// The address the public listener is bound to.
    pub addr: SocketAddr,
    pub data: DataDir,
    pub state: AppState,
    cancel: CancellationToken,
    servers: Vec<JoinHandle<()>>,
    log_writer: JoinHandle<()>,
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

    let db = sw_core::db::open(&data.db_file())
        .await
        .context("cannot open the database")?;
    sw_core::db::migrate(&db, &data.backups_dir())
        .await
        .context("cannot migrate the database")?;

    let config = Arc::new(config);
    let store = SiteStore::open(db.clone(), data.clone(), &config)
        .await
        .context("cannot open the site store")?;
    let hasher = Arc::new(VisitorHasher::load(db.clone()).await);
    let (request_log, log_writer) = RequestLog::spawn(db.clone());
    let domains = DomainChecker::new();
    let providers = Arc::new(ConfiguredProviders {
        config: config.clone(),
        secrets_file: data.secrets_file(),
    });
    let agent = Agent::new(store.clone(), domains.clone(), config.clone(), providers)
        .await
        .context("cannot start the agent")?;
    let state = AppState::new(
        config.clone(),
        store.clone(),
        request_log,
        hasher,
        agent,
        domains,
    );

    let listen = listen.unwrap_or(config.server.listen);
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot listen on {listen}"))?;
    let addr = listener.local_addr()?;
    let socket_path = data.admin_socket();
    let socket = sw_web::socket::bind(&socket_path)
        .await
        .with_context(|| format!("cannot bind admin socket {}", socket_path.display()))?;

    install_health_check(&store, addr);

    let cancel = CancellationToken::new();
    let mut servers = Vec::new();

    let public = sw_web::public_router(state.clone());
    let token = cancel.clone();
    servers.push(tokio::spawn(async move {
        let service = public.into_make_service_with_connect_info::<SocketAddr>();
        if let Err(error) = axum::serve(listener, service)
            .with_graceful_shutdown(token.cancelled_owned())
            .await
        {
            tracing::error!(%error, "public listener failed");
        }
    }));

    let api = sw_web::api::router(state.clone());
    let token = cancel.clone();
    servers.push(tokio::spawn(async move {
        if let Err(error) = sw_web::socket::serve(socket, api, token.cancelled_owned()).await {
            tracing::error!(%error, "admin socket failed");
        }
        let _ = std::fs::remove_file(&socket_path);
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

    tracing::info!(%addr, admin_host = %state.admin_host, sites = state.store.routes().len(), "listening");
    Ok(Daemon {
        addr,
        data,
        state,
        cancel,
        servers,
        log_writer,
    })
}

/// Builds providers from the configuration. Secrets are read on every call, so a key
/// added to `secrets.toml` works without a restart.
struct ConfiguredProviders {
    config: Arc<sw_core::Config>,
    secrets_file: std::path::PathBuf,
}

impl ProviderSource for ConfiguredProviders {
    fn provider(&self, id: Option<&str>) -> Result<Arc<dyn LlmProvider>, String> {
        let llm = &self.config.llm;
        let provider = llm.provider(id).ok_or_else(|| match id {
            Some(id) => format!("there is no LLM provider with id {id:?} in the configuration"),
            None => "no LLM provider is configured; add one under [[llm.providers]] in config.toml"
                .to_owned(),
        })?;
        let secrets = sw_core::Secrets::load(&self.secrets_file).map_err(|e| e.to_string())?;
        sw_llm::build_provider(provider, &secrets).map_err(|e| e.to_string())
    }
}

/// After each publish, fetch the site's front page through the daemon's own listener,
/// exactly as a proxy would. A release that cannot be served is rolled back.
fn install_health_check(store: &SiteStore, addr: SocketAddr) {
    let target = if addr.ip().is_unspecified() {
        SocketAddr::from((Ipv4Addr::LOCALHOST, addr.port()))
    } else {
        addr
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .no_proxy()
        .build();
    let Ok(client) = client else { return };
    store.set_health_check(Arc::new(move |route, hostname| {
        let client = client.clone();
        Box::pin(async move {
            let response = client
                .get(format!("http://{target}/"))
                .header("host", &hostname)
                .header(
                    "user-agent",
                    concat!("sitewright-healthcheck/", env!("CARGO_PKG_VERSION")),
                )
                .send()
                .await
                .map_err(|e| format!("could not fetch the front page of {hostname}: {e}"))?;
            if !response.status().is_success() {
                return Err(format!(
                    "the front page of {hostname} returned {}",
                    response.status()
                ));
            }
            let _ = route;
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
        // The log writer stops once every handle to it is gone.
        drop(state);
        if tokio::time::timeout(Duration::from_secs(5), log_writer)
            .await
            .is_err()
        {
            tracing::warn!("request log did not flush in time");
        }
        db.close().await;
    }

    /// Run until SIGINT or SIGTERM, then shut down cleanly.
    pub async fn run_until_signal(self) -> anyhow::Result<()> {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        tracing::info!("shutting down");
        self.shutdown().await;
        Ok(())
    }
}
