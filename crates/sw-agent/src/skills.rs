//! Skills: instructions the model loads only when a task needs them.
//!
//! A skill is a folder in the [Agent Skills](https://agentskills.io) format: a `SKILL.md`
//! with YAML frontmatter (`name`, `description`) followed by Markdown instructions, plus
//! optional bundled files. The system prompt carries only the index (names and
//! descriptions); `load_skill` returns a skill's body and `read_skill_file` one of its
//! bundled files.
//!
//! Built-in skills are compiled into the binary from the repository's `skills/`
//! directory. Skills in `<data>/skills/` are read on every run, so dropping one in takes
//! effect at once, and a user skill replaces a built-in of the same name.

use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(rust_embed::RustEmbed)]
#[folder = "../../skills"]
struct Builtin;

/// Largest skill file handed to the model, in bytes.
const MAX_FILE_BYTES: usize = 200 * 1024;

/// One entry of the skills index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    /// True for skills from the data directory.
    pub user: bool,
}

/// Access to built-in and user skills. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Skills {
    user_dir: Option<PathBuf>,
}

impl Skills {
    /// `user_dir` is `<data>/skills`; `None` uses built-in skills only.
    pub fn new(user_dir: Option<PathBuf>) -> Self {
        Self { user_dir }
    }

    /// Every available skill, sorted by name. Invalid user skills are skipped (and
    /// logged) rather than breaking a run.
    pub fn index(&self) -> Vec<SkillMeta> {
        let mut skills: Vec<SkillMeta> = Vec::new();
        for name in self.user_names() {
            match self
                .user_file(&name, "SKILL.md")
                .and_then(|text| parse(&name, &text).map(|(d, _)| d))
            {
                Ok(description) => skills.push(SkillMeta {
                    name,
                    description,
                    user: true,
                }),
                Err(reason) => {
                    tracing::warn!(skill = %name, %reason, "ignoring an invalid user skill")
                }
            }
        }
        for name in builtin_names() {
            if skills.iter().any(|s| s.name == name) {
                continue; // overridden by a user skill
            }
            if let Some(Ok((description, _))) =
                builtin_file(&name, "SKILL.md").map(|text| parse(&name, &text))
            {
                skills.push(SkillMeta {
                    name,
                    description,
                    user: false,
                });
            }
        }
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    /// The instructions of a skill (the body of its `SKILL.md`) and its bundled files.
    pub fn load(&self, name: &str) -> Result<(String, Vec<String>), String> {
        check_name(name)?;
        let user = self.user_names().iter().any(|n| n == name);
        let text = if user {
            self.user_file(name, "SKILL.md")?
        } else {
            builtin_file(name, "SKILL.md").ok_or_else(|| self.unknown(name))?
        };
        let (_, body) = parse(name, &text)?;
        let files = if user {
            self.user_files(name)
        } else {
            builtin_files(name)
        };
        Ok((body, files))
    }

    /// One bundled file of a skill, for example `references/checklist.md`.
    pub fn read_file(&self, name: &str, path: &str) -> Result<String, String> {
        check_name(name)?;
        check_path(path)?;
        if self.user_names().iter().any(|n| n == name) {
            self.user_file(name, path)
        } else if builtin_file(name, "SKILL.md").is_some() {
            builtin_file(name, path).ok_or_else(|| format!("skill {name:?} has no file {path:?}"))
        } else {
            Err(self.unknown(name))
        }
    }

    fn unknown(&self, name: &str) -> String {
        let names: Vec<String> = self.index().into_iter().map(|s| s.name).collect();
        format!(
            "there is no skill named {name:?}; available skills: {}",
            names.join(", ")
        )
    }

    /// Names of the directories in the user skills directory that hold a `SKILL.md`.
    fn user_names(&self) -> Vec<String> {
        let Some(dir) = &self.user_dir else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| check_name(name).is_ok() && dir.join(name).join("SKILL.md").is_file())
            .collect();
        names.sort();
        names
    }

    fn user_file(&self, name: &str, path: &str) -> Result<String, String> {
        let dir = self
            .user_dir
            .as_ref()
            .ok_or("no user skills directory")?
            .join(name);
        let full = dir.join(path);
        // The path was checked component by component; refuse symlinks so a skill
        // cannot point outside its own folder.
        let mut current = dir.clone();
        for part in Path::new(path).components() {
            current.push(part);
            if std::fs::symlink_metadata(&current).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(format!(
                    "{path:?} is a symbolic link, which skills may not use"
                ));
            }
        }
        let bytes =
            std::fs::read(&full).map_err(|_| format!("skill {name:?} has no file {path:?}"))?;
        text_file(bytes, path)
    }

    fn user_files(&self, name: &str) -> Vec<String> {
        let Some(dir) = self.user_dir.as_ref().map(|d| d.join(name)) else {
            return Vec::new();
        };
        let mut files = Vec::new();
        let mut stack = vec![PathBuf::new()];
        while let Some(rel) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(dir.join(&rel)) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let Ok(file_name) = entry.file_name().into_string() else {
                    continue;
                };
                let child = rel.join(&file_name);
                match entry.file_type() {
                    Ok(t) if t.is_dir() => stack.push(child),
                    Ok(t) if t.is_file() && file_name != "SKILL.md" => {
                        files.push(child.to_string_lossy().replace('\\', "/"))
                    }
                    _ => {}
                }
            }
        }
        files.retain(|f| check_path(f).is_ok());
        files.sort();
        files
    }
}

fn builtin_names() -> Vec<String> {
    let mut names: Vec<String> = Builtin::iter()
        .filter_map(|path| path.strip_suffix("/SKILL.md").map(str::to_owned))
        .collect();
    names.sort();
    names
}

fn builtin_file(name: &str, path: &str) -> Option<String> {
    let file = Builtin::get(&format!("{name}/{path}"))?;
    String::from_utf8(file.data.into_owned()).ok()
}

fn builtin_files(name: &str) -> Vec<String> {
    let prefix = format!("{name}/");
    let mut files: Vec<String> = Builtin::iter()
        .filter_map(|p| p.strip_prefix(&prefix).map(str::to_owned))
        .filter(|p| p != "SKILL.md")
        .collect();
    files.sort();
    files
}

fn text_file(bytes: Vec<u8>, path: &str) -> Result<String, String> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(format!(
            "{path:?} is too large to read ({} bytes)",
            bytes.len()
        ));
    }
    String::from_utf8(bytes).map_err(|_| format!("{path:?} is not a text file"))
}

/// Skill names: 1 to 64 lowercase letters, digits and single hyphens.
fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--");
    if ok {
        Ok(())
    } else {
        Err(format!("{name:?} is not a valid skill name"))
    }
}

/// Paths inside a skill: relative, forward slashes, no hidden or parent components.
fn check_path(path: &str) -> Result<(), String> {
    let ok = !path.is_empty()
        && path.len() <= 200
        && !path.starts_with('/')
        && !path.contains('\\')
        && path.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        });
    if ok {
        Ok(())
    } else {
        Err(format!("{path:?} is not a valid path inside a skill"))
    }
}

/// Parse a `SKILL.md`: returns the description and the Markdown body. The frontmatter
/// must name the skill after its folder.
pub fn parse(folder: &str, text: &str) -> Result<(String, String), String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = text
        .strip_prefix("---")
        .ok_or("SKILL.md must start with a --- frontmatter block")?;
    let (frontmatter, body) = rest
        .split_once("\n---")
        .ok_or("the frontmatter block is not closed with ---")?;
    let body = body.trim_start_matches('-').trim().to_owned();

    let mut name = None;
    let mut description = None;
    let mut lines = frontmatter.lines().peekable();
    while let Some(line) = lines.next() {
        // Only top-level keys matter; nested ones (metadata) are indented.
        if line.starts_with(' ')
            || line.starts_with('\t')
            || line.trim().is_empty()
            || line.trim_start().starts_with('#')
        {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let mut value = value.trim().to_owned();
        if matches!(value.as_str(), ">" | "|" | ">-" | "|-" | ">+" | "|+" | "") {
            // A block scalar: the indented lines that follow.
            let mut parts = Vec::new();
            while let Some(next) = lines.peek() {
                if next.starts_with(' ') || next.starts_with('\t') || next.trim().is_empty() {
                    parts.push(next.trim().to_owned());
                    lines.next();
                } else {
                    break;
                }
            }
            value = parts.join(" ").trim().to_owned();
        } else if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = value[1..value.len() - 1]
                .replace("\\\"", "\"")
                .replace("''", "'");
        }
        match key.trim() {
            "name" => name = Some(value),
            "description" => description = Some(value),
            _ => {}
        }
    }
    let name = name.ok_or("the frontmatter has no name")?;
    check_name(&name)?;
    if name != folder {
        return Err(format!(
            "the skill is named {name:?} but lives in the folder {folder:?}"
        ));
    }
    let description = description
        .filter(|d| !d.is_empty())
        .ok_or("the frontmatter has no description")?;
    if description.chars().count() > 1024 {
        return Err("the description is longer than 1024 characters".into());
    }
    if body.is_empty() {
        return Err("the skill has no instructions".into());
    }
    Ok((description, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, text: &str) {
        let full = dir.join(rel);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, text).unwrap();
    }

    #[test]
    fn parses_frontmatter_in_its_common_shapes() {
        let (description, body) = parse("demo", "---\nname: demo\ndescription: Does a thing. Use when asked.\n---\n\n# Demo\n\nSteps.\n").unwrap();
        assert_eq!(description, "Does a thing. Use when asked.");
        assert_eq!(body, "# Demo\n\nSteps.");

        let folded = "---\nname: demo\ndescription: >\n  First line\n  second line.\nlicense: MIT\nmetadata:\n  name: not-this\n---\nBody";
        assert_eq!(parse("demo", folded).unwrap().0, "First line second line.");
        let quoted = "---\nname: \"demo\"\ndescription: 'It''s quoted: with a colon'\n---\nBody";
        assert_eq!(
            parse("demo", quoted).unwrap().0,
            "It's quoted: with a colon"
        );
    }

    #[test]
    fn rejects_malformed_skills() {
        for (text, why) in [
            ("# No frontmatter", "frontmatter"),
            ("---\nname: demo\ndescription: x\n", "not closed"),
            ("---\ndescription: x\n---\nBody", "no name"),
            ("---\nname: demo\n---\nBody", "no description"),
            ("---\nname: other\ndescription: x\n---\nBody", "folder"),
            (
                "---\nname: Demo\ndescription: x\n---\nBody",
                "valid skill name",
            ),
            ("---\nname: demo\ndescription: x\n---\n", "no instructions"),
        ] {
            let err = parse("demo", text).unwrap_err();
            assert!(err.contains(why), "{text:?}: {err}");
        }
        assert!(
            parse(
                "demo",
                &format!(
                    "---\nname: demo\ndescription: {}\n---\nBody",
                    "x".repeat(1025)
                )
            )
            .is_err()
        );
    }

    #[test]
    fn built_in_skills_are_embedded_and_valid() {
        let skills = Skills::new(None);
        let index = skills.index();
        let names: Vec<_> = index.iter().map(|s| s.name.as_str()).collect();
        for expected in ["create-site", "delete-site", "edit-site", "setup-domain"] {
            assert!(
                names.contains(&expected),
                "{expected} missing from {names:?}"
            );
        }
        for skill in &index {
            assert!(!skill.user);
            assert!(
                skill.description.len() > 40,
                "{}: the description should say what and when",
                skill.name
            );
            let (body, files) = skills.load(&skill.name).unwrap();
            assert!(body.len() > 200, "{}", skill.name);
            for file in files {
                assert!(
                    !skills.read_file(&skill.name, &file).unwrap().is_empty(),
                    "{}/{file}",
                    skill.name
                );
            }
        }
        assert!(
            skills
                .load("nope")
                .unwrap_err()
                .contains("available skills: build-backend, create-site")
        );
        assert!(skills.read_file("create-site", "missing.md").is_err());
    }

    #[test]
    fn user_skills_are_listed_override_built_ins_and_stay_in_their_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "house-style/SKILL.md",
            "---\nname: house-style\ndescription: Our colours and tone. Use for every site.\n---\nUse teal.",
        );
        write(root, "house-style/references/palette.md", "teal: #0aa");
        write(
            root,
            "edit-site/SKILL.md",
            "---\nname: edit-site\ndescription: Overridden editing rules.\n---\nAlways ask first.",
        );
        write(root, "broken/SKILL.md", "no frontmatter here");
        write(
            root,
            "Bad Name/SKILL.md",
            "---\nname: bad\ndescription: x\n---\nBody",
        );
        write(root, "secret.md", "outside any skill");
        std::os::unix::fs::symlink(root.join("secret.md"), root.join("house-style/link.md"))
            .unwrap();

        let skills = Skills::new(Some(root.to_owned()));
        let index = skills.index();
        let house = index.iter().find(|s| s.name == "house-style").unwrap();
        assert!(house.user);
        let edit = index.iter().find(|s| s.name == "edit-site").unwrap();
        assert!(edit.user && edit.description == "Overridden editing rules.");
        assert!(!index.iter().any(|s| s.name == "broken" || s.name == "bad"));
        assert!(index.iter().any(|s| s.name == "create-site" && !s.user));

        let (body, files) = skills.load("house-style").unwrap();
        assert_eq!(body, "Use teal.");
        assert_eq!(files, ["references/palette.md"], "symlinks are not listed");
        assert_eq!(
            skills
                .read_file("house-style", "references/palette.md")
                .unwrap(),
            "teal: #0aa"
        );
        assert_eq!(skills.load("edit-site").unwrap().0, "Always ask first.");

        for bad in [
            "../secret.md",
            "/etc/passwd",
            "references/../../secret.md",
            ".hidden",
            "a\\b.md",
            "",
        ] {
            assert!(skills.read_file("house-style", bad).is_err(), "{bad}");
        }
        assert!(
            skills
                .read_file("house-style", "link.md")
                .unwrap_err()
                .contains("symbolic link")
        );
        assert!(skills.read_file("../house-style", "SKILL.md").is_err());

        // A skill dropped in later is picked up without restarting anything.
        write(
            root,
            "late/SKILL.md",
            "---\nname: late\ndescription: Added while running.\n---\nHi.",
        );
        assert!(skills.index().iter().any(|s| s.name == "late"));
    }
}
