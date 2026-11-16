//! `sitewright update`: install the latest signed release, or roll the last one back.

use anyhow::Context;
use semver::Version;
use serde_json::{Value, json};
use sw_core::config::Loaded;
use sw_update::{CURRENT_VERSION, Updater};

use crate::client::AdminClient;

pub async fn run(loaded: &Loaded, check_only: bool, rollback: bool) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("cannot find the running executable")?;
    if rollback {
        sw_update::rollback(&exe)?;
        println!("Restored the previous version ({}).", exe.display());
        match sw_update::request_database_restore(&loaded.data_dir.backups_dir())? {
            Some(backup) => println!(
                "The database from before the update ({}) will be restored at the next start.",
                backup.display()
            ),
            None => println!("The update did not change the database, so it is left as it is."),
        }
        return restart(loaded).await;
    }

    let current = Version::parse(CURRENT_VERSION)?;
    let updater = Updater::official()?;
    let Some(release) = updater.latest().await? else {
        println!("No releases have been published yet. You are running {current}.");
        return Ok(());
    };
    if !release.is_newer_than(&current) {
        println!("You are running {current}, the latest release.");
        return Ok(());
    }
    println!(
        "Version {} is available (you are running {current}): {}",
        release.version, release.url
    );
    if check_only {
        println!("Run `sitewright update` to install it.");
        return Ok(());
    }

    println!("Downloading and verifying...");
    let binary = updater.download_verified(&release).await?;
    println!("Signature and checksum verified.");
    let previous = sw_update::install(&binary, &exe)
        .with_context(|| format!("cannot replace {}", exe.display()))?;
    println!(
        "Installed {}. The previous version is kept at {}.",
        release.version,
        previous.display()
    );
    restart(loaded).await
}

/// Ask the running daemon to exit so its supervisor starts the binary now on disk.
async fn restart(loaded: &Loaded) -> anyhow::Result<()> {
    let restarted: anyhow::Result<Value> = match AdminClient::new(&loaded.data_dir) {
        Ok(client) => client.post("/v1/admin/restart", &json!({})).await,
        Err(e) => Err(e),
    };
    match restarted {
        Ok(_) => println!("The daemon is restarting. Undo with `sitewright update --rollback`."),
        Err(_) => println!("No running daemon was found; start it to use the new version."),
    }
    Ok(())
}
