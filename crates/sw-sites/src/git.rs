//! Git plumbing for site repositories, on libgit2. All functions block; call them from
//! `spawn_blocking`.

use std::path::Path;

use git2::{
    IndexAddOption, ObjectType, Oid, Repository, RepositoryInitOptions, Signature, TreeWalkMode,
    TreeWalkResult,
};

use crate::error::{Result, SiteError};

const BRANCH: &str = "main";

/// Create an empty repository with `main` as its initial branch.
pub fn init(repo_dir: &Path) -> Result<()> {
    let mut options = RepositoryInitOptions::new();
    options.initial_head(BRANCH).mkpath(true);
    Repository::init_opts(repo_dir, &options)?;
    Ok(())
}

/// The result of [`commit_all`].
#[derive(Debug)]
pub struct Committed {
    pub sha: String,
    /// False when the working tree matched `HEAD` and no commit was made.
    pub changed: bool,
}

/// Stage everything in the working tree (additions, changes and deletions) and commit it.
/// When nothing changed since `HEAD`, no commit is made and `HEAD`'s sha is returned.
pub fn commit_all(repo_dir: &Path, message: &str, author_name: &str) -> Result<Committed> {
    let repo = Repository::open(repo_dir)?;
    let mut index = repo.index()?;
    index.add_all(["*"], IndexAddOption::DEFAULT, None)?;
    index.update_all(["*"], None)?;
    index.write()?;
    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;

    let parent = match repo.head() {
        Ok(head) => Some(head.peel_to_commit()?),
        Err(e)
            if e.code() == git2::ErrorCode::UnbornBranch
                || e.code() == git2::ErrorCode::NotFound =>
        {
            None
        }
        Err(e) => return Err(e.into()),
    };
    if let Some(parent) = &parent
        && parent.tree_id() == tree_id
    {
        return Ok(Committed {
            sha: parent.id().to_string(),
            changed: false,
        });
    }

    let email = format!("{}@sitewright.invalid", author_name.replace(' ', "-"));
    let signature = Signature::now(author_name, &email)?;
    let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
    let oid = repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        message,
        &tree,
        &parents,
    )?;
    Ok(Committed {
        sha: oid.to_string(),
        changed: true,
    })
}

/// Whether the working tree holds exactly the files of commit `sha`: no edits, additions
/// or deletions since. Stages the working tree as a side effect, which is harmless:
/// [`commit_all`] stages everything anyway.
pub fn worktree_matches(repo_dir: &Path, sha: &str) -> Result<bool> {
    let repo = Repository::open(repo_dir)?;
    let mut index = repo.index()?;
    index.add_all(["*"], IndexAddOption::DEFAULT, None)?;
    index.update_all(["*"], None)?;
    index.write()?;
    let tree_id = index.write_tree()?;
    let commit = repo.find_commit(Oid::from_str(sha)?)?;
    Ok(commit.tree_id() == tree_id)
}

/// Write the files of commit `sha` into `dest` (created if missing). Only regular files
/// are written: a tree can never produce a symlink or anything executable.
pub fn export_commit(repo_dir: &Path, sha: &str, dest: &Path) -> Result<()> {
    let repo = Repository::open(repo_dir)?;
    let commit = repo.find_commit(Oid::from_str(sha)?)?;
    let tree = commit.tree()?;
    std::fs::create_dir_all(dest)?;

    let mut failure: Option<SiteError> = None;
    tree.walk(TreeWalkMode::PreOrder, |dir, entry| {
        let Ok(name) = entry.name() else {
            return TreeWalkResult::Skip;
        };
        let rel = Path::new(dir).join(name);
        let result: Result<()> = (|| {
            match entry.kind() {
                Some(ObjectType::Tree) => std::fs::create_dir_all(dest.join(&rel))?,
                Some(ObjectType::Blob) if entry.filemode() != 0o120000 => {
                    let blob = repo.find_blob(entry.id())?;
                    std::fs::write(dest.join(&rel), blob.content())?;
                }
                _ => {}
            }
            Ok(())
        })();
        match result {
            Ok(()) => TreeWalkResult::Ok,
            Err(e) => {
                failure = Some(e);
                TreeWalkResult::Abort
            }
        }
    })
    .or_else(|e| if failure.is_some() { Ok(()) } else { Err(e) })?;
    match failure {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Make the working tree's files match commit `sha` without moving `HEAD`, so the next
/// commit records "back to this content" on top of the existing history.
pub fn restore_worktree(repo_dir: &Path, sha: &str) -> Result<()> {
    for entry in std::fs::read_dir(repo_dir)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
    }
    export_commit(repo_dir, sha, repo_dir)
}

/// One changed file in a commit, relative to its first parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    /// `A` added, `M` modified, `D` deleted.
    pub status: char,
}

/// The files commit `sha` changed compared with its parent (everything, for a root commit).
pub fn changed_files(repo_dir: &Path, sha: &str) -> Result<Vec<ChangedFile>> {
    let repo = Repository::open(repo_dir)?;
    let commit = repo.find_commit(Oid::from_str(sha)?)?;
    let tree = commit.tree()?;
    let parent_tree = match commit.parent(0) {
        Ok(parent) => Some(parent.tree()?),
        Err(_) => None,
    };
    let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)?;
    let mut changed = Vec::new();
    for delta in diff.deltas() {
        let status = match delta.status() {
            git2::Delta::Added => 'A',
            git2::Delta::Deleted => 'D',
            _ => 'M',
        };
        let file = if status == 'D' {
            delta.old_file()
        } else {
            delta.new_file()
        };
        if let Some(path) = file.path() {
            changed.push(ChangedFile {
                path: path.to_string_lossy().into_owned(),
                status,
            });
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_export_and_restore_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        init(&repo).unwrap();

        std::fs::create_dir_all(repo.join("css")).unwrap();
        std::fs::write(repo.join("index.html"), "v1").unwrap();
        std::fs::write(repo.join("css/a.css"), "a").unwrap();
        let first = commit_all(&repo, "first", "agent").unwrap();
        assert!(first.changed);

        // Nothing changed: no new commit.
        let again = commit_all(&repo, "noop", "agent").unwrap();
        assert!(!again.changed);
        assert_eq!(again.sha, first.sha);

        std::fs::write(repo.join("index.html"), "v2").unwrap();
        std::fs::remove_file(repo.join("css/a.css")).unwrap();
        std::fs::write(repo.join("new.html"), "n").unwrap();
        let second = commit_all(&repo, "second", "agent").unwrap();
        assert!(second.changed);
        let mut changed = changed_files(&repo, &second.sha).unwrap();
        changed.sort_by(|a, b| a.path.cmp(&b.path));
        let summary: Vec<_> = changed
            .iter()
            .map(|c| format!("{} {}", c.status, c.path))
            .collect();
        assert_eq!(summary, ["D css/a.css", "M index.html", "A new.html"]);

        let out = dir.path().join("out");
        export_commit(&repo, &first.sha, &out).unwrap();
        assert_eq!(
            std::fs::read_to_string(out.join("index.html")).unwrap(),
            "v1"
        );
        assert_eq!(std::fs::read_to_string(out.join("css/a.css")).unwrap(), "a");
        assert!(!out.join("new.html").exists());

        restore_worktree(&repo, &first.sha).unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.join("index.html")).unwrap(),
            "v1"
        );
        assert!(repo.join("css/a.css").exists());
        assert!(!repo.join("new.html").exists());
        // HEAD did not move, so committing now records the restored content as a new commit.
        let third = commit_all(&repo, "back to first", "user").unwrap();
        assert!(third.changed);
        assert_ne!(third.sha, first.sha);
    }
}
