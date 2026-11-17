//! `sitewright copilot ...`: install GitHub's Copilot runtime and check that it works.
//!
//! The runtime is a large separate program (ADR-0008). It is not part of Sitewright's
//! release; `install` downloads the exact version the SDK was built for and verifies
//! its SHA-256 against the value pinned here.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use sw_core::config::{AuthStyle, ProviderConfig, ProviderKind};
use sw_llm::copilot::Copilot;

/// The runtime version that matches the `github-copilot-sdk` crate in `Cargo.lock`
/// (its `cli-version.txt`). Update both together.
pub const RUNTIME_VERSION: &str = "1.0.112";

/// Release assets and their SHA-256, from the SDK's `cli-version.txt`.
const ASSETS: &[(&str, &str)] = &[
    (
        "copilot-linux-x64.tar.gz",
        "dfe7e59acf04f0b7f59b024ea9db101a9246503e9bf7152eb668fbb2a469b151",
    ),
    (
        "copilot-linux-arm64.tar.gz",
        "491d18303505935675dc20987ef0c5f3ae899f81e66c53d567ad97a8b03e504e",
    ),
    (
        "copilot-linuxmusl-x64.tar.gz",
        "027f1129a5617439cd520fa118e89df18bcc3b7079675a985aecd667865f0f16",
    ),
    (
        "copilot-linuxmusl-arm64.tar.gz",
        "c771b2d097e341f1560b06b3d8cff306d58294dd2d667f4f07de5d2afbcf175f",
    ),
];

/// Environment variables the token for `check` is read from, in order.
const TOKEN_VARS: &[&str] = &["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"];

/// Where the runtime lives inside the data directory.
pub fn runtime_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("copilot")
}

pub fn runtime_program(data_dir: &Path) -> PathBuf {
    runtime_dir(data_dir).join("copilot")
}

/// The asset for this machine: architecture, and musl when the system has no glibc.
fn asset_name() -> anyhow::Result<&'static str> {
    let musl = !Path::new("/lib64/ld-linux-x86-64.so.2").exists()
        && !Path::new("/lib/ld-linux-aarch64.so.1").exists()
        && std::fs::read_dir("/lib").is_ok_and(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("ld-musl-"))
        });
    Ok(match (std::env::consts::ARCH, musl) {
        ("x86_64", false) => "copilot-linux-x64.tar.gz",
        ("x86_64", true) => "copilot-linuxmusl-x64.tar.gz",
        ("aarch64", false) => "copilot-linux-arm64.tar.gz",
        ("aarch64", true) => "copilot-linuxmusl-arm64.tar.gz",
        (other, _) => bail!("there is no Copilot runtime for {other}"),
    })
}

fn pinned_hash(asset: &str) -> anyhow::Result<&'static str> {
    ASSETS
        .iter()
        .find(|(name, _)| *name == asset)
        .map(|(_, hash)| *hash)
        .with_context(|| format!("no pinned checksum for {asset}"))
}

/// Download, verify and unpack the runtime into the data directory.
pub async fn install(data_dir: &Path, from: Option<&Path>) -> anyhow::Result<()> {
    let asset = asset_name()?;
    let expected = pinned_hash(asset)?;
    let dir = runtime_dir(data_dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    set_mode(&dir, 0o700)?;
    let archive = dir.join("download.tar.gz");

    let actual = match from {
        Some(path) => {
            println!("Using {} as {asset}.", path.display());
            std::fs::copy(path, &archive).with_context(|| format!("copying {}", path.display()))?;
            hash_file(&archive)?
        }
        None => {
            let url = format!(
                "https://github.com/github/copilot-cli/releases/download/v{RUNTIME_VERSION}/{asset}"
            );
            println!("Downloading {url}");
            download(&url, &archive).await?
        }
    };
    if actual != expected {
        let _ = std::fs::remove_file(&archive);
        bail!(
            "the download does not match the pinned checksum for {asset} {RUNTIME_VERSION}\n  expected {expected}\n  got      {actual}"
        );
    }
    println!("Checksum verified.");

    let program = runtime_program(data_dir);
    let unpacked = unpack(&archive, &dir)?;
    let _ = std::fs::remove_file(&archive);
    std::fs::rename(&unpacked, &program)
        .with_context(|| format!("installing {}", program.display()))?;
    set_mode(&program, 0o755)?;
    let size = std::fs::metadata(&program)?.len();
    println!(
        "Installed the Copilot runtime {RUNTIME_VERSION} at {} ({} MB).",
        program.display(),
        size / 1_000_000
    );
    println!("Next: `sitewright copilot check` with a GitHub token in COPILOT_GITHUB_TOKEN.");
    Ok(())
}

async fn download(url: &str, to: &Path) -> anyhow::Result<String> {
    sw_core::crypto::install_default_provider();
    let http = reqwest::Client::builder()
        .user_agent(concat!("sitewright/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let response = http.get(url).send().await?.error_for_status()?;
    let mut file =
        std::fs::File::create(to).with_context(|| format!("creating {}", to.display()))?;
    let mut hasher = Sha256::new();
    let mut body = response.bytes_stream();
    let mut total = 0u64;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.context("the download was interrupted")?;
        hasher.update(&chunk);
        file.write_all(&chunk)?;
        total += chunk.len() as u64;
    }
    file.flush()?;
    println!("Downloaded {} MB.", total / 1_000_000);
    Ok(hex(&hasher.finalize()))
}

fn hash_file(path: &Path) -> anyhow::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let n = std::io::Read::read(&mut file, &mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Extract the one file named `copilot` from the archive into `dir`, under a temporary
/// name. Nothing else in the archive is written.
fn unpack(archive: &Path, dir: &Path) -> anyhow::Result<PathBuf> {
    let file = std::fs::File::open(archive)?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let out = dir.join("copilot.new");
    for entry in tar.entries()? {
        let mut entry = entry?;
        let is_program = entry.header().entry_type().is_file()
            && entry.path()?.file_name().is_some_and(|n| n == "copilot");
        if is_program {
            let mut target = std::fs::File::create(&out)?;
            std::io::copy(&mut entry, &mut target)?;
            return Ok(out);
        }
    }
    bail!("the archive has no `copilot` program in it")
}

fn set_mode(path: &Path, mode: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting permissions on {}", path.display()))
}

/// Resident memory of a process, current and peak, in megabytes.
fn memory_mb(pid: u32) -> Option<(u64, u64)> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let field = |name: &str| {
        status
            .lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|kb| kb.parse::<u64>().ok())
            .map(|kb| kb / 1024)
    };
    Some((field("VmRSS:")?, field("VmHWM:")?))
}

/// Start the runtime, list the models, make one model call with a tool, and report how
/// much memory the runtime used.
pub async fn check(data_dir: &Path, model: Option<String>) -> anyhow::Result<()> {
    sw_llm::copilot::set_default_runtime(runtime_program(data_dir));
    let program = sw_llm::copilot::runtime_path().context("no runtime path")?;
    if !program.is_file() {
        bail!(
            "the Copilot runtime is not at {}; run `sitewright copilot install` first",
            program.display()
        );
    }
    let token = TOKEN_VARS
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
        .with_context(|| {
            format!(
                "set a GitHub token with the \"Copilot Requests\" permission in {}",
                TOKEN_VARS[0]
            )
        })?;
    let config = |model: &str| ProviderConfig {
        id: "copilot-check".into(),
        kind: ProviderKind::Copilot,
        base_url: None,
        model: model.to_owned(),
        auth: AuthStyle::Bearer,
        key_ref: None,
        timeout_secs: 120,
        max_retries: 0,
        max_output_tokens: None,
        extra_body: toml::Table::new(),
    };

    println!("Runtime: {}", program.display());
    let lister = Copilot::new(&config("unused"), Some(token.clone()))?;
    let models = lister
        .models()
        .await
        .context("the runtime started, but listing models failed (is the token valid?)")?;
    println!("Started and signed in. Models this account can use:");
    for m in &models {
        println!("  {m}");
    }
    if let Some((now, peak)) = lister.runtime_pid().and_then(memory_mb) {
        println!("Memory when idle: {now} MB (peak so far {peak} MB)");
    }
    drop(lister);

    let model = match model.or_else(|| models.first().cloned()) {
        Some(model) => model,
        None => bail!("the account has no models"),
    };
    println!("Testing a model call with a tool, using {model} ...");
    let provider = Copilot::new(&config(&model), Some(token))?;
    // Sample memory while the call runs: the runtime stops as soon as it is over.
    let peak = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let test = {
        let call = sw_llm::test_connection(&provider);
        tokio::pin!(call);
        loop {
            tokio::select! {
                result = &mut call => break result,
                _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {
                    if let Some((_, hwm)) = provider.runtime_pid().and_then(memory_mb) {
                        peak.fetch_max(hwm, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
        }
    };
    let peak = peak.load(std::sync::atomic::Ordering::Relaxed);
    if peak > 0 {
        println!("Peak memory during the call: {peak} MB");
    }
    println!(
        "{} after {} ms: {}",
        if test.ok { "OK" } else { "FAILED" },
        test.latency_ms,
        test.detail
    );
    if !test.ok {
        bail!("the Copilot check failed");
    }
    println!(
        "Copilot works. Add it in the portal under Settings, kind \"GitHub Copilot\", model {model}."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_asset_has_a_full_checksum_and_this_machine_has_an_asset() {
        for (name, hash) in ASSETS {
            assert_eq!(hash.len(), 64, "{name}");
            assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        }
        let asset = asset_name().expect("an asset for this machine");
        assert!(pinned_hash(asset).is_ok());
    }

    #[test]
    fn a_wrong_archive_is_refused_and_a_right_one_is_unpacked() {
        let dir = tempfile::tempdir().expect("tempdir");
        // An archive with a decoy and the program.
        let archive = dir.path().join("a.tar.gz");
        {
            let gz = flate2::write::GzEncoder::new(
                std::fs::File::create(&archive).expect("create"),
                flate2::Compression::fast(),
            );
            let mut tar = tar::Builder::new(gz);
            for (path, body) in [
                ("package/README.md", "hi"),
                ("package/copilot", "#!program"),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                tar.append_data(&mut header, path, body.as_bytes())
                    .expect("append");
            }
            tar.into_inner().expect("tar").finish().expect("gz");
        }
        let out = unpack(&archive, dir.path()).expect("unpack");
        assert_eq!(std::fs::read_to_string(&out).expect("read"), "#!program");
        assert!(!dir.path().join("package").exists());

        // The pinned checksum rejects it as a runtime.
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let error = runtime
            .block_on(install(dir.path(), Some(&archive)))
            .expect_err("a made-up archive must not install");
        assert!(error.to_string().contains("pinned checksum"));
        assert!(!runtime_program(dir.path()).exists());
    }
}
