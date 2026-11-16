//! Site store behaviour: create, edit, publish, rollback, delete and the routing table.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sw_core::{Config, DataDir};
use sw_sites::{Author, ImportFile, SiteError, SiteStore};

struct Env {
    _dir: tempfile::TempDir,
    data: DataDir,
    store: SiteStore,
}

async fn env_with(config_toml: &str) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path());
    data.ensure().unwrap();
    let db = sw_core::db::open(&data.db_file()).await.unwrap();
    sw_core::db::migrate(&db, &data.backups_dir())
        .await
        .unwrap();
    let config = Config::from_toml(config_toml).unwrap();
    let store = SiteStore::open(db, data.clone(), &config).await.unwrap();
    Env {
        _dir: dir,
        data,
        store,
    }
}

async fn env() -> Env {
    env_with("[domains]\nzones = [\"example.com\"]\npublic_scheme = \"https\"\npublic_port = 443\n")
        .await
}

fn file(path: &str, data: &str) -> ImportFile {
    ImportFile {
        path: path.into(),
        data: data.as_bytes().to_vec(),
    }
}

async fn live_index(env: &Env, host: &str) -> Option<String> {
    let routes = env.store.routes();
    let route = routes.get(host)?;
    Some(std::fs::read_to_string(route.root.join("index.html")).unwrap())
}

#[tokio::test]
async fn create_validates_names_and_reserves_hostnames() {
    let env = env().await;
    let site = env
        .store
        .create("hikes", "My Hikes", Author::User)
        .await
        .unwrap();
    assert_eq!(site.name.as_str(), "hikes");
    assert!(site.current_release.is_none());
    assert!(env.data.site_repo(&site.name).join(".git").is_dir());

    let info = env.store.info("hikes").await.unwrap();
    assert_eq!(info.url, "https://hikes.example.com");
    assert_eq!(info.hostnames.len(), 1);
    assert!(info.hostnames[0].is_primary);

    for bad in ["Hikes", "-x", "a.b", "", "admin", "www", "api", "mail"] {
        let err = env.store.create(bad, "t", Author::User).await.unwrap_err();
        assert!(err.is_user_error(), "{bad}: {err}");
    }
    assert!(matches!(
        env.store
            .create("hikes", "Again", Author::User)
            .await
            .unwrap_err(),
        SiteError::Conflict(_)
    ));
    assert!(env.store.create("ok", "  ", Author::User).await.is_err());
    // Not published yet, so nothing is routed.
    assert!(env.store.routes().is_empty());
}

#[tokio::test]
async fn admin_host_label_is_reserved() {
    let env =
        env_with("[domains]\nzones = [\"example.com\"]\nadmin_host = \"panel.example.com\"\n")
            .await;
    assert!(
        env.store
            .create("panel", "Nope", Author::User)
            .await
            .is_err()
    );
    assert!(env.store.create("fine", "Fine", Author::User).await.is_ok());
}

#[tokio::test]
async fn file_tools_are_confined_and_limited() {
    let env = env_with("[sites]\nmax_file_bytes = 100\nmax_site_bytes = 150\n").await;
    env.store.create("s", "S", Author::Agent).await.unwrap();

    env.store
        .write_file("s", "index.html", b"<h1>hi</h1>".to_vec())
        .await
        .unwrap();
    env.store
        .write_file("s", "css/site.css", b"body{}".to_vec())
        .await
        .unwrap();
    let files = env.store.list_files("s").await.unwrap();
    let names: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(names, ["css/site.css", "index.html"]);
    assert_eq!(
        env.store.read_file("s", "index.html").await.unwrap(),
        b"<h1>hi</h1>"
    );

    for bad in [
        "../x.html",
        "/etc/passwd",
        ".git/config",
        "a/../../b.html",
        "x.php",
        ".env",
        "css",
    ] {
        let err = env
            .store
            .write_file("s", bad, b"x".to_vec())
            .await
            .unwrap_err();
        assert!(matches!(err, SiteError::Path { .. }), "{bad}: {err}");
        assert!(env.store.read_file("s", bad).await.is_err(), "{bad}");
        assert!(env.store.delete_file("s", bad).await.is_err(), "{bad}");
    }

    // Per-file and per-site size limits.
    assert!(
        env.store
            .write_file("s", "big.html", vec![b'a'; 101])
            .await
            .is_err()
    );
    env.store
        .write_file("s", "a.html", vec![b'a'; 100])
        .await
        .unwrap();
    assert!(
        env.store
            .write_file("s", "b.html", vec![b'a'; 100])
            .await
            .is_err()
    );
    // Replacing a file counts its new size, not old plus new.
    env.store
        .write_file("s", "a.html", vec![b'b'; 100])
        .await
        .unwrap();

    env.store.delete_file("s", "css/site.css").await.unwrap();
    assert!(!env.data.site_repo(&"s".parse_site()).join("css").exists());
    assert!(env.store.delete_file("s", "css/site.css").await.is_err());
    assert!(env.store.read_file("s", "missing.html").await.is_err());
}

#[tokio::test]
async fn symlinks_in_the_working_tree_are_never_followed() {
    let env = env().await;
    env.store.create("s", "S", Author::Agent).await.unwrap();
    let repo = env.data.site_repo(&"s".parse_site());
    let secret = env.data.root().join("secret.html");
    std::fs::write(&secret, "secret").unwrap();
    std::os::unix::fs::symlink(&secret, repo.join("leak.html")).unwrap();
    std::os::unix::fs::symlink(env.data.root(), repo.join("up")).unwrap();

    assert!(env.store.read_file("s", "leak.html").await.is_err());
    assert!(
        env.store
            .write_file("s", "leak.html", b"x".to_vec())
            .await
            .is_err()
    );
    assert!(
        env.store
            .write_file("s", "up/x.html", b"x".to_vec())
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "secret");
    assert!(env.store.list_files("s").await.unwrap().is_empty());
}

#[tokio::test]
async fn publish_goes_live_and_rollback_switches_back() {
    let env = env().await;
    env.store
        .create("hikes", "Hikes", Author::User)
        .await
        .unwrap();

    // Publishing without an index.html is refused.
    let err = env
        .store
        .publish("hikes", "empty", Author::Agent)
        .await
        .unwrap_err();
    assert!(matches!(err, SiteError::Validation { .. }));

    env.store
        .write_file("hikes", "index.html", b"v1".to_vec())
        .await
        .unwrap();
    let first = env
        .store
        .publish("hikes", "first", Author::Agent)
        .await
        .unwrap();
    assert!(first.changed);
    assert_eq!(first.url, "https://hikes.example.com");
    assert_eq!(
        live_index(&env, "hikes.example.com").await.as_deref(),
        Some("v1")
    );

    // Publishing again with no changes is a no-op.
    let again = env
        .store
        .publish("hikes", "noop", Author::Agent)
        .await
        .unwrap();
    assert!(!again.changed);
    assert_eq!(again.release, first.release);

    // Edits are not live until published.
    env.store
        .write_file("hikes", "index.html", b"v2".to_vec())
        .await
        .unwrap();
    assert_eq!(
        live_index(&env, "hikes.example.com").await.as_deref(),
        Some("v1")
    );
    let second = env
        .store
        .publish("hikes", "second", Author::User)
        .await
        .unwrap();
    assert_eq!(
        live_index(&env, "hikes.example.com").await.as_deref(),
        Some("v2")
    );
    assert_eq!(second.release.author, "user");

    // Rollback switches the live release and resets the working tree.
    let back = env
        .store
        .rollback("hikes", None, Author::User)
        .await
        .unwrap();
    assert_eq!(back, first.release);
    assert_eq!(
        live_index(&env, "hikes.example.com").await.as_deref(),
        Some("v1")
    );
    assert_eq!(
        env.store.read_file("hikes", "index.html").await.unwrap(),
        b"v1"
    );
    assert!(
        env.store
            .rollback("hikes", None, Author::User)
            .await
            .is_err(),
        "nothing earlier"
    );

    // Roll forward again by sha prefix.
    let forward = env
        .store
        .rollback("hikes", Some(&second.release.sha[..8]), Author::User)
        .await
        .unwrap();
    assert_eq!(forward, second.release);
    assert_eq!(
        live_index(&env, "hikes.example.com").await.as_deref(),
        Some("v2")
    );
    assert!(
        env.store
            .rollback("hikes", Some("zz"), Author::User)
            .await
            .is_err()
    );
    assert!(
        env.store
            .rollback("hikes", Some("0000000"), Author::User)
            .await
            .is_err()
    );

    let info = env.store.info("hikes").await.unwrap();
    assert_eq!(info.releases.len(), 2);
    assert_eq!(info.site.current_release, Some(second.release.id));
    let changes = env
        .store
        .release_changes(&info.site.name, &second.release.sha)
        .await
        .unwrap();
    assert_eq!(changes, [('M', "index.html".to_owned())]);
}

#[tokio::test]
async fn import_mirrors_the_source_and_reports_skipped_files() {
    let env = env().await;
    env.store
        .create("hikes", "Hikes", Author::User)
        .await
        .unwrap();
    let report = env
        .store
        .import(
            "hikes",
            vec![
                file("index.html", "one"),
                file("old.html", "old"),
                file(".git/config", "evil"),
                file("run.sh", "evil"),
                file("../escape.html", "evil"),
            ],
            "import",
            Author::User,
        )
        .await
        .unwrap();
    assert_eq!(report.files, 2);
    assert_eq!(report.skipped.len(), 3);
    assert_eq!(
        live_index(&env, "hikes.example.com").await.as_deref(),
        Some("one")
    );

    // A second import replaces the tree: old.html disappears from the next release.
    env.store
        .import(
            "hikes",
            vec![file("index.html", "two"), file("css/a.css", "x")],
            "again",
            Author::User,
        )
        .await
        .unwrap();
    let routes = env.store.routes();
    let root = &routes.get("hikes.example.com").unwrap().root;
    assert!(!root.join("old.html").exists());
    assert!(root.join("css/a.css").exists());
    assert!(!env.data.root().join("escape.html").exists());

    // An import without index.html is refused and leaves the site untouched.
    assert!(
        env.store
            .import("hikes", vec![file("a.html", "x")], "bad", Author::User)
            .await
            .is_err()
    );
    assert_eq!(
        live_index(&env, "hikes.example.com").await.as_deref(),
        Some("two")
    );
    assert_eq!(
        env.store.read_file("hikes", "index.html").await.unwrap(),
        b"two"
    );
}

#[tokio::test]
async fn failed_health_check_rolls_the_release_back() {
    let env = env().await;
    let fail = Arc::new(AtomicBool::new(false));
    let flag = fail.clone();
    env.store.set_health_check(Arc::new(move |_route, _host| {
        let fail = flag.load(Ordering::SeqCst);
        Box::pin(async move { if fail { Err("boom".to_owned()) } else { Ok(()) } })
    }));
    env.store.create("s", "S", Author::User).await.unwrap();
    env.store
        .write_file("s", "index.html", b"good".to_vec())
        .await
        .unwrap();
    env.store.publish("s", "good", Author::Agent).await.unwrap();

    fail.store(true, Ordering::SeqCst);
    env.store
        .write_file("s", "index.html", b"bad".to_vec())
        .await
        .unwrap();
    let err = env
        .store
        .publish("s", "bad", Author::Agent)
        .await
        .unwrap_err();
    assert!(matches!(err, SiteError::HealthCheck { .. }), "{err}");
    assert_eq!(
        live_index(&env, "s.example.com").await.as_deref(),
        Some("good")
    );
}

#[tokio::test]
async fn old_release_directories_are_pruned_but_stay_restorable() {
    let env = env_with("[domains]\nzones = [\"example.com\"]\n[sites]\nkeep_releases = 2\n").await;
    env.store.create("s", "S", Author::User).await.unwrap();
    let mut shas = Vec::new();
    for n in 1..=4 {
        env.store
            .write_file("s", "index.html", format!("v{n}").into_bytes())
            .await
            .unwrap();
        shas.push(
            env.store
                .publish("s", &format!("v{n}"), Author::Agent)
                .await
                .unwrap()
                .release
                .sha,
        );
    }
    let name = "s".parse_site();
    assert!(!env.data.site_release(&name, &shas[0]).exists());
    assert!(!env.data.site_release(&name, &shas[1]).exists());
    assert!(env.data.site_release(&name, &shas[2]).exists());
    assert!(env.data.site_release(&name, &shas[3]).exists());

    // Rolling back to a pruned release re-exports it from git.
    env.store
        .rollback("s", Some(&shas[0]), Author::User)
        .await
        .unwrap();
    assert_eq!(
        live_index(&env, "s.example.com").await.as_deref(),
        Some("v1")
    );
}

#[tokio::test]
async fn delete_is_soft_restorable_and_purged_later() {
    let env =
        env_with("[domains]\nzones = [\"example.com\"]\n[sites]\npurge_after_days = 0\n").await;
    env.store.create("s", "S", Author::User).await.unwrap();
    env.store
        .write_file("s", "index.html", b"hi".to_vec())
        .await
        .unwrap();
    env.store
        .publish("s", "first", Author::Agent)
        .await
        .unwrap();

    env.store.delete("s", Author::User).await.unwrap();
    assert!(env.store.routes().get("s.example.com").is_none());
    assert!(env.store.get("s").await.is_err());
    assert!(env.store.list(false).await.unwrap().is_empty());
    assert_eq!(env.store.list(true).await.unwrap().len(), 1);
    assert!(
        env.store.create("s", "Again", Author::User).await.is_err(),
        "name is held while recoverable"
    );

    env.store.restore("s", Author::User).await.unwrap();
    assert_eq!(
        live_index(&env, "s.example.com").await.as_deref(),
        Some("hi")
    );

    env.store.delete("s", Author::User).await.unwrap();
    assert_eq!(env.store.purge_expired().await.unwrap(), ["s"]);
    assert!(env.store.find("s").await.unwrap().is_none());
    assert!(!env.data.site_dir(&"s".parse_site()).exists());
    // The name is free again.
    env.store.create("s", "Fresh", Author::User).await.unwrap();
}

#[tokio::test]
async fn extra_hostnames_route_to_the_same_release() {
    let env = env().await;
    env.store.create("s", "S", Author::User).await.unwrap();
    env.store
        .write_file("s", "index.html", b"hi".to_vec())
        .await
        .unwrap();
    env.store
        .publish("s", "first", Author::Agent)
        .await
        .unwrap();
    env.store
        .add_hostname("s", "Blog.Other.org", Author::User)
        .await
        .unwrap();
    assert_eq!(
        live_index(&env, "blog.other.org").await.as_deref(),
        Some("hi")
    );
    assert!(
        env.store
            .add_hostname("s", "blog.other.org", Author::User)
            .await
            .is_err()
    );
    assert!(
        env.store
            .add_hostname("s", "admin.example.com", Author::User)
            .await
            .is_err()
    );
    assert!(
        env.store
            .remove_hostname("s", "s.example.com", Author::User)
            .await
            .is_err(),
        "primary stays"
    );
    // Promote the extra hostname: links use it, and the old primary stays as an extra.
    env.store
        .add_hostname_as("s", "blog.other.org", true, Author::User)
        .await
        .unwrap();
    let info = env.store.info("s").await.unwrap();
    assert_eq!(info.url, "https://blog.other.org");
    assert_eq!(info.hostnames.iter().filter(|h| h.is_primary).count(), 1);
    assert_eq!(
        live_index(&env, "s.example.com").await.as_deref(),
        Some("hi")
    );
    assert!(
        env.store
            .remove_hostname("s", "blog.other.org", Author::User)
            .await
            .is_err(),
        "now primary"
    );
    // A new hostname can be attached and promoted in one step; then demote back.
    env.store
        .add_hostname_as("s", "new.example.net", true, Author::User)
        .await
        .unwrap();
    assert_eq!(
        env.store.info("s").await.unwrap().url,
        "https://new.example.net"
    );
    assert_eq!(
        env.store
            .routes()
            .get("blog.other.org")
            .unwrap()
            .primary_hostname,
        "new.example.net"
    );
    env.store
        .add_hostname_as("s", "s.example.com", true, Author::User)
        .await
        .unwrap();
    env.store
        .remove_hostname("s", "new.example.net", Author::User)
        .await
        .unwrap();
    env.store
        .remove_hostname("s", "blog.other.org", Author::User)
        .await
        .unwrap();
    assert!(env.store.routes().get("blog.other.org").is_none());
}

#[tokio::test]
async fn routes_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path());
    data.ensure().unwrap();
    let config = Config::from_toml("[domains]\nzones = [\"example.com\"]\n").unwrap();
    {
        let db = sw_core::db::open(&data.db_file()).await.unwrap();
        sw_core::db::migrate(&db, &data.backups_dir())
            .await
            .unwrap();
        let store = SiteStore::open(db.clone(), data.clone(), &config)
            .await
            .unwrap();
        store.create("s", "S", Author::User).await.unwrap();
        store
            .write_file("s", "index.html", b"hi".to_vec())
            .await
            .unwrap();
        store.publish("s", "first", Author::Agent).await.unwrap();
        db.close().await;
    }
    let db = sw_core::db::open(&data.db_file()).await.unwrap();
    let store = SiteStore::open(db, data, &config).await.unwrap();
    assert!(store.routes().get("s.example.com").is_some());
}

trait ParseSite {
    fn parse_site(&self) -> sw_core::SiteName;
}
impl ParseSite for str {
    fn parse_site(&self) -> sw_core::SiteName {
        sw_core::SiteName::parse(self).unwrap()
    }
}
