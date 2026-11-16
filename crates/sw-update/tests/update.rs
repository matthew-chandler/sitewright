//! The updater against a mock release server: signed releases install, anything else
//! is refused, and rollback restores the previous binary and database.

#![allow(clippy::unwrap_used)]

use std::io::{Cursor, Write};

use minisign::KeyPair;
use semver::Version;
use serde_json::json;
use sha2::{Digest, Sha256};
use sw_update::{CURRENT_VERSION, UpdateError, Updater, install, previous_path, rollback, target};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fake_binary(marker: &str) -> Vec<u8> {
    let mut bytes = b"\x7fELF".to_vec();
    bytes.extend_from_slice(marker.as_bytes());
    bytes.resize(4096, 0);
    bytes
}

/// A release archive laid out as the release workflow builds it.
fn archive(binary: &[u8]) -> Vec<u8> {
    let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::default(),
    ));
    for (name, data, mode) in [
        ("README.md", b"readme".as_slice(), 0o644),
        ("sitewright", binary, 0o755),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(mode);
        header.set_cksum();
        tar.append_data(&mut header, format!("sitewright-{}/{name}", target()), data)
            .unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap()
}

struct Published {
    server: MockServer,
    public_key: String,
}

/// Publish `tag` on a mock release server. `tamper` lets a test corrupt one asset.
async fn publish(tag: &str, binary: &[u8], tamper: &str) -> Published {
    let keys = KeyPair::generate_unencrypted_keypair().unwrap();
    let mut archive = archive(binary);
    let signature = minisign::sign(None, &keys.sk, Cursor::new(&archive), None, None)
        .unwrap()
        .into_string();
    let mut checksum = format!(
        "{}  sitewright-{}.tar.gz\n",
        hex::encode(Sha256::digest(&archive)),
        target()
    );
    match tamper {
        "archive" => archive.extend_from_slice(b"extra"),
        "checksum" => checksum = format!("{}  x\n", "0".repeat(64)),
        _ => {}
    }

    let server = MockServer::start().await;
    let name = format!("sitewright-{}.tar.gz", target());
    let asset = |suffix: &str| json!({"name": format!("{name}{suffix}"), "browser_download_url": format!("{}/dl/{name}{suffix}", server.uri())});
    let mut assets = vec![asset(""), asset(".sha256"), asset(".minisig")];
    if tamper == "unsigned" {
        assets.pop();
    }
    if tamper == "other-platform" {
        assets = vec![
            json!({"name": "sitewright-riscv64.tar.gz", "browser_download_url": format!("{}/dl/x", server.uri())}),
        ];
    }
    Mock::given(method("GET"))
        .and(path("/repos/acme/sitewright/releases/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"tag_name": tag, "html_url": "https://example.test/release", "assets": assets}),
        ))
        .mount(&server)
        .await;
    for (suffix, body) in [
        ("", archive),
        (".sha256", checksum.into_bytes()),
        (".minisig", signature.into_bytes()),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/dl/{name}{suffix}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
    }
    Published {
        server,
        public_key: keys.pk.to_box().unwrap().into_string(),
    }
}

fn updater(published: &Published) -> Updater {
    Updater::new(
        &published.server.uri(),
        "acme/sitewright",
        &published.public_key,
    )
    .unwrap()
}

#[tokio::test]
async fn a_signed_release_downloads_verifies_and_installs_with_rollback() {
    let new_binary = fake_binary("version two");
    let published = publish("v99.1.0", &new_binary, "").await;
    let updater = updater(&published);

    let release = updater.latest().await.unwrap().unwrap();
    let current = Version::parse(CURRENT_VERSION).unwrap();
    assert_eq!(release.version, Version::new(99, 1, 0));
    assert!(release.is_newer_than(&current));
    let binary = updater.download_verified(&release).await.unwrap();
    assert_eq!(binary, new_binary);

    // Install over a stand-in executable; the old one is kept beside it.
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("sitewright");
    std::fs::write(&exe, fake_binary("version one")).unwrap();
    let previous = install(&binary, &exe).unwrap();
    assert_eq!(previous, previous_path(&exe));
    assert_eq!(std::fs::read(&exe).unwrap(), new_binary);
    assert_eq!(
        std::fs::read(&previous).unwrap(),
        fake_binary("version one")
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(!dir.path().join("sitewright.new").exists());

    // Rollback swaps them; rolling back again returns to the new version.
    rollback(&exe).unwrap();
    assert_eq!(std::fs::read(&exe).unwrap(), fake_binary("version one"));
    rollback(&exe).unwrap();
    assert_eq!(std::fs::read(&exe).unwrap(), new_binary);
    std::fs::remove_file(previous_path(&exe)).unwrap();
    assert!(matches!(rollback(&exe), Err(UpdateError::State(_))));
}

#[tokio::test]
async fn anything_that_does_not_verify_is_refused() {
    for (tamper, expected) in [
        ("archive", "signature does not match"),
        ("checksum", "checksum does not match"),
        ("unsigned", "no signature"),
    ] {
        let published = publish("v99.0.0", &fake_binary("x"), tamper).await;
        let updater = updater(&published);
        let release = updater.latest().await.unwrap().unwrap();
        let err = updater.download_verified(&release).await.unwrap_err();
        assert!(
            matches!(err, UpdateError::Verification(_)),
            "{tamper}: {err}"
        );
        assert!(
            err.to_string().contains(expected) && err.to_string().contains("NOT installed"),
            "{tamper}: {err}"
        );
    }

    // Signed, but by someone else's key (here: the real project key is not the signer).
    let published = publish("v99.0.0", &fake_binary("x"), "").await;
    let official_key = Updater::new(
        &published.server.uri(),
        "acme/sitewright",
        sw_update::PUBLIC_KEY,
    )
    .unwrap();
    let release = official_key.latest().await.unwrap().unwrap();
    let err = official_key.download_verified(&release).await.unwrap_err();
    assert!(matches!(err, UpdateError::Verification(_)), "{err}");

    // A correctly signed archive that is not a Linux executable.
    let published = publish("v99.0.0", b"#!/bin/sh\necho pwned\n", "").await;
    let updater_ok = updater(&published);
    let release = updater_ok.latest().await.unwrap().unwrap();
    assert!(matches!(
        updater_ok.download_verified(&release).await.unwrap_err(),
        UpdateError::Protocol(_)
    ));

    let published = publish("v99.0.0", &fake_binary("x"), "other-platform").await;
    let updater_other = updater(&published);
    let release = updater_other.latest().await.unwrap().unwrap();
    assert!(matches!(
        updater_other.download_verified(&release).await.unwrap_err(),
        UpdateError::NoBuild { .. }
    ));
}

#[tokio::test]
async fn versions_are_compared_and_automatic_updates_stay_within_a_major_version() {
    let published = publish("v1.4.2", &fake_binary("x"), "").await;
    let release = updater(&published).latest().await.unwrap().unwrap();
    let v = |s: &str| Version::parse(s).unwrap();
    assert!(release.is_newer_than(&v("1.4.1")) && release.is_newer_than(&v("0.9.0")));
    assert!(!release.is_newer_than(&v("1.4.2")) && !release.is_newer_than(&v("2.0.0")));
    assert!(
        release.auto_installable_from(&v("1.4.1")) && release.auto_installable_from(&v("1.0.0"))
    );
    assert!(
        !release.auto_installable_from(&v("0.9.0")),
        "a new major version is never installed automatically"
    );
    assert!(!release.auto_installable_from(&v("1.4.2")));

    // No releases yet, and an unreachable server.
    let empty = MockServer::start().await;
    assert_eq!(
        Updater::new(&empty.uri(), "acme/sitewright", "x")
            .unwrap()
            .latest()
            .await
            .unwrap(),
        None
    );
    assert!(matches!(
        Updater::new("http://127.0.0.1:9", "a/b", "x")
            .unwrap()
            .latest()
            .await,
        Err(UpdateError::Network(_))
    ));
}

#[test]
fn a_rollback_restores_the_database_from_before_the_migration() {
    let dir = tempfile::tempdir().unwrap();
    let backups = dir.path().join("backups");
    std::fs::create_dir_all(&backups).unwrap();
    let db = dir.path().join("sitewright.db");
    let backup = backups.join("sitewright-1.db");
    std::fs::File::create(&backup)
        .unwrap()
        .write_all(b"old schema")
        .unwrap();
    std::fs::write(&db, b"new schema").unwrap();
    std::fs::write(dir.path().join("sitewright.db-wal"), b"wal").unwrap();

    // Nothing recorded: nothing to restore.
    assert_eq!(sw_update::request_database_restore(&backups).unwrap(), None);
    assert_eq!(
        sw_update::restore_database_if_requested(&backups, &db).unwrap(),
        None
    );

    // This version migrated; a rollback asks for the restore; the next start performs it.
    sw_update::record_migration(&backups, &backup).unwrap();
    assert_eq!(
        sw_update::restore_database_if_requested(&backups, &db).unwrap(),
        None,
        "not until a rollback asks"
    );
    assert_eq!(
        sw_update::request_database_restore(&backups).unwrap(),
        Some(backup.clone())
    );
    assert_eq!(
        sw_update::restore_database_if_requested(&backups, &db).unwrap(),
        Some(backup)
    );
    assert_eq!(std::fs::read(&db).unwrap(), b"old schema");
    assert!(!dir.path().join("sitewright.db-wal").exists());
    let kept = dir
        .path()
        .join(format!("sitewright.db.from-{CURRENT_VERSION}"));
    assert_eq!(
        std::fs::read(kept).unwrap(),
        b"new schema",
        "the newer database is kept, not deleted"
    );
    // Once done it is not repeated.
    assert_eq!(
        sw_update::restore_database_if_requested(&backups, &db).unwrap(),
        None
    );
}

/// The release workflow signs with the `minisign` command-line tool, not with the Rust
/// crate the other tests use. Check that a signature made by the real tool verifies.
/// Skipped when `minisign` is not installed.
#[test]
fn signatures_made_by_the_minisign_cli_verify() {
    let installed = std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| dir.join("minisign").is_file())
    });
    if !installed {
        eprintln!("skipping: minisign is not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name);
    let run = |args: &[&str]| {
        let output = std::process::Command::new("minisign")
            .args(args)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "minisign {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let archive = archive(&fake_binary("signed by the cli"));
    std::fs::write(path("a.tar.gz"), &archive).unwrap();
    run(&["-G", "-W", "-f", "-p", "key.pub", "-s", "key.sec"]);
    run(&[
        "-S",
        "-s",
        "key.sec",
        "-m",
        "a.tar.gz",
        "-x",
        "a.tar.gz.minisig",
        "-t",
        "sitewright test",
    ]);

    let public_key = std::fs::read_to_string(path("key.pub")).unwrap();
    let signature = std::fs::read_to_string(path("a.tar.gz.minisig")).unwrap();
    let checksum = format!("{}  a.tar.gz\n", hex::encode(Sha256::digest(&archive)));
    sw_update::verify(&archive, &signature, &checksum, &public_key).unwrap();

    // The bare base64 key (as printed in the README) works too, and tampering is caught.
    let bare = public_key.lines().last().unwrap();
    sw_update::verify(&archive, &signature, &checksum, bare).unwrap();
    let mut tampered = archive.clone();
    tampered[100] ^= 1;
    assert!(sw_update::verify(&tampered, &signature, &checksum, &public_key).is_err());
    assert!(
        sw_update::verify(&archive, &signature, &checksum, sw_update::PUBLIC_KEY).is_err(),
        "signed by a different key"
    );
}

/// The key compiled into the binary is the one in the README and the installer.
#[test]
fn the_compiled_in_key_matches_the_published_one() {
    let key = sw_update::PUBLIC_KEY.lines().last().unwrap().trim();
    assert!(key.starts_with("RW") && key.len() == 56, "{key}");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for file in ["README.md", "scripts/install.sh", "docs/install.md"] {
        let text = std::fs::read_to_string(root.join(file)).unwrap();
        assert!(
            text.contains(key),
            "{file} does not show the current release key"
        );
    }
}
