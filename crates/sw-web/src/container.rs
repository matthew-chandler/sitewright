//! Packaging a site as a container image that runs without Sitewright.
//!
//! The image holds three things: this very program (a static binary, used in its
//! `run-site` mode), the files of the site's live release, and a manifest of the site's
//! settings. No Docker is needed to build it; `sw-image` writes the archive directly.
//!
//! Nothing secret goes into the image. Every build comes with a setup guide that lists
//! the environment variables the site needs where it will run.

use std::fmt::Write as _;
use std::path::PathBuf;

use sw_backend::{ManifestLlm, ManifestModel, SiteManifest, model_env_prefix};
use sw_image::{Entry, ImageSpec, Layer, Source};
use sw_sites::confine;

use crate::state::AppState;

/// The user the site runs as inside the container: unprivileged, with no name.
const RUN_AS: u32 = 65532;

/// A built image, ready to download.
#[derive(Debug, Clone)]
pub struct BuiltImage {
    pub path: PathBuf,
    pub file_name: String,
    /// The tag the image loads as, for example `sitewright-site/hikes:1a2b3c4d`.
    pub tag: String,
    pub bytes: u64,
}

/// Everything about a site's package except the archive itself.
#[derive(Debug, Clone)]
pub struct Package {
    pub manifest: SiteManifest,
    /// The directory of the release being packaged.
    pub root: PathBuf,
    pub tag: String,
    pub file_name: String,
}

/// Describe the package of a site's live release: its manifest and names.
pub async fn describe(state: &AppState, name: &str) -> Result<Package, String> {
    let site = state.store.get(name).await.map_err(|e| e.to_string())?;
    let route = state
        .store
        .routes()
        .sites()
        .into_iter()
        .find(|route| route.site_id == site.id)
        .ok_or_else(|| format!("{name} has not been published yet, so there is nothing to package"))?;

    let llm_settings = state.backend.llm().settings(site.id).await?;
    let llm = if llm_settings.enabled && route.backend {
        let allowed = state.backend.llm().allowed(site.id).await?;
        let providers = state.providers.list().await?;
        let models: Vec<ManifestModel> = allowed
            .iter()
            .filter_map(|model| {
                let view = providers.iter().find(|p| p.config.id == model.name)?;
                let mut provider = view.config.clone();
                // The key stays here. Where the site runs, it comes from the environment.
                provider.key_ref = None;
                let price = state
                    .config
                    .llm
                    .price(&provider.model)
                    .map(|p| (p.input_per_mtok, p.output_per_mtok));
                Some(ManifestModel { provider, price })
            })
            .collect();
        (!models.is_empty()).then_some(ManifestLlm {
            monthly_requests: llm_settings.monthly_requests,
            per_minute: llm_settings.per_minute,
            models,
        })
    } else {
        None
    };
    let hosts = state
        .backend
        .hosts(site.id)
        .await?
        .into_iter()
        .filter(|h| h.approved)
        .map(|h| h.host)
        .collect();
    let short: String = route.sha.chars().take(8).collect();
    Ok(Package {
        manifest: SiteManifest {
            name: site.name.as_str().to_owned(),
            title: site.title.clone(),
            release: route.sha.clone(),
            sitewright_version: env!("CARGO_PKG_VERSION").to_owned(),
            backend: route.backend,
            secrets: if route.backend {
                state.backend.secret_names(site.id)?
            } else {
                Vec::new()
            },
            hosts,
            llm,
        },
        root: route.root.clone(),
        tag: format!("sitewright-site/{}:{short}", site.name),
        file_name: format!("{}-image.tar", site.name),
    })
}

/// Build (or reuse) the image archive of a site's live release.
pub async fn build(state: &AppState, name: &str) -> Result<BuiltImage, String> {
    let package = describe(state, name).await?;
    let architecture = sw_image::host_architecture()
        .ok_or("images can be built on x86_64 and aarch64 servers only")?;
    let exports = state.data.exports_dir();
    let out = exports.join(&package.file_name);
    let program = (*state.program).clone();
    let guide = guide(&package);
    let manifest_json = serde_json::to_vec_pretty(&package.manifest).map_err(|e| e.to_string())?;
    let (tag, file_name) = (package.tag.clone(), package.file_name.clone());
    let root = package.root.clone();

    let bytes = tokio::task::spawn_blocking(move || -> Result<u64, String> {
        std::fs::create_dir_all(&exports).map_err(|e| e.to_string())?;
        let root_file = |path: &str, bytes: Vec<u8>| Entry::File {
            path: path.to_owned(),
            source: Source::Bytes(bytes),
            mode: 0o644,
            uid: 0,
            gid: 0,
        };
        // Layer 1: the program. The same for every site packaged by this version, so
        // registries and hosts store it once.
        let runtime = Layer {
            entries: vec![Entry::File {
                path: "sitewright".into(),
                source: Source::Path(program),
                mode: 0o755,
                uid: 0,
                gid: 0,
            }],
        };
        // Layer 2: the site, read-only, and a data directory the site's user can write.
        let (files, _) = confine::walk(&root).map_err(|e| e.to_string())?;
        let mut entries: Vec<Entry> = files
            .iter()
            .map(|file| Entry::File {
                path: format!("site/{}", file.rel),
                source: Source::Path(root.join(&file.rel)),
                mode: 0o644,
                uid: 0,
                gid: 0,
            })
            .collect();
        entries.push(root_file("site.json", manifest_json));
        entries.push(root_file("SETUP.md", guide.into_bytes()));
        entries.push(Entry::Dir {
            path: "data".into(),
            mode: 0o700,
            uid: RUN_AS,
            gid: RUN_AS,
        });
        let created = std::fs::metadata(&root)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64);
        let spec = ImageSpec {
            repo_tag: tag,
            architecture: architecture.to_owned(),
            layers: vec![runtime, Layer { entries }],
            entrypoint: vec!["/sitewright".into(), "run-site".into()],
            cmd: Vec::new(),
            env: vec![("PORT".into(), "8080".into())],
            exposed_ports: vec![8080],
            volumes: vec!["/data".into()],
            user: format!("{RUN_AS}:{RUN_AS}"),
            working_dir: "/".into(),
            labels: vec![
                ("org.opencontainers.image.title".into(), package.manifest.title.clone()),
                ("org.opencontainers.image.revision".into(), package.manifest.release.clone()),
                (
                    "org.opencontainers.image.description".into(),
                    format!("The site {} packaged by Sitewright; see /SETUP.md", package.manifest.name),
                ),
                ("dev.sitewright.version".into(), package.manifest.sitewright_version.clone()),
            ],
            created_unix_secs: created,
        };
        sw_image::write_docker_archive(&spec, &out)
            .map(|info| info.bytes)
            .map_err(|e| format!("could not build the image: {e}"))
    })
    .await
    .map_err(|e| format!("background task failed: {e}"))??;

    Ok(BuiltImage {
        path: state.data.exports_dir().join(&file_name),
        file_name,
        tag: package.tag,
        bytes,
    })
}

/// A copy of the site's database as it is now, for moving the data too. `None` when the
/// site has no database.
pub async fn data_snapshot(state: &AppState, name: &str) -> Result<Option<PathBuf>, String> {
    let site = state.store.get(name).await.map_err(|e| e.to_string())?;
    state
        .backend
        .snapshot(&site.name, &state.data.exports_dir())
        .await
}

/// The setup guide for one package, in Markdown. It names every environment variable
/// the site needs and never contains a value.
pub fn guide(package: &Package) -> String {
    let m = &package.manifest;
    let mut g = String::new();
    let _ = writeln!(g, "# Running {} in a container\n", m.title);
    let _ = writeln!(
        g,
        "This image holds the site **{}** (release `{}`), packaged by Sitewright {}. It runs \
         on its own: no Sitewright server is needed. It contains no passwords or API keys; \
         you supply those where it runs, as described below.\n",
        m.name,
        m.release.chars().take(8).collect::<String>(),
        m.sitewright_version
    );

    let _ = writeln!(g, "## 1. Load the image\n");
    let _ = writeln!(g, "```sh\ndocker load -i {}\n```\n", package.file_name);
    let _ = writeln!(
        g,
        "It loads as `{}`. Podman works the same way (`podman load -i ...`). The image is \
         built for the processor type of the server that packaged it (`docker image inspect` \
         shows which); run it on the same type.\n",
        package.tag
    );

    let _ = writeln!(g, "## 2. Set the environment\n");
    let mut rows: Vec<(String, String, &str)> = Vec::new();
    for secret in &m.secrets {
        rows.push((
            secret.clone(),
            "A secret this site's backend reads. Use the same value you stored in Sitewright under Backend access.".into(),
            "yes, if the feature that uses it should work",
        ));
    }
    if let Some(llm) = &m.llm {
        for (i, model) in llm.models.iter().enumerate() {
            let prefix = model_env_prefix(&model.provider.id);
            rows.push((
                format!("{prefix}_API_KEY"),
                format!(
                    "API key for the AI model the site calls `{}`{} ({}, model `{}`).",
                    model.provider.id,
                    if i == 0 { ", its default" } else { "" },
                    model.provider.kind.as_str(),
                    model.provider.model
                ),
                "yes, for the site's AI features",
            ));
            rows.push((
                format!("{prefix}_BASE_URL"),
                match &model.provider.base_url {
                    Some(url) => format!("Where that model is reached. Packaged value: `{url}`."),
                    None => "Where that model is reached. Packaged value: the provider's standard address.".into(),
                },
                "no",
            ));
            rows.push((
                format!("{prefix}_MODEL"),
                format!("The model or deployment name. Packaged value: `{}`.", model.provider.model),
                "no",
            ));
        }
    }
    rows.push((
        "PORT".into(),
        "The port the site listens on inside the container. Default `8080`.".into(),
        "no",
    ));
    if m.backend {
        rows.push((
            "PUBLIC_HTTPS".into(),
            "Set to `false` only if visitors reach the site over plain HTTP; sign-in cookies are otherwise marked Secure and will not work without HTTPS. Default `true`.".into(),
            "no",
        ));
        rows.push((
            "TRUSTED_PROXIES".into(),
            "Networks of the reverse proxies in front of the site, comma-separated (for example `172.16.0.0/12`). Lets the site tell visitors apart for rate limits. Default: none.".into(),
            "recommended behind a proxy",
        ));
    }
    let _ = writeln!(g, "| Variable | What it is | Needed |\n|---|---|---|");
    for (name, what, needed) in &rows {
        let _ = writeln!(g, "| `{name}` | {what} | {needed} |");
    }
    let _ = writeln!(g);
    if m.secrets.is_empty() && m.llm.is_none() {
        let _ = writeln!(g, "This site needs no secrets.\n");
    } else {
        let _ = writeln!(
            g,
            "Put the secret ones in a file that only you can read, one `NAME=value` per line, \
             and keep it out of version control:\n\n```sh\ncat > {}.env <<'EOF'",
            m.name
        );
        for secret in &m.secrets {
            let _ = writeln!(g, "{secret}=");
        }
        if let Some(llm) = &m.llm {
            for model in &llm.models {
                let _ = writeln!(g, "{}_API_KEY=", model_env_prefix(&model.provider.id));
            }
        }
        let _ = writeln!(g, "EOF\nchmod 600 {}.env\n```\n", m.name);
        let _ = writeln!(
            g,
            "Fill in the values after each `=`. Sitewright cannot show you stored values, so \
             take them from where you first got them. If your host has its own secret store \
             (Docker or Kubernetes secrets, a cloud provider's), use that to set the same \
             variable names.\n"
        );
    }

    let _ = writeln!(g, "## 3. Run it\n");
    let env_file = if m.secrets.is_empty() && m.llm.is_none() {
        String::new()
    } else {
        format!("  --env-file {}.env \\\n", m.name)
    };
    let volume = if m.backend {
        format!("  -v {}-data:/data \\\n", m.name)
    } else {
        String::new()
    };
    let _ = writeln!(
        g,
        "```sh\ndocker run -d --name {name} --restart unless-stopped \\\n  -p 8080:8080 \\\n{env_file}{volume}  --read-only --cap-drop ALL --security-opt no-new-privileges \\\n  {tag}\n```\n",
        name = m.name,
        tag = package.tag
    );
    let _ = writeln!(
        g,
        "The site is then at `http://<server>:8080`. Check the start-up messages with \
         `docker logs {}`: a secret or model key that is not set is named there.\n",
        m.name
    );
    if m.backend {
        let _ = writeln!(
            g,
            "- **`/data` holds the site's database.** Keep the volume; without it the data is \
             lost when the container is replaced. The container runs as user {RUN_AS}, so a \
             host directory mounted there must be writable by that user \
             (`chown {RUN_AS}:{RUN_AS} <dir>`).\n\
             - The site's backend runs in the same sandbox as under Sitewright. If the \
             container fails to start with a message about the sandbox, your runtime's \
             seccomp profile blocks nested filters; Docker's and Podman's defaults allow them."
        );
    }
    let _ = writeln!(
        g,
        "- **HTTPS**: the container speaks plain HTTP. Put a reverse proxy that handles \
         certificates in front of it (Caddy, nginx, Traefik, or your host's load balancer).\n"
    );

    if m.backend {
        let _ = writeln!(g, "## 4. Bring the data along (optional)\n");
        let _ = writeln!(
            g,
            "The image has the site's code but not its data. To move the data too, download \
             **the site's data** from the same place you got the image (`{name}-data.db`), then, \
             before the first start:\n\n```sh\ndocker volume create {name}-data\ndocker run --rm -v {name}-data:/data -v \"$PWD\":/in --user 0 --entrypoint /sitewright {tag} \\\n  import-data /in/{name}-data.db\n```\n\n\
             After that, start the site as in step 3. Visitor accounts and their passwords \
             move with the data; signed-in visitors stay signed in.\n",
            name = m.name,
            tag = package.tag
        );
    }

    let _ = writeln!(g, "## What the site may reach\n");
    if m.hosts.is_empty() {
        let _ = writeln!(g, "The backend may not send requests to any outside host.\n");
    } else {
        let _ = writeln!(
            g,
            "The backend may send HTTPS requests to these hosts only, as approved in \
             Sitewright when the image was built:\n"
        );
        for host in &m.hosts {
            let _ = writeln!(g, "- `{host}`");
        }
        let _ = writeln!(g);
    }
    if let Some(llm) = &m.llm {
        let _ = writeln!(
            g,
            "AI requests are limited to {} per month and {} per minute, counted inside the \
             container's database. These numbers were set in Sitewright; to change them, \
             change them there and package the site again.\n",
            llm.monthly_requests, llm.per_minute
        );
    }

    let _ = writeln!(g, "## What is different from running under Sitewright\n");
    let _ = writeln!(
        g,
        "- No portal, no agent and no dashboards: to change the site, change it in \
         Sitewright, download a new image and replace the container (keep the volume).\n\
         - No visitor statistics are collected.\n\
         - Settings (approved hosts, AI limits, which secrets exist) are fixed when the \
         image is built. This guide is also inside the image, at `/SETUP.md`."
    );
    g
}
