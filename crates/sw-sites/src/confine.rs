//! Path confinement: every file path from outside is checked here before it touches disk.
//!
//! The rules are deliberately stricter than "stays inside the directory": no hidden
//! files, no unusual characters, an extension allowlist. A site is plain static content,
//! so nothing legitimate is lost and whole classes of tricks are ruled out.

use std::path::{Path, PathBuf};

use crate::error::{Result, SiteError};

/// Longest accepted relative path, in bytes.
pub const MAX_PATH_BYTES: usize = 200;
/// Deepest accepted nesting (number of components).
pub const MAX_DEPTH: usize = 8;

/// Check a site-relative path such as `css/site.css` and return it as a relative
/// [`PathBuf`]. `allowed_extensions` are lowercase, without the dot.
pub fn clean_rel_path(input: &str, allowed_extensions: &[String]) -> Result<PathBuf> {
    let err = |reason: &str| Err(SiteError::path(input, reason));

    if input.is_empty() {
        return err("empty path");
    }
    if input.len() > MAX_PATH_BYTES {
        return err("path is too long");
    }
    if input.starts_with('/') {
        return err("absolute paths are not allowed; use a path relative to the site root");
    }
    if input.contains('\\') {
        return err("use forward slashes");
    }

    let mut clean = PathBuf::new();
    let mut depth = 0;
    for part in input.split('/') {
        depth += 1;
        if depth > MAX_DEPTH {
            return err("path is nested too deeply");
        }
        match part {
            "" => return err("empty path component"),
            "." | ".." => return err("'.' and '..' are not allowed"),
            _ => {}
        }
        if part.starts_with('.') {
            return err("hidden files and directories (including .git) are not allowed");
        }
        if !part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | '+'))
        {
            return err("use only letters, digits, '-', '_', '.', '@' and '+' in file names");
        }
        if part.ends_with('.') {
            return err("file names may not end with a dot");
        }
        clean.push(part);
    }

    let extension = clean
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match extension {
        Some(ext) if allowed_extensions.contains(&ext) => Ok(clean),
        Some(ext) => Err(SiteError::path(
            input,
            format!("files of type .{ext} are not allowed"),
        )),
        None => err("files need an extension, such as .html or .css"),
    }
}

/// The directory that holds a site's backend code.
pub const BACKEND_DIR: &str = "backend";
/// File types allowed under `backend/`.
const BACKEND_EXTENSIONS: &[&str] = &["js", "mjs", "json", "sql", "md"];

/// Whether a site-relative path is inside the backend directory.
pub fn is_backend_path(path: &str) -> bool {
    path == BACKEND_DIR
        || path
            .strip_prefix(BACKEND_DIR)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Check a site-relative path. Paths under `backend/` are code, not content: they take
/// their own file types, and SQL only as migrations.
pub fn clean_site_path(input: &str, allowed_extensions: &[String]) -> Result<PathBuf> {
    if !is_backend_path(input) {
        return clean_rel_path(input, allowed_extensions);
    }
    let allowed: Vec<String> = BACKEND_EXTENSIONS.iter().map(|e| (*e).to_owned()).collect();
    let clean = clean_rel_path(input, &allowed)?;
    let is_sql = input.to_ascii_lowercase().ends_with(".sql");
    let in_migrations = input.starts_with("backend/migrations/");
    if is_sql && !in_migrations {
        return Err(SiteError::path(
            input,
            "SQL files belong in backend/migrations/",
        ));
    }
    if in_migrations {
        let name = input.trim_start_matches("backend/migrations/");
        let (number, rest) = name.split_at(name.len().min(4));
        let stem = rest.strip_suffix(".sql").unwrap_or("");
        let valid = number.len() == 4
            && number.bytes().all(|b| b.is_ascii_digit())
            && stem.starts_with('_')
            && stem.len() > 1
            && stem
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !valid {
            return Err(SiteError::path(
                input,
                "migrations are named like backend/migrations/0001_create_entries.sql",
            ));
        }
    }
    Ok(clean)
}

/// The directory that holds files the owner uploaded from the portal. The agent may use
/// them but not change them.
pub const UPLOADS_DIR: &str = "uploads";
/// File types the owner may upload: media, fonts, documents and data. Never pages or
/// scripts, which the agent writes. Also subject to the configured allowlist.
pub const UPLOAD_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "avif", "svg", "ico", "woff", "woff2", "ttf", "otf",
    "pdf", "mp3", "mp4", "webm", "txt", "csv", "json", "md",
];
/// Longest file name an upload keeps, in bytes, extension included.
const MAX_UPLOAD_NAME: usize = 80;

/// Whether a site-relative path is inside the uploads directory.
pub fn is_upload_path(path: &str) -> bool {
    path.strip_prefix(UPLOADS_DIR)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Turn a file name from a person's device (`My Logo (final).PNG`) into the path the
/// upload gets in the site (`uploads/my-logo-final.png`). Rejects types that cannot be
/// uploaded.
pub fn upload_path(file_name: &str, allowed_extensions: &[String]) -> Result<String> {
    // Browsers send a bare name, but some include a path; keep only the last part.
    let base = file_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(file_name)
        .trim();
    let (stem, extension) = match base.rsplit_once('.') {
        Some((stem, ext)) if !ext.is_empty() => (stem, ext.to_ascii_lowercase()),
        _ => {
            return Err(SiteError::path(
                file_name,
                "the file needs an extension, such as .png or .pdf",
            ));
        }
    };
    if !UPLOAD_EXTENSIONS.contains(&extension.as_str()) || !allowed_extensions.contains(&extension)
    {
        let allowed: Vec<&str> = UPLOAD_EXTENSIONS
            .iter()
            .copied()
            .filter(|e| allowed_extensions.iter().any(|a| a == e))
            .collect();
        return Err(SiteError::path(
            file_name,
            format!(
                "files of type .{extension} cannot be uploaded; allowed: {}",
                allowed.join(", ")
            ),
        ));
    }

    let mut clean = String::with_capacity(stem.len());
    for c in stem.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() || c == '_' {
            clean.push(c);
        } else if !clean.ends_with('-') {
            clean.push('-');
        }
    }
    let budget = MAX_UPLOAD_NAME - extension.len() - 1;
    clean.truncate(budget);
    let clean = clean.trim_matches('-');
    let clean = if clean.is_empty() { "file" } else { clean };
    Ok(format!("{UPLOADS_DIR}/{clean}.{extension}"))
}

/// Join a cleaned relative path onto `root`, refusing to pass through a symlink.
///
/// `root` must exist. Components of `rel` that do not exist yet are fine; any existing
/// component that is a symlink is refused, so a link planted inside the tree can never
/// redirect a read or write outside it.
pub fn resolve_in(root: &Path, rel: &Path) -> Result<PathBuf> {
    let mut current = root.to_owned();
    for part in rel.components() {
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(SiteError::path(
                    &rel.to_string_lossy(),
                    "symbolic links are not allowed",
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(current)
}

/// A regular file found by [`walk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Walked {
    /// Path relative to the walked root, with forward slashes.
    pub rel: String,
    pub size: u64,
}

/// List the regular files under `root`, sorted by path. Hidden entries (names starting
/// with a dot, which covers `.git`) and symlinks are skipped; skipped symlinks are
/// reported in the second list.
pub fn walk(root: &Path) -> std::io::Result<(Vec<Walked>, Vec<String>)> {
    let mut files = Vec::new();
    let mut symlinks = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel_dir) = stack.pop() {
        for entry in std::fs::read_dir(root.join(&rel_dir))? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with('.') {
                continue;
            }
            let rel = rel_dir.join(name);
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                symlinks.push(rel_str);
            } else if file_type.is_dir() {
                stack.push(rel);
            } else if file_type.is_file() {
                files.push(Walked {
                    rel: rel_str,
                    size: entry.metadata()?.len(),
                });
            }
        }
    }
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    symlinks.sort();
    Ok((files, symlinks))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exts() -> Vec<String> {
        ["html", "css", "js", "png"].map(str::to_owned).to_vec()
    }

    #[test]
    fn accepts_ordinary_paths() {
        for path in [
            "index.html",
            "css/site.css",
            "a/b/c/d.js",
            "img/Photo_1@2x.PNG",
            "about.html",
        ] {
            assert!(clean_rel_path(path, &exts()).is_ok(), "{path}");
        }
    }

    #[test]
    fn rejects_escapes_and_oddities() {
        let cases = [
            "",
            "/etc/passwd",
            "../index.html",
            "a/../../b.html",
            "./index.html",
            "a//b.html",
            "a/",
            ".git/config",
            "a/.git/hooks/x.js",
            ".htaccess",
            "a\\b.html",
            "a b.html",
            "a\0.html",
            "caf\u{e9}.html",
            "index.php",
            "index",
            "x.html.",
            "a/b/c/d/e/f/g/h/i.html",
            "a:b.html",
            "~root.html",
            "%2e%2e/x.html",
        ];
        for path in cases {
            assert!(
                clean_rel_path(path, &exts()).is_err(),
                "{path:?} should be rejected"
            );
        }
        assert!(clean_rel_path(&format!("{}.html", "a".repeat(200)), &exts()).is_err());
    }

    #[test]
    fn upload_names_are_cleaned() {
        let allowed: Vec<String> = ["png", "jpg", "pdf", "html"].map(str::to_owned).to_vec();
        let ok = |name: &str| upload_path(name, &allowed).unwrap();
        assert_eq!(ok("My Logo (final).PNG"), "uploads/my-logo-final.png");
        assert_eq!(ok("C:\\Users\\me\\photo_1.jpg"), "uploads/photo_1.jpg");
        assert_eq!(ok("../../etc/x.pdf"), "uploads/x.pdf");
        assert_eq!(ok("Café menu.pdf"), "uploads/caf-menu.pdf");
        assert_eq!(ok("...png"), "uploads/file.png");
        assert_eq!(ok(".hidden.png"), "uploads/hidden.png");
        let long = ok(&format!("{}.png", "a".repeat(300)));
        assert_eq!(long.len(), "uploads/".len() + MAX_UPLOAD_NAME);

        // Pages and scripts are the agent's; unknown or unlisted types are refused.
        for name in ["index.html", "x.js", "noext", "x.", "photo.heic", "doc.svg"] {
            assert!(upload_path(name, &allowed).is_err(), "{name}");
        }
        // Every cleaned name passes the general path rules.
        for name in ["My Logo (final).PNG", "Café menu.pdf", "...png"] {
            assert!(clean_site_path(&ok(name), &allowed).is_ok(), "{name}");
        }
        assert!(is_upload_path("uploads/a.png"));
        assert!(!is_upload_path("uploadsx/a.png"));
        assert!(!is_upload_path("img/uploads/a.png"));
    }

    #[test]
    fn resolve_refuses_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        std::os::unix::fs::symlink(outside.join("f.html"), root.join("file.html")).unwrap();

        assert!(resolve_in(&root, Path::new("link/x.html")).is_err());
        assert!(resolve_in(&root, Path::new("file.html")).is_err());
        assert_eq!(
            resolve_in(&root, Path::new("new/x.html")).unwrap(),
            root.join("new/x.html")
        );
    }

    #[test]
    fn walk_skips_hidden_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("css")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("index.html"), "hi").unwrap();
        std::fs::write(root.join("css/site.css"), "body{}").unwrap();
        std::fs::write(root.join(".git/config"), "x").unwrap();
        std::fs::write(root.join(".hidden"), "x").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.join("pw.html")).unwrap();

        let (files, symlinks) = walk(root).unwrap();
        let names: Vec<_> = files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(names, ["css/site.css", "index.html"]);
        assert_eq!(symlinks, ["pw.html"]);
    }
}
