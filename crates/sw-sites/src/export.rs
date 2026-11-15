//! Exporting a published site as an archive: the plain files, or a Docker build context.
//!
//! Both formats are built by ordinary code from the live release. No model is involved
//! and nothing here needs Docker on the server.

use std::path::{Path, PathBuf};

use flate2::write::GzEncoder;
use serde::Serialize;

use crate::confine;
use crate::error::{Result, SiteError};

/// The static file server image used by the `context` export, pinned by digest so a
/// build today and a build next year produce the same thing.
///
/// To bump it: pick a release of https://hub.docker.com/r/joseluisq/static-web-server,
/// copy the multi-arch index digest shown for that tag (or run
/// `docker buildx imagetools inspect joseluisq/static-web-server:<tag>`), update both
/// constants and run the tests.
pub const SERVER_IMAGE: &str = "joseluisq/static-web-server:2.44.2";
pub const SERVER_IMAGE_DIGEST: &str =
    "sha256:23be6c04d474979d6e738081918f5874f88502a150d1123fd000719101a0f05c";

/// Exports older than this are deleted.
const KEEP_EXPORTS_MS: i64 = 24 * 3600 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    /// A `.tar.gz` of the site's files, ready for any static host.
    Folder,
    /// A `.tar.gz` with the files, a `Dockerfile` and a README: a Docker build context.
    Context,
}

impl ExportFormat {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "folder" => Ok(Self::Folder),
            "context" => Ok(Self::Context),
            other => Err(SiteError::Invalid(format!(
                "unknown export format {other:?}; use \"folder\" or \"context\""
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Folder => "folder",
            Self::Context => "context",
        }
    }
}

/// A finished export.
#[derive(Debug, Clone, Serialize)]
pub struct Export {
    pub format: ExportFormat,
    pub file_name: String,
    #[serde(skip)]
    pub path: PathBuf,
    pub bytes: u64,
    pub files: usize,
    /// The release that was exported.
    pub sha: String,
}

/// Build the archive for `release_dir` in `exports_dir`. Blocking.
///
/// The archive is deterministic: entries are sorted and carry fixed timestamps, owners
/// and modes, so exporting the same release twice gives identical bytes.
pub fn build(
    site: &str,
    sha: &str,
    release_dir: &Path,
    exports_dir: &Path,
    format: ExportFormat,
) -> Result<Export> {
    std::fs::create_dir_all(exports_dir)?;
    clean(exports_dir);
    let (files, _) = confine::walk(release_dir)?;
    if files.is_empty() {
        return Err(SiteError::Invalid(format!(
            "site {site} has no files to export"
        )));
    }
    let short = &sha[..sha.len().min(8)];
    let file_name = match format {
        ExportFormat::Folder => format!("{site}-{short}.tar.gz"),
        ExportFormat::Context => format!("{site}-{short}-docker-context.tar.gz"),
    };
    let path = exports_dir.join(&file_name);
    let tmp = exports_dir.join(format!(".{file_name}.tmp"));

    let mut tar = tar::Builder::new(GzEncoder::new(
        std::fs::File::create(&tmp)?,
        flate2::Compression::default(),
    ));
    let mut add = |name: String, data: &[u8]| -> std::io::Result<()> {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_cksum();
        tar.append_data(&mut header, name, data)
    };
    let site_prefix = match format {
        ExportFormat::Folder => format!("{site}/"),
        ExportFormat::Context => format!("{site}/site/"),
    };
    if format == ExportFormat::Context {
        add(format!("{site}/Dockerfile"), dockerfile().as_bytes())?;
        add(format!("{site}/README.md"), readme(site, short).as_bytes())?;
    }
    for file in &files {
        add(
            format!("{site_prefix}{}", file.rel),
            &std::fs::read(release_dir.join(&file.rel))?,
        )?;
    }
    drop(add);
    tar.into_inner()?.finish()?.sync_all()?;
    std::fs::rename(&tmp, &path)?;
    let bytes = std::fs::metadata(&path)?.len();
    Ok(Export {
        format,
        file_name,
        path,
        bytes,
        files: files.len(),
        sha: sha.to_owned(),
    })
}

/// Delete exports older than a day. Failures are ignored: this is housekeeping.
pub fn clean(exports_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(exports_dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.filter_map(std::result::Result::ok) {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age.as_millis() as i64 > KEEP_EXPORTS_MS);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn dockerfile() -> String {
    format!(
        "# A static file server (about 4 MB, no shell) with the site baked in.\n\
         # The base image is pinned by digest: this build is reproducible.\n\
         FROM {SERVER_IMAGE}@{SERVER_IMAGE_DIGEST}\n\
         COPY site/ /public/\n\
         ENV SERVER_ROOT=/public \\\n    SERVER_PORT=8080 \\\n    SERVER_ERROR_PAGE_404=/public/404.html\n\
         EXPOSE 8080\n"
    )
}

fn readme(site: &str, short: &str) -> String {
    format!(
        "# {site}\n\n\
         A Docker build context for the site `{site}` (release `{short}`), exported from Sitewright.\n\n\
         ## Build and run\n\n\
         ```sh\n\
         docker build -t {site} .\n\
         docker run --rm -p 8080:8080 {site}\n\
         ```\n\n\
         Then open http://localhost:8080.\n\n\
         ## What is inside\n\n\
         - `site/`: the site's files, exactly as published.\n\
         - `Dockerfile`: copies them into `{SERVER_IMAGE}`, a small static file server,\n  pinned by digest.\n\n\
         The image serves plain HTTP on port 8080. Put it behind whatever terminates TLS in\n\
         your environment.\n\n\
         The analytics line `<script src=\"/_sw/a.js\">` in the pages refers to a script that\n\
         only a Sitewright host serves. Elsewhere it is a harmless 404; remove the line if you\n\
         prefer.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn release() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("css")).unwrap();
        std::fs::write(dir.path().join("index.html"), "<h1>hi</h1>").unwrap();
        std::fs::write(dir.path().join("css/site.css"), "body{}").unwrap();
        std::fs::write(dir.path().join(".hidden"), "x").unwrap();
        dir
    }

    fn entries(path: &Path) -> Vec<(String, String)> {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(
            std::fs::File::open(path).unwrap(),
        ));
        archive
            .entries()
            .unwrap()
            .map(|e| {
                let mut e = e.unwrap();
                let mut text = String::new();
                e.read_to_string(&mut text).unwrap();
                (e.path().unwrap().to_string_lossy().into_owned(), text)
            })
            .collect()
    }

    #[test]
    fn folder_export_holds_the_site_files_and_is_deterministic() {
        let (release, out) = (release(), tempfile::tempdir().unwrap());
        let export = build(
            "hikes",
            "abcdef0123456789",
            release.path(),
            out.path(),
            ExportFormat::Folder,
        )
        .unwrap();
        assert_eq!(export.file_name, "hikes-abcdef01.tar.gz");
        assert_eq!(export.files, 2);
        let names: Vec<String> = entries(&export.path).into_iter().map(|e| e.0).collect();
        assert_eq!(names, ["hikes/css/site.css", "hikes/index.html"]);

        let first = std::fs::read(&export.path).unwrap();
        let again = build(
            "hikes",
            "abcdef0123456789",
            release.path(),
            out.path(),
            ExportFormat::Folder,
        )
        .unwrap();
        assert_eq!(
            first,
            std::fs::read(&again.path).unwrap(),
            "same release, same bytes"
        );
    }

    #[test]
    fn context_export_adds_a_digest_pinned_dockerfile_and_a_readme() {
        let (release, out) = (release(), tempfile::tempdir().unwrap());
        let export = build(
            "hikes",
            "abcdef0123456789",
            release.path(),
            out.path(),
            ExportFormat::Context,
        )
        .unwrap();
        assert_eq!(export.file_name, "hikes-abcdef01-docker-context.tar.gz");
        let files = entries(&export.path);
        let names: Vec<&str> = files.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(
            names,
            [
                "hikes/Dockerfile",
                "hikes/README.md",
                "hikes/site/css/site.css",
                "hikes/site/index.html"
            ]
        );
        let dockerfile = &files[0].1;
        let from = dockerfile.lines().find(|l| l.starts_with("FROM ")).unwrap();
        assert!(
            from.contains("@sha256:") && from.split("@sha256:").nth(1).unwrap().len() == 64,
            "{from}"
        );
        assert!(dockerfile.contains("COPY site/ /public/") && dockerfile.contains("EXPOSE 8080"));
        assert!(
            files[1].1.contains("docker build -t hikes .")
                && files[1].1.contains("docker run --rm -p 8080:8080 hikes")
        );
    }

    #[test]
    fn formats_parse_and_empty_sites_are_refused() {
        assert_eq!(ExportFormat::parse("folder").unwrap(), ExportFormat::Folder);
        assert_eq!(
            ExportFormat::parse("context").unwrap(),
            ExportFormat::Context
        );
        assert!(ExportFormat::parse("zip").is_err());
        let (empty, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        assert!(build("s", "abc", empty.path(), out.path(), ExportFormat::Folder).is_err());
    }
}
