//! Site validation: the checks a working tree must pass before it can be published.
//!
//! Errors block publishing. Warnings are advice for whoever (or whatever) built the site.

use std::collections::BTreeSet;
use std::path::Path;

use scraper::{Html, Selector};
use serde::Serialize;
use sw_core::config::SitesConfig;

use crate::confine;

/// Largest total weight of one page: its HTML plus the local files it loads.
pub const PAGE_WEIGHT_BUDGET: u64 = 2 * 1024 * 1024;
/// Stop listing problems after this many; a broken site can produce hundreds.
const MAX_PROBLEMS: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

/// One finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub severity: Severity,
    /// The file the problem is in, or empty for site-wide problems.
    pub file: String,
    pub message: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.file.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.file, self.message)
        }
    }
}

/// The result of validating a site.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub files: usize,
    pub bytes: u64,
    pub problems: Vec<Problem>,
}

impl Report {
    /// Whether the site may be published.
    pub fn ok(&self) -> bool {
        !self.problems.iter().any(|p| p.severity == Severity::Error)
    }

    pub fn errors(&self) -> impl Iterator<Item = &Problem> {
        self.problems
            .iter()
            .filter(|p| p.severity == Severity::Error)
    }

    fn push(&mut self, severity: Severity, file: &str, message: impl Into<String>) {
        if self.problems.len() < MAX_PROBLEMS {
            self.problems.push(Problem {
                severity,
                file: file.to_owned(),
                message: message.into(),
            });
        }
    }
}

/// Validate the working tree at `root`. Blocking: call from `spawn_blocking`.
pub fn validate(root: &Path, limits: &SitesConfig) -> std::io::Result<Report> {
    let (files, symlinks) = confine::walk(root)?;
    let mut report = Report {
        files: files.len(),
        bytes: files.iter().map(|f| f.size).sum(),
        problems: Vec::new(),
    };
    let paths: BTreeSet<&str> = files.iter().map(|f| f.rel.as_str()).collect();

    if !paths.contains("index.html") {
        report.push(
            Severity::Error,
            "",
            "index.html is missing: every site needs one at its top level",
        );
    }
    for link in &symlinks {
        report.push(Severity::Error, link, "symbolic links are not allowed");
    }
    if report.bytes > limits.max_site_bytes {
        report.push(
            Severity::Error,
            "",
            format!(
                "the site is {} bytes; the limit is {} bytes",
                report.bytes, limits.max_site_bytes
            ),
        );
    }
    for file in &files {
        if file.size > limits.max_file_bytes {
            report.push(
                Severity::Error,
                &file.rel,
                format!(
                    "{} bytes; the limit is {} bytes per file",
                    file.size, limits.max_file_bytes
                ),
            );
        }
        if let Err(crate::SiteError::Path { reason, .. }) =
            confine::clean_rel_path(&file.rel, &limits.allowed_extensions)
        {
            report.push(Severity::Error, &file.rel, reason);
        }
    }

    let size_of = |rel: &str| files.iter().find(|f| f.rel == rel).map_or(0, |f| f.size);
    for file in files
        .iter()
        .filter(|f| f.rel.ends_with(".html") || f.rel.ends_with(".htm"))
    {
        let bytes = std::fs::read(root.join(&file.rel))?;
        let Ok(text) = String::from_utf8(bytes) else {
            report.push(Severity::Error, &file.rel, "not valid UTF-8");
            continue;
        };
        check_page(&mut report, &file.rel, &text, &paths, file.size, &size_of);
    }
    Ok(report)
}

fn check_page(
    report: &mut Report,
    file: &str,
    text: &str,
    paths: &BTreeSet<&str>,
    size: u64,
    size_of: &dyn Fn(&str) -> u64,
) {
    if text.trim().is_empty() {
        report.push(Severity::Error, file, "the file is empty");
        return;
    }
    let doc = Html::parse_document(text);
    let select = |css: &str| Selector::parse(css).ok();

    if !text
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("<!doctype html")
    {
        report.push(
            Severity::Warning,
            file,
            "missing <!doctype html> on the first line",
        );
    }
    let has = |css: &str| select(css).is_some_and(|s| doc.select(&s).next().is_some());
    let title = select("head > title")
        .and_then(|s| doc.select(&s).next())
        .map(|t| t.text().collect::<String>());
    if title.is_none_or(|t| t.trim().is_empty()) {
        report.push(Severity::Warning, file, "missing or empty <title>");
    }
    if !has("meta[name=viewport]") {
        report.push(
            Severity::Warning,
            file,
            "missing <meta name=\"viewport\">; the page will not fit phones",
        );
    }
    if !has("html[lang]") {
        report.push(Severity::Warning, file, "missing lang attribute on <html>");
    }
    if let Some(error) = doc.errors.iter().find(|e| is_serious_parse_error(e)) {
        report.push(
            Severity::Warning,
            file,
            format!("HTML parse problem: {error}"),
        );
    }

    // Every local link and asset must resolve; everything a page loads counts to its weight.
    let mut weight = size;
    let mut counted: BTreeSet<String> = BTreeSet::new();
    let references: &[(&str, &str, bool)] = &[
        ("a[href]", "href", false),
        ("link[href]", "href", true),
        ("script[src]", "src", true),
        ("img[src]", "src", true),
        ("source[src]", "src", true),
        ("video[src]", "src", true),
        ("audio[src]", "src", true),
        ("iframe[src]", "src", false),
    ];
    for (css, attribute, is_asset) in references {
        let Some(selector) = select(css) else {
            continue;
        };
        for element in doc.select(&selector) {
            let Some(raw) = element.value().attr(attribute) else {
                continue;
            };
            match classify(raw) {
                Reference::External => {
                    if *is_asset {
                        report.push(
                            Severity::Warning,
                            file,
                            format!("loads {raw} from another server; keep assets in the site unless the user asked for this"),
                        );
                    }
                }
                Reference::Skip => {}
                Reference::Local(target) => match resolve(file, &target, paths) {
                    Some(found) => {
                        if *is_asset && counted.insert(found.clone()) {
                            weight += size_of(&found);
                        }
                    }
                    None => report.push(
                        Severity::Error,
                        file,
                        format!("{attribute}=\"{raw}\" points to a file that does not exist"),
                    ),
                },
            }
        }
    }
    if weight > PAGE_WEIGHT_BUDGET {
        report.push(
            Severity::Error,
            file,
            format!("the page and its assets weigh {weight} bytes; the budget is {PAGE_WEIGHT_BUDGET} bytes"),
        );
    }
}

/// html5ever reports many harmless recoveries; only structural ones are worth surfacing.
fn is_serious_parse_error(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    e.contains("unexpected") && (e.contains("eof") || e.contains("end of file"))
        || e.contains("unclosed")
}

enum Reference {
    /// Another origin.
    External,
    /// Nothing to check: fragments, mailto:, data:, daemon-served paths.
    Skip,
    /// A path inside the site, without query or fragment.
    Local(String),
}

fn classify(raw: &str) -> Reference {
    let value = raw.trim();
    if value.is_empty() || value.starts_with('#') {
        return Reference::Skip;
    }
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("//") {
        return Reference::External;
    }
    // Any other scheme (mailto:, tel:, data:, javascript:, sms:, ...).
    if let Some((scheme, _)) = lower.split_once(':')
        && !scheme.is_empty()
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return Reference::Skip;
    }
    // Served by the daemon itself (analytics beacon), not part of the site's files.
    if value.starts_with("/_sw/") {
        return Reference::Skip;
    }
    let path = value.split(['?', '#']).next().unwrap_or("");
    if path.is_empty() {
        return Reference::Skip;
    }
    Reference::Local(path.to_owned())
}

/// Resolve a local reference the way the server does: the file itself, a directory's
/// `index.html`, or `<path>.html`. Returns the site-relative file it lands on.
fn resolve(from_file: &str, target: &str, paths: &BTreeSet<&str>) -> Option<String> {
    let mut parts: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        let mut dir: Vec<&str> = from_file.split('/').collect();
        dir.pop();
        dir
    };
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    let directory_index = if joined.is_empty() {
        "index.html".to_owned()
    } else {
        format!("{joined}/index.html")
    };
    if target.ends_with('/') || joined.is_empty() {
        return paths
            .contains(directory_index.as_str())
            .then_some(directory_index);
    }
    [joined.clone(), directory_index, format!("{joined}.html")]
        .into_iter()
        .find(|candidate| paths.contains(candidate.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
        <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>T</title>\
        <link rel=\"stylesheet\" href=\"css/site.css\"></head><body>\
        <a href=\"about\">About</a> <a href=\"/trails/\">Trails</a> <a href=\"trails/rainier.html#top\">R</a>\
        <a href=\"https://example.org\">ext</a> <a href=\"mailto:a@b.c\">mail</a> <a href=\"#x\">frag</a>\
        <img src=\"/img/a.png?v=2\" alt=\"\"><script src=\"/_sw/a.js\" defer></script></body></html>";

    fn site(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, content) in files {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, content).unwrap();
        }
        dir
    }

    fn messages(report: &Report, severity: Severity) -> Vec<String> {
        report
            .problems
            .iter()
            .filter(|p| p.severity == severity)
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn a_well_formed_site_passes_cleanly() {
        let dir = site(&[
            ("index.html", GOOD),
            ("about.html", GOOD),
            (
                "trails/index.html",
                "<!doctype html><html lang=\"en\"><head><meta name=\"viewport\" content=\"w\"><title>T</title></head><body><a href=\"../\">home</a><a href=\"rainier.html\">r</a></body></html>",
            ),
            (
                "trails/rainier.html",
                "<!doctype html><html lang=\"en\"><head><meta name=\"viewport\" content=\"w\"><title>T</title></head><body><a href=\"../about.html\">a</a></body></html>",
            ),
            ("css/site.css", "body{}"),
            ("img/a.png", "png"),
        ]);
        let report = validate(dir.path(), &SitesConfig::default()).unwrap();
        assert!(report.ok());
        assert_eq!(report.problems, [], "{:?}", report.problems);
        assert_eq!(report.files, 6);
    }

    #[test]
    fn missing_index_and_broken_references_are_errors() {
        let dir = site(&[(
            "home.html",
            "<!doctype html><html lang=\"en\"><head><meta name=\"viewport\" content=\"w\"><title>T</title></head><body>\
             <a href=\"nope.html\">x</a><img src=\"img/missing.png\"><a href=\"../../etc/passwd\">up</a><link href=\"/gone.css\" rel=\"stylesheet\"></body></html>",
        )]);
        let report = validate(dir.path(), &SitesConfig::default()).unwrap();
        assert!(!report.ok());
        let errors = messages(&report, Severity::Error);
        assert_eq!(errors.len(), 5, "{errors:?}");
        assert!(errors[0].contains("index.html is missing"));
        assert!(errors.iter().any(|e| e.contains("nope.html")));
        assert!(errors.iter().any(|e| e.contains("img/missing.png")));
        assert!(errors.iter().any(|e| e.contains("../../etc/passwd")));
        assert!(errors.iter().any(|e| e.contains("/gone.css")));
    }

    #[test]
    fn quality_issues_are_warnings_that_do_not_block() {
        let dir = site(&[(
            "index.html",
            "<html><head></head><body><script src=\"https://cdn.example.com/lib.js\"></script><a href=\"https://example.org\">fine</a></body></html>",
        )]);
        let report = validate(dir.path(), &SitesConfig::default()).unwrap();
        assert!(report.ok());
        let warnings = messages(&report, Severity::Warning);
        for needle in ["doctype", "<title>", "viewport", "lang", "cdn.example.com"] {
            assert!(
                warnings.iter().any(|w| w.contains(needle)),
                "{needle}: {warnings:?}"
            );
        }
        assert!(
            !warnings.iter().any(|w| w.contains("example.org")),
            "plain links out are fine"
        );
    }

    #[test]
    fn size_limits_and_page_weight_are_enforced() {
        let limits = SitesConfig {
            max_file_bytes: 50,
            max_site_bytes: 100,
            ..SitesConfig::default()
        };
        let dir = site(&[
            ("index.html", &"x".repeat(60)),
            ("big.css", &"y".repeat(60)),
            ("empty.html", " "),
        ]);
        let report = validate(dir.path(), &limits).unwrap();
        let errors = messages(&report, Severity::Error);
        assert!(
            errors.iter().any(|e| e.contains("the site is 121 bytes")),
            "{errors:?}"
        );
        assert!(errors.iter().any(|e| e.starts_with("big.css: 60 bytes")));
        assert!(
            errors
                .iter()
                .any(|e| e.starts_with("empty.html: the file is empty"))
        );

        let heavy = "z".repeat(900 * 1024);
        let page = "<!doctype html><html lang=\"en\"><head><meta name=\"viewport\" content=\"w\"><title>T</title></head><body><img src=\"a.png\"><img src=\"b.png\"><img src=\"c.png\"><img src=\"a.png\"></body></html>";
        let dir = site(&[
            ("index.html", page),
            ("a.png", &heavy),
            ("b.png", &heavy),
            ("c.png", &heavy),
        ]);
        let report = validate(dir.path(), &SitesConfig::default()).unwrap();
        let errors = messages(&report, Severity::Error);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("budget"));
    }

    #[test]
    fn stray_files_and_bad_encodings_are_errors() {
        let dir = site(&[
            ("index.html", GOOD),
            ("about.html", GOOD),
            ("trails/index.html", GOOD),
            ("trails/rainier.html", GOOD),
            ("css/site.css", ""),
            ("img/a.png", ""),
            ("run.sh", "x"),
        ]);
        std::fs::write(dir.path().join("latin.html"), [0xff, 0xfe, 0x00]).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join("pw.html")).unwrap();
        let report = validate(dir.path(), &SitesConfig::default()).unwrap();
        let errors = messages(&report, Severity::Error);
        assert!(
            errors.iter().any(|e| e.starts_with("run.sh:")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.starts_with("latin.html: not valid UTF-8"))
        );
        assert!(
            errors
                .iter()
                .any(|e| e.starts_with("pw.html: symbolic links"))
        );
    }
}
