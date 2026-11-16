//! Builds archives and reads them back with the `tar` crate, checking the structure that
//! `docker load` relies on.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sw_image::{Entry, ImageError, ImageSpec, Layer, Source, write_docker_archive};

const CREATED: i64 = 1_700_000_000;
const BINARY: &[u8] = b"\x7fELF not really a binary";

fn file(path: &str, content: &[u8]) -> Entry {
    Entry::File {
        path: path.to_owned(),
        source: Source::Bytes(content.to_vec()),
        mode: 0o644,
        uid: 0,
        gid: 0,
    }
}

/// Two layers: the binary (streamed from `binary_path`), then a site and a data directory.
fn spec(binary_path: &Path) -> ImageSpec {
    ImageSpec {
        repo_tag: "sitewright-site/hikes:abc12345".to_owned(),
        architecture: "amd64".to_owned(),
        layers: vec![
            Layer {
                entries: vec![Entry::File {
                    path: "usr/local/bin/sitewright".to_owned(),
                    source: Source::Path(binary_path.to_path_buf()),
                    mode: 0o755,
                    uid: 0,
                    gid: 0,
                }],
            },
            Layer {
                entries: vec![
                    // Deliberately not in path order.
                    file("site/posts/2026/first.html", b"<p>first</p>"),
                    Entry::Dir {
                        path: "data".to_owned(),
                        mode: 0o700,
                        uid: 65532,
                        gid: 65532,
                    },
                    file("site/index.html", b"<h1>Hikes</h1>"),
                    file("site/assets/style.css", b"body{}"),
                ],
            },
        ],
        entrypoint: vec!["/usr/local/bin/sitewright".to_owned()],
        cmd: vec!["serve-site".to_owned(), "/site".to_owned()],
        env: vec![
            ("SITEWRIGHT_DATA".to_owned(), "/data".to_owned()),
            ("PATH".to_owned(), "/usr/local/bin".to_owned()),
        ],
        exposed_ports: vec![8080],
        volumes: vec!["/data".to_owned()],
        user: "65532:65532".to_owned(),
        working_dir: "/".to_owned(),
        labels: vec![(
            "org.opencontainers.image.title".to_owned(),
            "hikes".to_owned(),
        )],
        created_unix_secs: CREATED,
    }
}

fn binary_in(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("fake-binary");
    std::fs::write(&path, BINARY).unwrap();
    path
}

/// What one tar entry looked like.
#[derive(Debug)]
struct Seen {
    path: String,
    kind: tar::EntryType,
    mode: u32,
    uid: u64,
    gid: u64,
    mtime: u64,
    user: String,
    group: String,
    content: Vec<u8>,
}

fn read_tar(bytes: &[u8]) -> Vec<Seen> {
    let mut archive = tar::Archive::new(bytes);
    archive
        .entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let header = entry.header().clone();
            // No PAX records anywhere: long names use the GNU extension.
            assert!(entry.pax_extensions().unwrap().is_none());
            // The device major and minor fields (bytes 329..345) are left blank.
            assert!(header.as_bytes()[329..345].iter().all(|&byte| byte == 0));
            let mut content = Vec::new();
            entry.read_to_end(&mut content).unwrap();
            assert_eq!(content.len() as u64, header.size().unwrap());
            Seen {
                path: String::from_utf8(entry.path_bytes().into_owned()).unwrap(),
                kind: header.entry_type(),
                mode: header.mode().unwrap(),
                uid: header.uid().unwrap(),
                gid: header.gid().unwrap(),
                mtime: header.mtime().unwrap(),
                user: header.username().unwrap().unwrap_or("").to_owned(),
                group: header.groupname().unwrap().unwrap_or("").to_owned(),
                content,
            }
        })
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// `(path, is_dir, mode, uid, size)` for each entry, in archive order.
fn summary(entries: &[Seen]) -> Vec<(&str, bool, u32, u64, usize)> {
    entries
        .iter()
        .map(|seen| {
            (
                seen.path.as_str(),
                seen.kind == tar::EntryType::Directory,
                seen.mode,
                seen.uid,
                seen.content.len(),
            )
        })
        .collect()
}

fn dir_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn archive_has_the_structure_docker_load_expects() {
    let inputs = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let out = output.path().join("image.tar");
    let info = write_docker_archive(&spec(&binary_in(inputs.path())), &out).unwrap();

    let bytes = std::fs::read(&out).unwrap();
    assert_eq!(info.bytes, bytes.len() as u64);
    // Nothing but the archive is left: the temporary layer files are gone.
    assert_eq!(dir_entries(output.path()), ["image.tar"]);

    let outer = read_tar(&bytes);
    for seen in &outer {
        assert_eq!(seen.mtime, CREATED as u64);
        assert_eq!((seen.uid, seen.gid), (0, 0));
        assert_eq!((seen.user.as_str(), seen.group.as_str()), ("", ""));
    }
    let files: BTreeMap<&str, &[u8]> = outer
        .iter()
        .map(|seen| (seen.path.as_str(), seen.content.as_slice()))
        .collect();
    assert_eq!(files.len(), outer.len(), "no path appears twice");

    // manifest.json names the config and the layers by paths that exist in the archive.
    let manifest: Value = serde_json::from_slice(files["manifest.json"]).unwrap();
    let image_hex = info.image_id.strip_prefix("sha256:").unwrap();
    let config_path = format!("blobs/sha256/{image_hex}");
    let layer_paths: Vec<String> = manifest[0]["Layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|path| path.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(layer_paths.len(), 2);
    assert_eq!(
        manifest,
        json!([{
            "Config": config_path,
            "RepoTags": ["sitewright-site/hikes:abc12345"],
            "Layers": layer_paths,
        }])
    );
    // Key order matters to nobody but is part of the promise of identical bytes.
    assert!(files["manifest.json"].starts_with(br#"[{"Config":"blobs/sha256/"#));

    // The image id is the hash of the exact config bytes.
    let config_bytes = files[config_path.as_str()];
    assert_eq!(sha256_hex(config_bytes), image_hex);
    let config: Value = serde_json::from_slice(config_bytes).unwrap();

    // Each diff_id is the hash of the layer tar that manifest.json lists at that position,
    // and each blob is stored under its own hash.
    let diff_ids = config["rootfs"]["diff_ids"].as_array().unwrap();
    assert_eq!(diff_ids.len(), 2);
    for (layer_path, diff_id) in layer_paths.iter().zip(diff_ids) {
        let layer_hex = sha256_hex(files[layer_path.as_str()]);
        assert_eq!(diff_id.as_str().unwrap(), format!("sha256:{layer_hex}"));
        assert_eq!(layer_path, &format!("blobs/sha256/{layer_hex}"));
    }
    assert_eq!(config["rootfs"]["type"], "layers");

    assert_eq!(config["architecture"], "amd64");
    assert_eq!(config["os"], "linux");
    assert_eq!(config["created"], "2023-11-14T22:13:20Z");
    assert_eq!(
        config["config"],
        json!({
            "Entrypoint": ["/usr/local/bin/sitewright"],
            "Cmd": ["serve-site", "/site"],
            "Env": ["SITEWRIGHT_DATA=/data", "PATH=/usr/local/bin"],
            "ExposedPorts": {"8080/tcp": {}},
            "Volumes": {"/data": {}},
            "User": "65532:65532",
            "WorkingDir": "/",
            "Labels": {"org.opencontainers.image.title": "hikes"},
        })
    );
    let history = config["history"].as_array().unwrap();
    assert_eq!(history.len(), 2);
    for entry in history {
        assert_eq!(entry["created"], "2023-11-14T22:13:20Z");
        assert!(entry["created_by"].as_str().unwrap().contains("sitewright"));
    }

    // Layer 1: the binary, with the parent directories nobody listed.
    let first = read_tar(files[layer_paths[0].as_str()]);
    assert_eq!(
        summary(&first),
        [
            ("usr/", true, 0o755, 0, 0),
            ("usr/local/", true, 0o755, 0, 0),
            ("usr/local/bin/", true, 0o755, 0, 0),
            ("usr/local/bin/sitewright", false, 0o755, 0, BINARY.len()),
        ]
    );
    assert_eq!(first[3].content, BINARY);

    // Layer 2: sorted by path, parents first, the data directory with its owner.
    let second = read_tar(files[layer_paths[1].as_str()]);
    assert_eq!(
        summary(&second),
        [
            ("data/", true, 0o700, 65532, 0),
            ("site/", true, 0o755, 0, 0),
            ("site/assets/", true, 0o755, 0, 0),
            ("site/assets/style.css", false, 0o644, 0, 6),
            ("site/index.html", false, 0o644, 0, 14),
            ("site/posts/", true, 0o755, 0, 0),
            ("site/posts/2026/", true, 0o755, 0, 0),
            ("site/posts/2026/first.html", false, 0o644, 0, 12),
        ]
    );
    assert_eq!(second[0].gid, 65532);
    assert_eq!(second[4].content, b"<h1>Hikes</h1>");
    for seen in first.iter().chain(&second) {
        assert_eq!(seen.mtime, CREATED as u64);
        assert_eq!((seen.user.as_str(), seen.group.as_str()), ("", ""));
        assert!(matches!(
            seen.kind,
            tar::EntryType::Directory | tar::EntryType::Regular
        ));
    }
}

#[test]
fn archive_is_also_a_valid_oci_layout() {
    let inputs = tempfile::tempdir().unwrap();
    let out = inputs.path().join("image.tar");
    let info = write_docker_archive(&spec(&binary_in(inputs.path())), &out).unwrap();
    let bytes = std::fs::read(&out).unwrap();
    let outer = read_tar(&bytes);

    // The order modern `docker save` uses: blobs first, then the three top-level files.
    let paths: Vec<&str> = outer.iter().map(|seen| seen.path.as_str()).collect();
    assert_eq!(paths[..2], ["blobs/", "blobs/sha256/"]);
    assert_eq!(
        paths[paths.len() - 3..],
        ["index.json", "manifest.json", "oci-layout"]
    );
    // Two layers, the config and the OCI manifest.
    assert_eq!(paths.len(), 2 + 4 + 3);

    let files: BTreeMap<&str, &[u8]> = outer
        .iter()
        .map(|seen| (seen.path.as_str(), seen.content.as_slice()))
        .collect();
    // Every blob is named by the hash of its content.
    for (path, content) in &files {
        if let Some(name) = path
            .strip_prefix("blobs/sha256/")
            .filter(|name| !name.is_empty())
        {
            assert_eq!(name, sha256_hex(content));
        }
    }
    assert_eq!(
        serde_json::from_slice::<Value>(files["oci-layout"]).unwrap(),
        json!({"imageLayoutVersion": "1.0.0"})
    );

    let index: Value = serde_json::from_slice(files["index.json"]).unwrap();
    assert_eq!(index["schemaVersion"], 2);
    assert_eq!(
        index["mediaType"],
        "application/vnd.oci.image.index.v1+json"
    );
    let descriptor = &index["manifests"][0];
    assert_eq!(index["manifests"].as_array().unwrap().len(), 1);
    assert_eq!(
        descriptor["annotations"],
        json!({
            "io.containerd.image.name": "docker.io/sitewright-site/hikes:abc12345",
            "org.opencontainers.image.ref.name": "abc12345",
        })
    );
    assert_eq!(
        descriptor["mediaType"],
        "application/vnd.oci.image.manifest.v1+json"
    );

    let blob = |descriptor: &Value| -> &[u8] {
        let hex = descriptor["digest"]
            .as_str()
            .unwrap()
            .strip_prefix("sha256:")
            .unwrap();
        let content = files[format!("blobs/sha256/{hex}").as_str()];
        assert_eq!(descriptor["size"], content.len() as u64);
        content
    };
    let manifest: Value = serde_json::from_slice(blob(descriptor)).unwrap();
    assert_eq!(manifest["schemaVersion"], 2);
    assert_eq!(
        manifest["mediaType"],
        "application/vnd.oci.image.manifest.v1+json"
    );
    assert_eq!(
        manifest["config"]["mediaType"],
        "application/vnd.oci.image.config.v1+json"
    );
    assert_eq!(manifest["config"]["digest"], info.image_id.as_str());
    let config: Value = serde_json::from_slice(blob(&manifest["config"])).unwrap();
    let layers = manifest["layers"].as_array().unwrap();
    assert_eq!(layers.len(), 2);
    for (layer, diff_id) in layers
        .iter()
        .zip(config["rootfs"]["diff_ids"].as_array().unwrap())
    {
        assert_eq!(layer["mediaType"], "application/vnd.oci.image.layer.v1.tar");
        // Layers are uncompressed, so the blob digest and the diff_id coincide.
        assert_eq!(&layer["digest"], diff_id);
        blob(layer);
    }
}

#[test]
fn same_spec_and_inputs_give_identical_bytes() {
    let inputs = tempfile::tempdir().unwrap();
    let binary = binary_in(inputs.path());
    let first_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();
    let first_out = first_dir.path().join("a.tar");
    let second_out = second_dir.path().join("b.tar");

    let first = write_docker_archive(&spec(&binary), &first_out).unwrap();
    // The order in which entries are listed does not matter either.
    let mut reordered = spec(&binary);
    reordered.layers[1].entries.reverse();
    let second = write_docker_archive(&reordered, &second_out).unwrap();

    assert_eq!(first, second);
    assert_eq!(
        std::fs::read(&first_out).unwrap(),
        std::fs::read(&second_out).unwrap()
    );

    // And a different input gives a different image.
    let mut changed = spec(&binary);
    changed.layers[1]
        .entries
        .push(file("site/new.html", b"new"));
    let third = write_docker_archive(&changed, &second_out).unwrap();
    assert_ne!(first.image_id, third.image_id);
}

#[test]
fn identical_layers_are_stored_once() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("image.tar");
    let mut spec = spec(Path::new("unused"));
    let layer = Layer {
        entries: vec![file("same.txt", b"same")],
    };
    spec.layers = vec![layer.clone(), layer];
    write_docker_archive(&spec, &out).unwrap();

    let bytes = std::fs::read(&out).unwrap();
    let outer = read_tar(&bytes);
    // One layer blob, the config and the OCI manifest.
    let blobs = outer
        .iter()
        .filter(|seen| seen.kind == tar::EntryType::Regular && seen.path.starts_with("blobs/"))
        .count();
    assert_eq!(blobs, 3);
    let manifest = outer
        .iter()
        .find(|seen| seen.path == "manifest.json")
        .unwrap();
    let manifest: Value = serde_json::from_slice(&manifest.content).unwrap();
    assert_eq!(manifest[0]["Layers"][0], manifest[0]["Layers"][1]);
    assert_eq!(manifest[0]["Layers"].as_array().unwrap().len(), 2);
}

#[test]
fn long_paths_survive_without_pax_headers() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("image.tar");
    let long_dir = format!("site/{}", "d".repeat(120));
    let long_file = format!("{long_dir}/{}.html", "f".repeat(150));
    let mut spec = spec(Path::new("unused"));
    spec.layers = vec![Layer {
        entries: vec![file(&long_file, b"deep")],
    }];
    write_docker_archive(&spec, &out).unwrap();

    let bytes = std::fs::read(&out).unwrap();
    let outer = read_tar(&bytes);
    let layer = outer
        .iter()
        .filter(|seen| seen.kind == tar::EntryType::Regular && seen.path.starts_with("blobs/"))
        .max_by_key(|seen| seen.content.len())
        .unwrap();
    // `read_tar` asserts that no entry carries PAX extensions.
    let entries = read_tar(&layer.content);
    assert_eq!(
        summary(&entries),
        [
            ("site/", true, 0o755, 0, 0),
            (format!("{long_dir}/").as_str(), true, 0o755, 0, 0),
            (long_file.as_str(), false, 0o644, 0, 4),
        ]
    );
    // The raw tar holds GNU long-name records and no PAX ones.
    let kinds: Vec<u8> = layer
        .content
        .chunks(512)
        .filter(|block| &block[257..263] == b"ustar ")
        .map(|block| block[156])
        .collect();
    assert_eq!(kinds, [b'5', b'L', b'5', b'L', b'0']);
}

#[test]
fn an_explicit_directory_overrides_an_implied_parent() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("image.tar");
    let mut spec = spec(Path::new("unused"));
    spec.layers = vec![Layer {
        entries: vec![
            file("data/db/seed.sql", b"-- seed"),
            Entry::Dir {
                path: "data".to_owned(),
                mode: 0o750,
                uid: 1000,
                gid: 1000,
            },
        ],
    }];
    write_docker_archive(&spec, &out).unwrap();
    let bytes = std::fs::read(&out).unwrap();
    let outer = read_tar(&bytes);
    let layer = outer
        .iter()
        .filter(|seen| seen.kind == tar::EntryType::Regular && seen.path.starts_with("blobs/"))
        .max_by_key(|seen| seen.content.len())
        .unwrap();
    assert_eq!(
        summary(&read_tar(&layer.content)),
        [
            ("data/", true, 0o750, 1000, 0),
            ("data/db/", true, 0o755, 0, 0),
            ("data/db/seed.sql", false, 0o644, 0, 7),
        ]
    );
}

/// Runs a build that must fail and checks that it left nothing in the output directory.
fn failure(spec: &ImageSpec) -> ImageError {
    let dir = tempfile::tempdir().unwrap();
    let error = write_docker_archive(spec, &dir.path().join("image.tar")).unwrap_err();
    assert_eq!(dir_entries(dir.path()), [] as [&str; 0], "{error}");
    error
}

fn with_entries(entries: Vec<Entry>) -> ImageSpec {
    let mut spec = spec(Path::new("unused"));
    spec.layers = vec![Layer { entries }];
    spec
}

#[test]
fn duplicate_paths_are_rejected() {
    let error = failure(&with_entries(vec![
        file("site/index.html", b"a"),
        file("site/index.html", b"b"),
    ]));
    assert!(matches!(error, ImageError::DuplicatePath(path) if path == "site/index.html"));

    let dir = |path: &str| Entry::Dir {
        path: path.to_owned(),
        mode: 0o755,
        uid: 0,
        gid: 0,
    };
    let error = failure(&with_entries(vec![dir("data"), dir("data")]));
    assert!(matches!(error, ImageError::DuplicatePath(_)));
    let error = failure(&with_entries(vec![dir("data"), file("data", b"x")]));
    assert!(matches!(error, ImageError::DuplicatePath(_)));

    // The same path in two different layers is fine: the upper one wins.
    let mut layered = spec(Path::new("unused"));
    layered.layers = vec![
        Layer {
            entries: vec![file("index.html", b"old")],
        },
        Layer {
            entries: vec![file("index.html", b"new")],
        },
    ];
    let out = tempfile::tempdir().unwrap();
    write_docker_archive(&layered, &out.path().join("image.tar")).unwrap();
}

#[test]
fn a_file_cannot_have_children() {
    for entries in [
        vec![file("site", b"x"), file("site/index.html", b"y")],
        vec![file("site/index.html", b"y"), file("site", b"x")],
    ] {
        let error = failure(&with_entries(entries));
        assert!(matches!(error, ImageError::NotADirectory(path) if path == "site"));
    }
}

#[test]
fn invalid_paths_are_rejected() {
    for path in [
        "",
        "/etc/passwd",
        "../escape",
        "site/../../escape",
        "site/..",
        "./site/index.html",
        "site/./index.html",
        "site//index.html",
        "site/",
        "site\\index.html",
        "site/nul\0byte",
    ] {
        let error = failure(&with_entries(vec![file(path, b"x")]));
        assert!(
            matches!(&error, ImageError::InvalidPath { path: bad, .. } if bad == path),
            "{path:?} gave {error:?}"
        );
        let error = failure(&with_entries(vec![Entry::Dir {
            path: path.to_owned(),
            mode: 0o755,
            uid: 0,
            gid: 0,
        }]));
        assert!(matches!(error, ImageError::InvalidPath { .. }));
    }
    // Dots inside a name are ordinary characters.
    let out = tempfile::tempdir().unwrap();
    let fine = with_entries(vec![file("site/.well-known/a..b", b"x")]);
    write_docker_archive(&fine, &out.path().join("image.tar")).unwrap();
}

#[test]
fn invalid_specs_are_rejected() {
    let base = || with_entries(vec![file("index.html", b"x")]);

    for tag in [
        "",
        "hikes",
        "Hikes:v1",
        "hikes:bad tag",
        "hikes:",
        "a//b:v1",
    ] {
        let mut spec = base();
        spec.repo_tag = tag.to_owned();
        let error = failure(&spec);
        assert!(
            matches!(&error, ImageError::InvalidTag { tag: bad, .. } if bad == tag),
            "{tag:?} gave {error:?}"
        );
    }

    for architecture in ["x86_64", "aarch64", "riscv64", "", "AMD64"] {
        let mut spec = base();
        spec.architecture = architecture.to_owned();
        assert!(matches!(
            failure(&spec),
            ImageError::UnsupportedArchitecture(bad) if bad == architecture
        ));
    }

    let mut spec = base();
    spec.layers.clear();
    assert!(matches!(failure(&spec), ImageError::NoLayers));

    let mut spec = base();
    spec.created_unix_secs = -1;
    assert!(matches!(failure(&spec), ImageError::InvalidTimestamp(-1)));

    let mut spec = base();
    spec.env.push(("BAD=NAME".to_owned(), "x".to_owned()));
    assert!(matches!(failure(&spec), ImageError::InvalidConfig(_)));

    let mut spec = base();
    spec.volumes.push("data".to_owned());
    assert!(matches!(failure(&spec), ImageError::InvalidConfig(_)));

    let mut spec = base();
    spec.layers[0].entries.push(Entry::Dir {
        path: "data".to_owned(),
        mode: 0o40755,
        uid: 0,
        gid: 0,
    });
    assert!(matches!(failure(&spec), ImageError::InvalidMode { .. }));
}

#[test]
fn arm64_and_an_empty_config_are_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("image.tar");
    let spec = ImageSpec {
        repo_tag: "hikes:latest".to_owned(),
        architecture: "arm64".to_owned(),
        layers: vec![Layer::default()],
        entrypoint: vec![],
        cmd: vec![],
        env: vec![],
        exposed_ports: vec![],
        volumes: vec![],
        user: String::new(),
        working_dir: String::new(),
        labels: vec![],
        created_unix_secs: 0,
    };
    let info = write_docker_archive(&spec, &out).unwrap();
    let bytes = std::fs::read(&out).unwrap();
    let outer = read_tar(&bytes);
    let config_path = format!("blobs/sha256/{}", &info.image_id["sha256:".len()..]);
    let config = outer.iter().find(|seen| seen.path == config_path).unwrap();
    let config: Value = serde_json::from_slice(&config.content).unwrap();
    assert_eq!(config["architecture"], "arm64");
    assert_eq!(config["created"], "1970-01-01T00:00:00Z");
    assert_eq!(config["config"], json!({"User": "", "WorkingDir": ""}));
    let index = outer.iter().find(|seen| seen.path == "index.json").unwrap();
    let index: Value = serde_json::from_slice(&index.content).unwrap();
    assert_eq!(
        index["manifests"][0]["annotations"]["io.containerd.image.name"],
        "docker.io/library/hikes:latest"
    );
}

#[test]
fn a_missing_source_file_fails_and_leaves_nothing_behind() {
    let inputs = tempfile::tempdir().unwrap();
    let missing = inputs.path().join("no-such-binary");
    // The first layer is fine, so a layer file already exists when the second one fails.
    let mut spec = spec(&missing);
    spec.layers.reverse();
    let error = failure(&spec);
    assert!(matches!(&error, ImageError::Source { path, .. } if path == &missing));
    assert!(error.to_string().contains("no-such-binary"));

    // A directory is not a file either.
    let mut spec = with_entries(vec![]);
    spec.layers[0].entries.push(Entry::File {
        path: "bin/sitewright".to_owned(),
        source: Source::Path(inputs.path().to_path_buf()),
        mode: 0o755,
        uid: 0,
        gid: 0,
    });
    assert!(matches!(failure(&spec), ImageError::Source { .. }));
}

#[test]
fn a_failed_build_keeps_the_previous_archive() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("image.tar");
    write_docker_archive(&with_entries(vec![file("index.html", b"x")]), &out).unwrap();
    let before = std::fs::read(&out).unwrap();

    let broken = spec(&dir.path().join("missing"));
    assert!(write_docker_archive(&broken, &out).is_err());
    assert_eq!(std::fs::read(&out).unwrap(), before);
    assert_eq!(dir_entries(dir.path()), ["image.tar"]);
}

#[test]
fn large_files_are_streamed_intact() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("big");
    let content: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&source, &content).unwrap();
    let out = dir.path().join("image.tar");
    let spec = with_entries(vec![Entry::File {
        path: "big.bin".to_owned(),
        source: Source::Path(source),
        mode: 0o600,
        uid: 7,
        gid: 8,
    }]);
    write_docker_archive(&spec, &out).unwrap();
    let bytes = std::fs::read(&out).unwrap();
    let outer = read_tar(&bytes);
    let layer = outer.iter().max_by_key(|seen| seen.content.len()).unwrap();
    let entries = read_tar(&layer.content);
    assert_eq!(entries.len(), 1);
    assert_eq!(
        (entries[0].uid, entries[0].gid, entries[0].mode),
        (7, 8, 0o600)
    );
    assert_eq!(sha256_hex(&entries[0].content), sha256_hex(&content));
}

#[test]
fn host_architecture_is_a_supported_name_or_nothing() {
    let expected = match std::env::consts::ARCH {
        "x86_64" => Some("amd64"),
        "aarch64" => Some("arm64"),
        _ => None,
    };
    assert_eq!(sw_image::host_architecture(), expected);
}

/// Loads a real archive into a local container engine. Needs `docker` or `podman`, so it
/// only runs on request: `cargo nextest run -p sw-image --run-ignored ignored-only`.
#[test]
#[ignore = "needs docker or podman on PATH"]
fn a_container_engine_loads_the_archive() {
    let engine = ["docker", "podman"]
        .into_iter()
        .find(|engine| {
            std::process::Command::new(engine)
                .arg("version")
                .output()
                .is_ok_and(|output| output.status.success())
        })
        .expect("docker or podman must be installed and running for this test");

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("image.tar");
    let mut spec = spec(&binary_in(dir.path()));
    spec.repo_tag = "sitewright-test/sw-image-load:test".to_owned();
    spec.architecture = sw_image::host_architecture().unwrap().to_owned();
    let info = write_docker_archive(&spec, &out).unwrap();

    let run = |args: &[&str]| {
        let output = std::process::Command::new(engine)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{engine} {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    run(&["load", "-i", out.to_str().unwrap()]);
    let inspected = run(&["image", "inspect", &spec.repo_tag]);
    run(&["image", "rm", &spec.repo_tag]);

    let inspected: Value = serde_json::from_str(&inspected).unwrap();
    assert_eq!(inspected[0]["Config"]["User"], "65532:65532");
    assert_eq!(inspected[0]["Architecture"], spec.architecture.as_str());
    assert_eq!(
        inspected[0]["RootFS"]["Layers"].as_array().unwrap().len(),
        2
    );
    if engine == "docker" {
        // With the classic image store the id is the config digest. The containerd store
        // reports the manifest digest instead, so only the form is checked there.
        let id = inspected[0]["Id"].as_str().unwrap();
        assert!(id == info.image_id || id.starts_with("sha256:"));
    }
}
