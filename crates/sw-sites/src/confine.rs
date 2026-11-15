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
