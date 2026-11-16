//! Writes a container image as a `docker load`-compatible archive, without Docker.
//!
//! Sitewright can package one website together with its own static binary into a
//! `FROM scratch`-style image. The server that does this usually has no Docker daemon (and
//! Sitewright must not need one), so this crate writes the image archive directly: the
//! same tar format `docker save` produces, which `docker load` and `podman load` read.
//!
//! # Archive format
//!
//! The archive is an uncompressed tar with the layout modern `docker save` (Docker 25 and
//! later) uses. It is at once a Docker archive and an OCI image layout:
//!
//! ```text
//! blobs/
//! blobs/sha256/
//! blobs/sha256/<hex>     one per layer (an uncompressed tar), the image config (JSON)
//!                        and the OCI image manifest (JSON), each named by its SHA-256
//! index.json             OCI index pointing at the OCI manifest
//! manifest.json          [{"Config": "blobs/sha256/<hex>", "RepoTags": ["name:tag"],
//!                          "Layers": ["blobs/sha256/<hex>", ...]}]
//! oci-layout             {"imageLayoutVersion": "1.0.0"}
//! ```
//!
//! Loaders that predate the OCI layout read only `manifest.json` and follow the paths in
//! it, whatever they are; containerd-based loaders use `oci-layout` and `index.json`.
//! Every blob is stored once and serves both.
//!
//! The image id is the SHA-256 of the config JSON. Each `rootfs.diff_ids` entry is the
//! SHA-256 of the corresponding uncompressed layer tar.
//!
//! # Determinism
//!
//! The same [`ImageSpec`] and the same file contents give a byte-identical archive: layer
//! entries are sorted by path, every timestamp is [`ImageSpec::created_unix_secs`], and no
//! host user names, clocks or random values reach the output.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

/// The last second of the year 9999, the largest instant RFC 3339 can express.
const MAX_CREATED_SECS: i64 = 253_402_300_799;

const MEDIA_TYPE_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const MEDIA_TYPE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const MEDIA_TYPE_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const MEDIA_TYPE_LAYER: &str = "application/vnd.oci.image.layer.v1.tar";

/// Where the content of a file in the image comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A file on disk. It is streamed into the layer, never read into memory whole.
    Path(PathBuf),
    /// Content held in memory.
    Bytes(Vec<u8>),
}

/// One file-system entry of a layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A regular file at `path` inside the image (no leading slash, forward slashes).
    File {
        path: String,
        source: Source,
        mode: u32,
        uid: u32,
        gid: u32,
    },
    /// A directory (use for empty directories and to set ownership, e.g. a writable /data).
    Dir {
        path: String,
        mode: u32,
        uid: u32,
        gid: u32,
    },
}

/// One image layer. Parent directories that are not listed are added automatically
/// (mode 0755, owned by root).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layer {
    pub entries: Vec<Entry>,
}

/// Everything that defines an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSpec {
    /// `name:tag`, e.g. `sitewright-site/hikes:abc12345`. The tag is required.
    pub repo_tag: String,
    /// `amd64` or `arm64`.
    pub architecture: String,
    /// Bottom layer first. At least one.
    pub layers: Vec<Layer>,
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub env: Vec<(String, String)>,
    /// TCP ports, written as `"8080/tcp"`.
    pub exposed_ports: Vec<u16>,
    /// Absolute paths inside the image, e.g. `/data`.
    pub volumes: Vec<String>,
    /// E.g. `65532:65532`. Empty means root.
    pub user: String,
    pub working_dir: String,
    pub labels: Vec<(String, String)>,
    /// Used for the config's `created` and for every tar modification time.
    pub created_unix_secs: i64,
}

/// What [`write_docker_archive`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageInfo {
    /// `sha256:<hex>` of the image config: the id `docker images` shows after a load.
    pub image_id: String,
    /// Size of the archive in bytes.
    pub bytes: u64,
}

/// Why an image could not be written.
#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("i/o error while writing the image archive: {0}")]
    Io(#[from] io::Error),
    #[error("cannot read source file {}: {source}", path.display())]
    Source {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot encode image metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid path {path:?} in an image layer: {reason}")]
    InvalidPath { path: String, reason: &'static str },
    #[error("path {0:?} appears more than once in a layer")]
    DuplicatePath(String),
    #[error("path {0:?} is a file, but the layer places other entries beneath it")]
    NotADirectory(String),
    #[error("invalid mode {mode:o} for {path:?}: only permission bits (up to 7777) are allowed")]
    InvalidMode { path: String, mode: u32 },
    #[error("invalid image tag {tag:?}: {reason}")]
    InvalidTag { tag: String, reason: &'static str },
    #[error("unsupported architecture {0:?}: expected \"amd64\" or \"arm64\"")]
    UnsupportedArchitecture(String),
    #[error("an image needs at least one layer")]
    NoLayers,
    #[error("creation time {0} is outside the years 1970 to 9999")]
    InvalidTimestamp(i64),
    #[error("invalid image configuration: {0}")]
    InvalidConfig(String),
    #[error("output path {} has no file name", .0.display())]
    InvalidOutputPath(PathBuf),
}

/// The Docker architecture name of the machine this program was compiled for, if it is
/// one this crate supports.
pub fn host_architecture() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Some("amd64"),
        "aarch64" => Some("arm64"),
        _ => None,
    }
}

/// Writes `spec` to `out` as a `docker load`-compatible archive.
///
/// The archive is built under a temporary name in the directory of `out` and renamed into
/// place at the end, so a failed build leaves nothing behind and never a partial file. The
/// result is readable by its owner only, because a site can hold private content.
pub fn write_docker_archive(spec: &ImageSpec, out: &Path) -> Result<ImageInfo, ImageError> {
    let reference = parse_repo_tag(&spec.repo_tag)?;
    if !matches!(spec.architecture.as_str(), "amd64" | "arm64") {
        return Err(ImageError::UnsupportedArchitecture(
            spec.architecture.clone(),
        ));
    }
    if spec.layers.is_empty() {
        return Err(ImageError::NoLayers);
    }
    let mtime = u64::try_from(spec.created_unix_secs)
        .ok()
        .filter(|_| spec.created_unix_secs <= MAX_CREATED_SECS)
        .ok_or(ImageError::InvalidTimestamp(spec.created_unix_secs))?;
    let created = rfc3339_utc(spec.created_unix_secs);
    let container_config = container_config(spec)?;
    // Check every layer before touching the disk.
    let plans = spec
        .layers
        .iter()
        .map(plan_layer)
        .collect::<Result<Vec<_>, _>>()?;

    if out.file_name().is_none() {
        return Err(ImageError::InvalidOutputPath(out.to_path_buf()));
    }
    let dir = match out.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };

    // Layers go to temporary files first: their digests are part of the config, which
    // has to be known before the outer archive can be written. The files delete
    // themselves when dropped, on success and on every error path.
    let layers = plans
        .iter()
        .map(|plan| write_layer(plan, dir, mtime))
        .collect::<Result<Vec<_>, _>>()?;

    let config = serde_json::to_vec(&ConfigFile {
        architecture: &spec.architecture,
        config: container_config,
        created: &created,
        history: (1..=layers.len())
            .map(|number| History {
                created: &created,
                created_by: format!("sitewright image layer {number}"),
            })
            .collect(),
        os: "linux",
        rootfs: RootFs {
            kind: "layers",
            diff_ids: layers.iter().map(|layer| digest(&layer.hex)).collect(),
        },
    })?;
    let config_hex = hex::encode(Sha256::digest(&config));

    let oci_manifest = serde_json::to_vec(&OciManifest {
        schema_version: 2,
        media_type: MEDIA_TYPE_MANIFEST,
        config: Descriptor {
            media_type: MEDIA_TYPE_CONFIG,
            digest: digest(&config_hex),
            size: config.len() as u64,
            annotations: BTreeMap::new(),
        },
        layers: layers
            .iter()
            .map(|layer| Descriptor {
                media_type: MEDIA_TYPE_LAYER,
                digest: digest(&layer.hex),
                size: layer.size,
                annotations: BTreeMap::new(),
            })
            .collect(),
    })?;
    let oci_manifest_hex = hex::encode(Sha256::digest(&oci_manifest));

    let index = serde_json::to_vec(&OciIndex {
        schema_version: 2,
        media_type: MEDIA_TYPE_INDEX,
        manifests: vec![Descriptor {
            media_type: MEDIA_TYPE_MANIFEST,
            digest: digest(&oci_manifest_hex),
            size: oci_manifest.len() as u64,
            annotations: BTreeMap::from([
                ("io.containerd.image.name", reference.canonical.clone()),
                ("org.opencontainers.image.ref.name", reference.tag.clone()),
            ]),
        }],
    })?;

    let docker_manifest = serde_json::to_vec(&[DockerManifest {
        config: blob_path(&config_hex),
        repo_tags: [&spec.repo_tag],
        layers: layers.iter().map(|layer| blob_path(&layer.hex)).collect(),
    }])?;

    // Sorted by digest, and stored once even if two layers are identical.
    let mut blobs: BTreeMap<&str, Blob<'_>> = BTreeMap::new();
    for layer in &layers {
        blobs.insert(&layer.hex, Blob::Layer(layer));
    }
    blobs.insert(&config_hex, Blob::Bytes(&config));
    blobs.insert(&oci_manifest_hex, Blob::Bytes(&oci_manifest));

    let archive = tempfile::Builder::new()
        .prefix(".sw-image-")
        .suffix(".tmp")
        .tempfile_in(dir)?;
    let mut builder = tar::Builder::new(BufWriter::new(archive.as_file()));
    for path in ["blobs/", "blobs/sha256/"] {
        let mut header = header(tar::EntryType::Directory, 0o755, 0, 0, mtime, 0);
        builder.append_data(&mut header, path, io::empty())?;
    }
    for (hex, blob) in &blobs {
        let path = blob_path(hex);
        match blob {
            Blob::Bytes(bytes) => append_bytes(&mut builder, &path, bytes, mtime)?,
            Blob::Layer(layer) => {
                let mut header = header(tar::EntryType::Regular, 0o644, 0, 0, mtime, layer.size);
                let reader = ExactReader::new(layer.file.reopen()?, layer.size);
                builder.append_data(&mut header, &path, reader)?;
            }
        }
    }
    append_bytes(&mut builder, "index.json", &index, mtime)?;
    append_bytes(&mut builder, "manifest.json", &docker_manifest, mtime)?;
    append_bytes(
        &mut builder,
        "oci-layout",
        br#"{"imageLayoutVersion":"1.0.0"}"#,
        mtime,
    )?;
    builder
        .into_inner()?
        .into_inner()
        .map_err(io::IntoInnerError::into_error)?;
    archive.as_file().sync_all()?;
    let bytes = archive.as_file().metadata()?.len();
    archive.persist(out).map_err(|error| error.error)?;

    Ok(ImageInfo {
        image_id: digest(&config_hex),
        bytes,
    })
}

// ---------------------------------------------------------------------------------------
// Layers
// ---------------------------------------------------------------------------------------

/// A layer entry after validation.
#[derive(Debug)]
enum Node<'a> {
    Dir {
        mode: u32,
        uid: u32,
        gid: u32,
    },
    File {
        source: &'a Source,
        mode: u32,
        uid: u32,
        gid: u32,
    },
}

/// Validates a layer and returns its entries, parent directories included, keyed by path.
///
/// The map's byte-wise order is the order in the tar. A directory always sorts before
/// what it contains, because its path is a prefix of theirs.
fn plan_layer(layer: &Layer) -> Result<BTreeMap<&str, Node<'_>>, ImageError> {
    let mut nodes: BTreeMap<&str, Node<'_>> = BTreeMap::new();
    let mut listed = BTreeSet::new();
    for entry in &layer.entries {
        let (path, node) = match entry {
            Entry::File {
                path,
                source,
                mode,
                uid,
                gid,
            } => (
                path.as_str(),
                Node::File {
                    source,
                    mode: *mode,
                    uid: *uid,
                    gid: *gid,
                },
            ),
            Entry::Dir {
                path,
                mode,
                uid,
                gid,
            } => (
                path.as_str(),
                Node::Dir {
                    mode: *mode,
                    uid: *uid,
                    gid: *gid,
                },
            ),
        };
        validate_path(path)?;
        let (Node::File { mode, .. } | Node::Dir { mode, .. }) = &node;
        if *mode > 0o7777 {
            return Err(ImageError::InvalidMode {
                path: path.to_owned(),
                mode: *mode,
            });
        }
        if !listed.insert(path) {
            return Err(ImageError::DuplicatePath(path.to_owned()));
        }
        // Anything already here was added as the parent of an earlier entry. A listed
        // directory replaces it (that is how ownership is set); a file cannot.
        if nodes.contains_key(path) && matches!(node, Node::File { .. }) {
            return Err(ImageError::NotADirectory(path.to_owned()));
        }
        nodes.insert(path, node);
        for (index, _) in path.match_indices('/') {
            let parent = &path[..index];
            match nodes.get(parent) {
                Some(Node::File { .. }) => {
                    return Err(ImageError::NotADirectory(parent.to_owned()));
                }
                Some(Node::Dir { .. }) => {}
                None => {
                    nodes.insert(
                        parent,
                        Node::Dir {
                            mode: 0o755,
                            uid: 0,
                            gid: 0,
                        },
                    );
                }
            }
        }
    }
    Ok(nodes)
}

fn validate_path(path: &str) -> Result<(), ImageError> {
    let invalid = |reason| {
        Err(ImageError::InvalidPath {
            path: path.to_owned(),
            reason,
        })
    };
    if path.is_empty() {
        return invalid("it is empty");
    }
    if path.starts_with('/') {
        return invalid("it must be relative to the image root, without a leading slash");
    }
    if path.contains('\\') {
        return invalid("it contains a backslash; use forward slashes");
    }
    if path.contains('\0') {
        return invalid("it contains a NUL byte");
    }
    for component in path.split('/') {
        match component {
            "" => return invalid("it has an empty component (a doubled or trailing slash)"),
            "." => return invalid("it has a `.` component"),
            ".." => return invalid("it has a `..` component"),
            _ => {}
        }
    }
    Ok(())
}

/// A finished layer tar in a temporary file.
#[derive(Debug)]
struct LayerFile {
    file: NamedTempFile,
    /// SHA-256 of the tar, lower-case hex.
    hex: String,
    size: u64,
}

fn write_layer(
    plan: &BTreeMap<&str, Node<'_>>,
    dir: &Path,
    mtime: u64,
) -> Result<LayerFile, ImageError> {
    let file = tempfile::Builder::new()
        .prefix(".sw-image-layer-")
        .suffix(".tmp")
        .tempfile_in(dir)?;
    let mut builder = tar::Builder::new(HashingWriter::new(BufWriter::new(file.as_file())));
    for (path, node) in plan {
        match node {
            Node::Dir { mode, uid, gid } => {
                let mut header = header(tar::EntryType::Directory, *mode, *uid, *gid, mtime, 0);
                builder.append_data(&mut header, format!("{path}/"), io::empty())?;
            }
            Node::File {
                source: Source::Bytes(bytes),
                mode,
                uid,
                gid,
            } => {
                let size = bytes.len() as u64;
                let mut header = header(tar::EntryType::Regular, *mode, *uid, *gid, mtime, size);
                builder.append_data(&mut header, path, bytes.as_slice())?;
            }
            Node::File {
                source: Source::Path(source_path),
                mode,
                uid,
                gid,
            } => {
                let source_error = |source| ImageError::Source {
                    path: source_path.clone(),
                    source,
                };
                let source_file = File::open(source_path).map_err(source_error)?;
                let metadata = source_file.metadata().map_err(source_error)?;
                if !metadata.is_file() {
                    return Err(source_error(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "not a regular file",
                    )));
                }
                let size = metadata.len();
                let mut header = header(tar::EntryType::Regular, *mode, *uid, *gid, mtime, size);
                // The header already promises `size` bytes; a file that changes while it
                // is being copied must not corrupt the tar.
                let mut reader = ExactReader::new(source_file, size);
                let appended = builder.append_data(&mut header, path, &mut reader);
                if let Some(source) = reader.failure.take() {
                    return Err(source_error(source));
                }
                appended?;
            }
        }
    }
    let HashingWriter {
        inner,
        hasher,
        bytes,
    } = builder.into_inner()?;
    inner.into_inner().map_err(io::IntoInnerError::into_error)?;
    Ok(LayerFile {
        file,
        hex: hex::encode(hasher.finalize()),
        size: bytes,
    })
}

/// A tar header with exactly the given fields: no user or group names, no device numbers.
fn header(
    kind: tar::EntryType,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: u64,
    size: u64,
) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(kind);
    header.set_mode(mode);
    header.set_uid(u64::from(uid));
    header.set_gid(u64::from(gid));
    header.set_mtime(mtime);
    header.set_size(size);
    header
}

fn append_bytes<W: Write>(
    builder: &mut tar::Builder<W>,
    path: &str,
    bytes: &[u8],
    mtime: u64,
) -> io::Result<()> {
    let mut header = header(
        tar::EntryType::Regular,
        0o644,
        0,
        0,
        mtime,
        bytes.len() as u64,
    );
    builder.append_data(&mut header, path, bytes)
}

/// Passes writes through while hashing and counting them.
struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    bytes: u64,
}

impl<W> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Yields exactly `remaining` bytes of `inner`: stops there if `inner` has more, and
/// fails if it has fewer. Remembers a failure in `failure` so the caller can tell errors
/// of the source from errors of the destination.
struct ExactReader<R> {
    inner: R,
    remaining: u64,
    failure: Option<io::Error>,
}

impl<R> ExactReader<R> {
    fn new(inner: R, remaining: u64) -> Self {
        Self {
            inner,
            remaining,
            failure: None,
        }
    }

    fn fail(&mut self, error: io::Error) -> io::Error {
        let copy = io::Error::new(error.kind(), error.to_string());
        self.failure = Some(error);
        copy
    }
}

impl<R: Read> Read for ExactReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || buf.is_empty() {
            return Ok(0);
        }
        let limit = usize::try_from(self.remaining)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        match self.inner.read(&mut buf[..limit]) {
            Ok(0) => Err(self.fail(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the file shrank while it was being read",
            ))),
            Ok(read) => {
                self.remaining -= read as u64;
                Ok(read)
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => Err(error),
            Err(error) => Err(self.fail(error)),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Image name
// ---------------------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct Reference {
    /// Fully qualified, the way containerd names images: `docker.io/library/name:tag`.
    canonical: String,
    tag: String,
}

/// Validates `name:tag` by the rules of Docker's reference grammar and works out the
/// fully qualified name.
fn parse_repo_tag(repo_tag: &str) -> Result<Reference, ImageError> {
    let invalid = |reason| {
        Err(ImageError::InvalidTag {
            tag: repo_tag.to_owned(),
            reason,
        })
    };
    if repo_tag.contains('@') {
        return invalid("digests are not supported; use name:tag");
    }
    // A colon before the last slash belongs to a registry port, not to a tag.
    let name_end = repo_tag.rfind('/').map_or(0, |slash| slash + 1);
    let Some(colon) = repo_tag[name_end..].rfind(':').map(|at| name_end + at) else {
        return invalid("the tag is missing; use name:tag");
    };
    let (name, tag) = (&repo_tag[..colon], &repo_tag[colon + 1..]);

    let tag_start_ok = tag
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    let tag_rest_ok = tag
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'));
    if !tag_start_ok || !tag_rest_ok || tag.len() > 128 {
        return invalid(
            "the tag must be 1 to 128 letters, digits, `_`, `.` or `-`, not starting with `.` or `-`",
        );
    }

    if name.is_empty() {
        return invalid("the name is empty");
    }
    if name.len() > 255 {
        return invalid("the name is longer than 255 characters");
    }
    // Docker's rule: the first component is a registry host only if it looks like one.
    let (domain, remainder) = match name.split_once('/') {
        Some((first, rest))
            if first.contains(['.', ':'])
                || first == "localhost"
                || first.bytes().any(|byte| byte.is_ascii_uppercase()) =>
        {
            (Some(first), rest)
        }
        _ => (None, name),
    };
    if let Some(domain) = domain {
        let (host, port) = match domain.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (domain, None),
        };
        let host_ok = !host.is_empty()
            && host.split('.').all(|label| {
                !label.is_empty()
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            });
        let port_ok =
            port.is_none_or(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()));
        if !host_ok || !port_ok {
            return invalid("the registry host is not valid");
        }
    }
    if !remainder.split('/').all(valid_name_component) {
        return invalid(
            "each part of the name must be lower-case letters and digits, separated by `.`, `_`, `__` or dashes",
        );
    }

    let canonical = match domain {
        Some("docker.io") | None if !remainder.contains('/') => {
            format!("docker.io/library/{remainder}:{tag}")
        }
        Some("docker.io") | None => format!("docker.io/{remainder}:{tag}"),
        Some(domain) => format!("{domain}/{remainder}:{tag}"),
    };
    Ok(Reference {
        canonical,
        tag: tag.to_owned(),
    })
}

/// `[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*`
fn valid_name_component(component: &str) -> bool {
    let alphanumeric = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let bytes = component.as_bytes();
    let (Some(&first), Some(&last)) = (bytes.first(), bytes.last()) else {
        return false;
    };
    if !alphanumeric(first) || !alphanumeric(last) {
        return false;
    }
    bytes
        .split(|&byte| alphanumeric(byte))
        .filter(|separator| !separator.is_empty())
        .all(|separator| {
            matches!(separator, b"." | b"_" | b"__") || separator.iter().all(|&byte| byte == b'-')
        })
}

// ---------------------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------------------

fn digest(hex: &str) -> String {
    format!("sha256:{hex}")
}

fn blob_path(hex: &str) -> String {
    format!("blobs/sha256/{hex}")
}

/// A blob of the outer archive.
enum Blob<'a> {
    Bytes(&'a [u8]),
    Layer(&'a LayerFile),
}

/// Docker's `{}` set-member value, as in `"ExposedPorts": {"8080/tcp": {}}`.
#[derive(Serialize)]
struct Empty {}

// The JSON below comes from structs and `BTreeMap`s rather than `serde_json::Value`, so
// that key order never depends on which serde_json features some other crate enables.

/// The image config. It is both a Docker image config and an OCI one.
#[derive(Serialize)]
struct ConfigFile<'a> {
    architecture: &'a str,
    config: ContainerConfig<'a>,
    created: &'a str,
    history: Vec<History<'a>>,
    os: &'static str,
    rootfs: RootFs,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ContainerConfig<'a> {
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    cmd: &'a [String],
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    entrypoint: &'a [String],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    env: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    exposed_ports: BTreeMap<String, Empty>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    labels: BTreeMap<&'a str, &'a str>,
    user: &'a str,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    volumes: BTreeMap<&'a str, Empty>,
    working_dir: &'a str,
}

#[derive(Serialize)]
struct History<'a> {
    created: &'a str,
    created_by: String,
}

#[derive(Serialize)]
struct RootFs {
    #[serde(rename = "type")]
    kind: &'static str,
    diff_ids: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    media_type: &'static str,
    digest: String,
    size: u64,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    annotations: BTreeMap<&'static str, String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OciManifest {
    schema_version: u32,
    media_type: &'static str,
    config: Descriptor,
    layers: Vec<Descriptor>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OciIndex {
    schema_version: u32,
    media_type: &'static str,
    manifests: Vec<Descriptor>,
}

/// One element of `manifest.json`.
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct DockerManifest<'a> {
    config: String,
    repo_tags: [&'a str; 1],
    layers: Vec<String>,
}

fn container_config(spec: &ImageSpec) -> Result<ContainerConfig<'_>, ImageError> {
    let invalid = |message: String| Err(ImageError::InvalidConfig(message));
    let mut env = Vec::with_capacity(spec.env.len());
    for (name, value) in &spec.env {
        if name.is_empty() || name.contains(['=', '\0']) || value.contains('\0') {
            return invalid(format!("environment variable name {name:?} is not valid"));
        }
        env.push(format!("{name}={value}"));
    }
    if spec.exposed_ports.contains(&0) {
        return invalid("port 0 cannot be exposed".to_owned());
    }
    if let Some(volume) = spec.volumes.iter().find(|path| !path.starts_with('/')) {
        return invalid(format!("volume {volume:?} is not an absolute path"));
    }
    if let Some((name, _)) = spec.labels.iter().find(|(name, _)| name.is_empty()) {
        return invalid(format!("label name {name:?} is empty"));
    }
    Ok(ContainerConfig {
        cmd: &spec.cmd,
        entrypoint: &spec.entrypoint,
        env,
        exposed_ports: spec
            .exposed_ports
            .iter()
            .map(|port| (format!("{port}/tcp"), Empty {}))
            .collect(),
        labels: spec
            .labels
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect(),
        user: &spec.user,
        volumes: spec
            .volumes
            .iter()
            .map(|path| (path.as_str(), Empty {}))
            .collect(),
        working_dir: &spec.working_dir,
    })
}

/// Formats seconds since the Unix epoch as RFC 3339 in UTC, e.g. `2026-11-16T09:30:00Z`.
///
/// The date part is the well-known "civil from days" conversion for the proleptic
/// Gregorian calendar; it saves a date-time dependency for one line of output.
fn rfc3339_utc(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let second_of_day = unix_secs.rem_euclid(86_400);
    // Count days from 0000-03-01, so that a leap day is the last day of its year.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3_600,
        second_of_day % 3_600 / 60,
        second_of_day % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_timestamps_as_rfc3339() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(951_782_399), "2000-02-28T23:59:59Z");
        assert_eq!(rfc3339_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_utc(951_868_800), "2000-03-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339_utc(4_107_542_399), "2100-02-28T23:59:59Z");
        assert_eq!(rfc3339_utc(4_107_542_400), "2100-03-01T00:00:00Z");
        assert_eq!(rfc3339_utc(MAX_CREATED_SECS), "9999-12-31T23:59:59Z");
    }

    #[test]
    fn qualifies_image_names_the_way_docker_does() {
        let canonical = |tag: &str| parse_repo_tag(tag).unwrap().canonical;
        assert_eq!(canonical("hikes:v1"), "docker.io/library/hikes:v1");
        assert_eq!(
            canonical("sitewright-site/hikes:abc12345"),
            "docker.io/sitewright-site/hikes:abc12345"
        );
        assert_eq!(
            canonical("docker.io/hikes:v1"),
            "docker.io/library/hikes:v1"
        );
        assert_eq!(
            canonical("registry.example.com:5000/a/b:v1"),
            "registry.example.com:5000/a/b:v1"
        );
        assert_eq!(canonical("localhost/hikes:v1"), "localhost/hikes:v1");
        assert_eq!(parse_repo_tag("a/b:v1").unwrap().tag, "v1");
    }

    #[test]
    fn rejects_names_docker_would_reject() {
        for tag in [
            "",
            "hikes",
            ":v1",
            "hikes:",
            "Hikes:v1",
            "hikes:v 1",
            "hikes:-v1",
            "hikes:.v1",
            "hi kes:v1",
            "/hikes:v1",
            "a//b:v1",
            "a/:v1",
            "a_-b:v1",
            "a___b:v1",
            "-a:v1",
            "a.:v1",
            "registry.example.com:5000/hikes",
            "registry.example.com:port/hikes:v1",
            "bad_host.example.com/hikes:v1",
            "hikes@sha256:0000:v1",
            "héllo:v1",
        ] {
            assert!(
                matches!(parse_repo_tag(tag), Err(ImageError::InvalidTag { .. })),
                "{tag:?} should be rejected"
            );
        }
        let long_tag = format!("hikes:{}", "t".repeat(129));
        assert!(parse_repo_tag(&long_tag).is_err());
        for tag in ["a--b:v1", "a__b:v1", "a.b/c_d:V_1.0-rc", "0:0"] {
            assert!(parse_repo_tag(tag).is_ok(), "{tag:?} should be accepted");
        }
    }

    #[test]
    fn exact_reader_stops_at_the_promised_size_and_reports_a_short_source() {
        let mut exact = ExactReader::new(&b"abcdef"[..], 4);
        let mut read = Vec::new();
        exact.read_to_end(&mut read).unwrap();
        assert_eq!(read, b"abcd");

        let mut short = ExactReader::new(&b"ab"[..], 4);
        let error = short.read_to_end(&mut Vec::new()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert!(short.failure.is_some());
    }
}
