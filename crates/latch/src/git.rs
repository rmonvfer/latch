//! The git operations the app needs, through libgit2.

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};
use git2::{Repository, StatusOptions, WorktreeAddOptions, WorktreePruneOptions};

/// Changes in a working tree relative to `HEAD`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiffStats {
    pub files: usize,
    pub insertions: usize,
    pub deletions: usize,
}

/// The main repository for `dir`, even when `dir` is inside a worktree.
fn main_repository(dir: &Path) -> Result<Repository> {
    let repo = Repository::discover(dir).context("not inside a git repository")?;
    if repo.is_worktree() {
        return Repository::open(repo.commondir()).context("cannot open the main repository");
    }
    Ok(repo)
}

/// The top-level directory of the repository (or worktree) containing `dir`.
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    let repo = Repository::discover(dir).ok()?;
    repo.workdir().map(Path::to_path_buf)
}

/// Uncommitted changes, staged and unstaged, relative to `HEAD`.
pub fn diff_stats(dir: &Path) -> Option<DiffStats> {
    let repo = Repository::discover(dir).ok()?;
    let head = repo.head().ok()?.peel_to_tree().ok()?;
    let diff = repo
        .diff_tree_to_workdir_with_index(Some(&head), None)
        .ok()?;
    let stats = diff.stats().ok()?;
    Some(DiffStats {
        files: stats.files_changed(),
        insertions: stats.insertions(),
        deletions: stats.deletions(),
    })
}

/// Create a worktree of the repository containing `dir` on a new branch
/// `agent/<name>` from `HEAD`, in a `<repo>-worktrees` folder next to the
/// repository. Returns the worktree's path.
pub fn create_worktree(dir: &Path, label: &str) -> Result<PathBuf> {
    let repo = main_repository(dir)?;
    let root = repo
        .workdir()
        .context("repository has no working directory")?
        .to_path_buf();
    let repo_name = root
        .file_name()
        .context("repository has no name")?
        .to_string_lossy()
        .into_owned();
    let parent = root
        .parent()
        .context("repository has no parent directory")?;
    let name = format!("{}-{}", slug(label), unique_suffix());
    let container = parent.join(format!("{repo_name}-worktrees"));
    std::fs::create_dir_all(&container)
        .with_context(|| format!("failed to create {}", container.display()))?;
    let path = container.join(&name);

    let commit = repo
        .head()
        .and_then(|head| head.peel_to_commit())
        .context("the repository has no commit to branch from")?;
    let branch = repo
        .branch(&format!("agent/{name}"), &commit, false)
        .context("failed to create the agent branch")?;
    let mut options = WorktreeAddOptions::new();
    options.reference(Some(branch.get()));
    repo.worktree(&name, &path, Some(&options))
        .with_context(|| format!("failed to create worktree at {}", path.display()))?;
    Ok(path)
}

/// Remove a worktree and its directory. Refuses when it has uncommitted or
/// untracked changes, so no work is lost.
pub fn remove_worktree(path: &Path) -> Result<()> {
    let checkout = Repository::open(path).context("not a git checkout")?;
    if !checkout.is_worktree() {
        bail!("{} is not a worktree", path.display());
    }
    let mut options = StatusOptions::new();
    options.include_untracked(true).include_ignored(false);
    if !checkout.statuses(Some(&mut options))?.is_empty() {
        bail!("the worktree has uncommitted changes");
    }
    let name = path
        .file_name()
        .context("worktree has no name")?
        .to_string_lossy()
        .into_owned();
    let repo = main_repository(path)?;
    let worktree = repo
        .find_worktree(&name)
        .context("the repository does not know this worktree")?;
    worktree.prune(Some(
        WorktreePruneOptions::new().valid(true).working_tree(true),
    ))?;
    Ok(())
}

/// Lowercase ASCII words joined by dashes: "Claude Code" → "claude-code".
fn slug(label: &str) -> String {
    let words: Vec<String> = label
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    if words.is_empty() {
        "agent".to_string()
    } else {
        words.join("-")
    }
}

/// A short, time-ordered suffix that keeps worktree names distinct.
fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default();
    to_base36(nanos)
}

fn to_base36(mut value: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(value % 36) as usize]);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).expect("ascii digits")
}

#[cfg(test)]
mod tests {
    use git2::{IndexAddOption, Signature};

    use super::*;

    fn commit_all(repo: &Repository, message: &str) {
        let mut index = repo.index().unwrap();
        index.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = Signature::now("test", "test@example.com").unwrap();
        let parents: Vec<git2::Commit> = repo
            .head()
            .ok()
            .and_then(|head| head.peel_to_commit().ok())
            .into_iter()
            .collect();
        let parents: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parents,
        )
        .unwrap();
    }

    #[test]
    fn slugs_and_suffixes() {
        assert_eq!(slug("Claude Code"), "claude-code");
        assert_eq!(slug("  "), "agent");
        assert_eq!(to_base36(0), "0");
        assert_eq!(to_base36(36 * 36 + 1), "101");
    }

    #[test]
    fn creates_reports_and_removes_worktrees() {
        let base = std::env::temp_dir().join(format!("git-test-{}", std::process::id()));
        std::fs::create_dir_all(base.join("repo")).unwrap();
        let base = base.canonicalize().unwrap();
        let repo_dir = base.join("repo");
        let repo = Repository::init(&repo_dir).unwrap();
        std::fs::write(repo_dir.join("a.txt"), "one\n").unwrap();
        commit_all(&repo, "init");

        std::fs::write(repo_dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        assert_eq!(repo_root(&repo_dir).unwrap(), repo_dir.join(""));
        assert_eq!(
            diff_stats(&repo_dir),
            Some(DiffStats {
                files: 1,
                insertions: 2,
                deletions: 0
            })
        );

        let worktree = create_worktree(&repo_dir, "Claude Code").unwrap();
        assert!(worktree.starts_with(base.join("repo-worktrees")));
        assert!(worktree.join("a.txt").exists());
        // Creating from inside a worktree still targets the main repository.
        let nested = create_worktree(&worktree, "Codex").unwrap();
        assert!(nested.starts_with(base.join("repo-worktrees")));

        std::fs::write(worktree.join("new.txt"), "work").unwrap();
        assert!(remove_worktree(&worktree).is_err());
        std::fs::remove_file(worktree.join("new.txt")).unwrap();
        remove_worktree(&worktree).unwrap();
        assert!(!worktree.exists());
        remove_worktree(&nested).unwrap();
        std::fs::remove_dir_all(&base).unwrap();
    }
}
