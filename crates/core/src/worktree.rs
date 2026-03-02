use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("git command failed: {0}")]
    Git(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("bare repo not found: {0}")]
    BareRepoNotFound(String),
    /// Merge or rebase produced conflicts that need manual resolution.
    #[error("merge conflict: {0}")]
    Conflict(String),
}

pub type Result<T> = std::result::Result<T, WorktreeError>;

#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: String,
    pub is_bare: bool,
}

/// Status of the source repository's working tree.
///
/// Returned by `WorktreeManager::check_source_status()`.
/// An empty status (all fields zero/empty) means the source is clean.
#[derive(Debug, Default)]
pub struct SourceStatus {
    /// Files with staged or unstaged modifications.
    pub modified: Vec<String>,
    /// Untracked files (not in .gitignore).
    pub untracked: Vec<String>,
    /// Number of commits on HEAD not pushed to upstream.
    pub unpushed: usize,
}

impl SourceStatus {
    /// True if the source repo has no uncommitted work.
    pub fn is_clean(&self) -> bool {
        self.modified.is_empty() && self.untracked.is_empty() && self.unpushed == 0
    }

    /// Human-readable summary of issues.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.modified.is_empty() {
            parts.push(format!("{} modified file(s)", self.modified.len()));
        }
        if !self.untracked.is_empty() {
            parts.push(format!("{} untracked file(s)", self.untracked.len()));
        }
        if self.unpushed > 0 {
            parts.push(format!("{} unpushed commit(s)", self.unpushed));
        }
        if parts.is_empty() {
            "clean".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// Manages git worktrees for a project's shared bare repository.
pub struct WorktreeManager {
    bare_repo: PathBuf,
}

impl WorktreeManager {
    /// Create a new WorktreeManager for a bare repo.
    /// The bare repo must already exist.
    pub fn new(bare_repo: impl Into<PathBuf>) -> Result<Self> {
        let bare_repo = bare_repo.into();
        if !bare_repo.exists() {
            return Err(WorktreeError::BareRepoNotFound(
                bare_repo.display().to_string(),
            ));
        }
        Ok(Self { bare_repo })
    }

    /// Create a new worktree with a new branch based on the given base ref.
    /// Returns the path to the created worktree.
    ///
    /// Sets `GIT_LFS_SKIP_SMUDGE=1` to avoid downloading LFS objects during
    /// worktree creation (matches Gastown's optimization).
    pub fn create(&self, branch: &str, base_ref: &str, worktree_dir: &Path) -> Result<PathBuf> {
        let worktree_path = worktree_dir.join(branch);

        let output = Command::new("git")
            .args(["worktree", "add", "-b", branch])
            .arg(&worktree_path)
            .arg(base_ref)
            .env("GIT_DIR", &self.bare_repo)
            .env("GIT_LFS_SKIP_SMUDGE", "1")
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::Git(format!(
                "worktree add failed: {stderr}"
            )));
        }

        Ok(worktree_path)
    }

    /// Remove a worktree and its branch.
    pub fn remove(&self, worktree_path: &Path, delete_branch: bool) -> Result<()> {
        // Get the branch name before removing
        let branch = if delete_branch {
            self.branch_at(worktree_path).ok()
        } else {
            None
        };

        let output = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(worktree_path)
            .env("GIT_DIR", &self.bare_repo)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::Git(format!(
                "worktree remove failed: {stderr}"
            )));
        }

        // Delete the branch after removing the worktree
        if let Some(branch) = branch {
            let output = Command::new("git")
                .args(["branch", "-D", &branch])
                .env("GIT_DIR", &self.bare_repo)
                .output()?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!("failed to delete branch {branch}: {stderr}");
            }
        }

        Ok(())
    }

    /// List all worktrees for this bare repo.
    pub fn list(&self) -> Result<Vec<WorktreeInfo>> {
        let output = Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .env("GIT_DIR", &self.bare_repo)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::Git(format!(
                "worktree list failed: {stderr}"
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_worktree_list(&stdout))
    }

    /// Merge a branch into a target branch (e.g. main) within the bare repo.
    ///
    /// Tries fast-forward first. If that fails (diverged histories), falls back
    /// to a regular merge commit. Returns Ok(()) on success, Err on conflict.
    pub fn merge_branch(&self, source_branch: &str, target_branch: &str) -> Result<()> {
        // Try fast-forward merge first
        let output = Command::new("git")
            .args([
                "merge-base", "--is-ancestor",
                target_branch, source_branch,
            ])
            .env("GIT_DIR", &self.bare_repo)
            .output()?;

        if output.status.success() {
            // Fast-forward: just update the target ref to point at source
            let output = Command::new("git")
                .args([
                    "update-ref",
                    &format!("refs/heads/{target_branch}"),
                    source_branch,
                ])
                .env("GIT_DIR", &self.bare_repo)
                .output()?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(WorktreeError::Git(format!(
                    "update-ref failed: {stderr}"
                )));
            }
            return Ok(());
        }

        // Not fast-forwardable — need a real merge.
        // Bare repos can't do `git merge` directly. We need a temporary index.
        let tmp_index = self.bare_repo.join("enki-merge.index");

        // Read target branch tree into temp index
        let output = Command::new("git")
            .args(["read-tree", target_branch])
            .env("GIT_DIR", &self.bare_repo)
            .env("GIT_INDEX_FILE", &tmp_index)
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = std::fs::remove_file(&tmp_index);
            return Err(WorktreeError::Git(format!("read-tree failed: {stderr}")));
        }

        // Merge source branch into the temp index
        let output = Command::new("git")
            .args(["merge-tree", "--write-tree", target_branch, source_branch])
            .env("GIT_DIR", &self.bare_repo)
            .output()?;

        let _ = std::fs::remove_file(&tmp_index);

        if !output.status.success() {
            return Err(WorktreeError::Conflict(format!(
                "{source_branch} into {target_branch}"
            )));
        }

        // merge-tree --write-tree outputs the tree hash on success
        let tree_hash = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();

        if tree_hash.is_empty() {
            return Err(WorktreeError::Git("merge-tree produced no tree hash".into()));
        }

        // Create merge commit
        let output = Command::new("git")
            .args([
                "commit-tree", &tree_hash,
                "-p", target_branch,
                "-p", source_branch,
                "-m", &format!("Merge {source_branch} into {target_branch}"),
            ])
            .env("GIT_DIR", &self.bare_repo)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::Git(format!(
                "commit-tree failed: {stderr}"
            )));
        }

        let commit_hash = String::from_utf8_lossy(&output.stdout).trim().to_string();

        // Update target branch ref to the merge commit
        let output = Command::new("git")
            .args([
                "update-ref",
                &format!("refs/heads/{target_branch}"),
                &commit_hash,
            ])
            .env("GIT_DIR", &self.bare_repo)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::Git(format!(
                "update-ref after merge failed: {stderr}"
            )));
        }

        Ok(())
    }

    /// Get the branch checked out in a worktree.
    fn branch_at(&self, worktree_path: &Path) -> Result<String> {
        let output = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(worktree_path)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::Git(format!(
                "rev-parse failed: {stderr}"
            )));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Fetch the latest commits from origin into the bare repo.
    ///
    /// The bare repo's origin is the local source repo used when cloning.
    /// This keeps the bare repo in sync so workers branch from current code.
    /// After the refspec configuration in `init_bare`, this populates
    /// `refs/remotes/origin/*` without touching local branches.
    pub fn sync(&self) -> Result<()> {
        let output = Command::new("git")
            .args(["fetch", "origin", "--prune"])
            .env("GIT_DIR", &self.bare_repo)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::Git(format!("fetch failed: {stderr}")));
        }

        Ok(())
    }

    /// Get the source repo path (the bare repo's origin URL).
    pub fn source_repo(&self) -> Result<PathBuf> {
        let output = Command::new("git")
            .args(["config", "remote.origin.url"])
            .env("GIT_DIR", &self.bare_repo)
            .output()?;

        if !output.status.success() {
            return Err(WorktreeError::Git("no remote.origin.url configured".into()));
        }

        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(PathBuf::from(url))
    }

    /// Check the source repo for uncommitted work.
    ///
    /// Returns a list of issues (empty = clean). Checks for:
    /// - Modified/staged/untracked files (git status --porcelain)
    /// - Unpushed commits on the current branch
    ///
    /// Matches Gastown's `CheckUncommittedWork()` pattern.
    pub fn check_source_status(&self) -> Result<SourceStatus> {
        let source = self.source_repo()?;
        if !source.exists() {
            return Err(WorktreeError::Git(format!(
                "source repo not found: {}",
                source.display()
            )));
        }

        // Check for modified/untracked files
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&source)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::Git(format!(
                "git status failed: {stderr}"
            )));
        }

        let status_output = String::from_utf8_lossy(&output.stdout);
        let mut modified = Vec::new();
        let mut untracked = Vec::new();

        for line in status_output.lines() {
            if line.len() < 4 {
                continue;
            }
            let file = line[3..].to_string();
            if line.starts_with("??") {
                untracked.push(file);
            } else {
                modified.push(file);
            }
        }

        // Check for unpushed commits (commits on HEAD not in origin/HEAD)
        // This is best-effort — may fail if there's no upstream
        let unpushed = Command::new("git")
            .args(["rev-list", "--count", "@{upstream}..HEAD"])
            .current_dir(&source)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .parse::<usize>()
                    .ok()
            })
            .unwrap_or(0);

        Ok(SourceStatus {
            modified,
            untracked,
            unpushed,
        })
    }

    /// Rebase a worktree's branch onto target_branch.
    ///
    /// Runs `git rebase <target_branch>` inside the worktree directory.
    /// If rebase fails (real conflict), aborts the rebase and returns `Conflict`.
    /// On success the worktree's branch is linearly on top of target_branch,
    /// so a subsequent `merge_branch` will always fast-forward.
    pub fn rebase_onto(&self, worktree_path: &Path, target_branch: &str) -> Result<()> {
        let output = Command::new("git")
            .args([
                "-c", "user.name=enki",
                "-c", "user.email=enki@local",
                "rebase", target_branch,
            ])
            .current_dir(worktree_path)
            .output()?;

        if !output.status.success() {
            // Clean up the in-progress rebase so the worktree is usable again.
            let _ = Command::new("git")
                .args(["rebase", "--abort"])
                .current_dir(worktree_path)
                .output();

            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::Conflict(format!(
                "rebase onto {target_branch} failed: {stderr}"
            )));
        }

        Ok(())
    }

    /// Initialize a bare repo from an existing repo.
    ///
    /// After cloning, configures the refspec so that `git fetch origin` populates
    /// `refs/remotes/origin/*` instead of overwriting local `refs/heads/*`.
    /// This keeps the bare repo's local branches (our merge targets) separate
    /// from the source repo's branches (what workers branch from).
    ///
    /// Matches Gastown's `configureRefspec()` pattern.
    pub fn init_bare(source_repo: &Path, bare_path: &Path) -> Result<()> {
        let output = Command::new("git")
            .args(["clone", "--bare"])
            .arg(source_repo)
            .arg(bare_path)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::Git(format!(
                "bare clone failed: {stderr}"
            )));
        }

        // Reconfigure refspec: map remote heads into refs/remotes/origin/*
        // instead of the bare-clone default of refs/heads/* (which overwrites
        // our local branches on fetch).
        let output = Command::new("git")
            .args([
                "config", "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ])
            .env("GIT_DIR", bare_path)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::Git(format!(
                "refspec config failed: {stderr}"
            )));
        }

        // Run initial fetch to populate refs/remotes/origin/*
        let output = Command::new("git")
            .args(["fetch", "origin"])
            .env("GIT_DIR", bare_path)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::Git(format!(
                "initial fetch failed: {stderr}"
            )));
        }

        Ok(())
    }
}

fn parse_worktree_list(output: &str) -> Vec<WorktreeInfo> {
    let mut worktrees = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch = String::new();
    let mut is_bare = false;

    for line in output.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            // Save previous entry if exists
            if let Some(path) = current_path.take() {
                worktrees.push(WorktreeInfo {
                    path,
                    branch: std::mem::take(&mut current_branch),
                    is_bare,
                });
                is_bare = false;
            }
            current_path = Some(PathBuf::from(path));
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            // "refs/heads/main" -> "main"
            current_branch = branch_ref
                .strip_prefix("refs/heads/")
                .unwrap_or(branch_ref)
                .to_string();
        } else if line == "bare" {
            is_bare = true;
        }
    }

    // Don't forget the last entry
    if let Some(path) = current_path {
        worktrees.push(WorktreeInfo {
            path,
            branch: current_branch,
            is_bare,
        });
    }

    worktrees
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Helpers ──────────────────────────────────────────────

    /// Create a temp dir with a unique name for test isolation.
    fn tmp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("enki-{prefix}-{}", ulid::Ulid::new()))
    }

    /// Create a source repo with an initial commit on `main`.
    fn setup_source(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        run_git(path, &["init"]);
        run_git(path, &["checkout", "-b", "main"]);
        std::fs::write(path.join("README.md"), "# test").unwrap();
        run_git(path, &["add", "."]);
        run_git_with_author(path, &["commit", "-m", "init"]);
    }

    /// Full setup: source repo → bare clone → WorktreeManager.
    fn setup_all(tmp: &Path) -> (PathBuf, PathBuf, PathBuf, WorktreeManager) {
        let source = tmp.join("source");
        let bare = tmp.join("repo.git");
        let worktrees = tmp.join("worktrees");
        setup_source(&source);
        WorktreeManager::init_bare(&source, &bare).unwrap();
        let mgr = WorktreeManager::new(&bare).unwrap();
        std::fs::create_dir_all(&worktrees).unwrap();
        (source, bare, worktrees, mgr)
    }

    fn run_git_with_author(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(["-c", "user.name=test", "-c", "user.email=test@test.com"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        if !output.status.success() {
            panic!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        if !output.status.success() {
            panic!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    fn git_output(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn git_output_bare(bare: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .env("GIT_DIR", bare)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    // ─── Unit tests ──────────────────────────────────────────

    #[test]
    fn parse_worktree_list_output() {
        let output = "\
worktree /tmp/repo.git
bare

worktree /tmp/worktrees/task/feature-auth
branch refs/heads/task/feature-auth

worktree /tmp/worktrees/task/fix-login
branch refs/heads/task/fix-login

";
        let result = parse_worktree_list(output);
        assert_eq!(result.len(), 3);

        assert_eq!(result[0].path, PathBuf::from("/tmp/repo.git"));
        assert!(result[0].is_bare);

        assert_eq!(result[1].branch, "task/feature-auth");
        assert_eq!(result[2].branch, "task/fix-login");
    }

    #[test]
    fn bare_repo_not_found() {
        let result = WorktreeManager::new("/nonexistent/path");
        assert!(matches!(result, Err(WorktreeError::BareRepoNotFound(_))));
    }

    #[test]
    fn source_status_clean() {
        let status = SourceStatus::default();
        assert!(status.is_clean());
        assert_eq!(status.summary(), "clean");
    }

    #[test]
    fn source_status_dirty() {
        let status = SourceStatus {
            modified: vec!["foo.rs".into()],
            untracked: vec!["bar.txt".into(), "baz.txt".into()],
            unpushed: 3,
        };
        assert!(!status.is_clean());
        assert_eq!(
            status.summary(),
            "1 modified file(s), 2 untracked file(s), 3 unpushed commit(s)"
        );
    }

    // ─── Bare repo setup (matching Gastown) ──────────────────

    #[test]
    fn init_bare_configures_refspec() {
        let tmp = tmp_dir("refspec");
        let (_, bare, _, _mgr) = setup_all(&tmp);

        // Verify refspec is set to map into refs/remotes/origin/*
        let refspec = git_output_bare(&bare, &["config", "remote.origin.fetch"]);
        assert_eq!(refspec, "+refs/heads/*:refs/remotes/origin/*");

        // Verify origin/main ref exists
        let origin_main = git_output_bare(&bare, &["rev-parse", "origin/main"]);
        assert!(!origin_main.is_empty(), "origin/main should exist after init_bare");

        // Verify local main also exists (from the original clone)
        let local_main = git_output_bare(&bare, &["rev-parse", "main"]);
        assert!(!local_main.is_empty(), "local main should exist after init_bare");

        // They should point at the same commit initially
        assert_eq!(origin_main, local_main);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn init_bare_source_repo_is_readable() {
        let tmp = tmp_dir("source-repo");
        let (source, _, _, mgr) = setup_all(&tmp);

        let resolved = mgr.source_repo().unwrap();
        assert_eq!(resolved, source);

        std::fs::remove_dir_all(&tmp).ok();
    }

    // ─── Sync: committed changes propagate ───────────────────

    #[test]
    fn sync_propagates_committed_changes() {
        let tmp = tmp_dir("sync-commit");
        let (source, _bare, worktrees, mgr) = setup_all(&tmp);

        // Add a new committed file in source AFTER bare clone
        std::fs::write(source.join("new.txt"), "new content").unwrap();
        run_git(&source, &["add", "new.txt"]);
        run_git_with_author(&source, &["commit", "-m", "add new.txt"]);

        // Before sync: worktree from origin/main should NOT have new.txt
        // (origin/main still points at the old commit)
        let wt_before = mgr.create("task/before-sync", "origin/main", &worktrees).unwrap();
        assert!(!wt_before.join("new.txt").exists(),
            "new.txt should not exist before sync");

        // Sync
        mgr.sync().unwrap();

        // After sync: worktree from origin/main should have new.txt
        let wt_after = mgr.create("task/after-sync", "origin/main", &worktrees).unwrap();
        assert!(wt_after.join("new.txt").exists(),
            "new.txt should exist after sync");
        assert_eq!(
            std::fs::read_to_string(wt_after.join("new.txt")).unwrap(),
            "new content"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    // ─── Sync does NOT overwrite local merge target ──────────

    #[test]
    fn sync_does_not_overwrite_local_main() {
        let tmp = tmp_dir("sync-preserve");
        let (source, _bare, worktrees, mgr) = setup_all(&tmp);

        // Worker branches from origin/main, makes a commit, merges into local main
        let wt = mgr.create("task/worker-1", "origin/main", &worktrees).unwrap();
        std::fs::write(wt.join("worker.txt"), "worker output").unwrap();
        run_git(&wt, &["add", "."]);
        run_git_with_author(&wt, &["commit", "-m", "worker commit"]);
        mgr.merge_branch("task/worker-1", "main").unwrap();

        // Now source makes a new commit (simulates user continuing to work)
        std::fs::write(source.join("user.txt"), "user work").unwrap();
        run_git(&source, &["add", "user.txt"]);
        run_git_with_author(&source, &["commit", "-m", "user commit"]);

        // Sync — this should update origin/main but NOT touch local main
        mgr.sync().unwrap();

        // Local main should still have worker.txt (from the merge)
        let verify_main = mgr.create("verify-main", "main", &worktrees).unwrap();
        assert!(verify_main.join("worker.txt").exists(),
            "local main should preserve merged worker output after sync");

        // origin/main should have user.txt but NOT worker.txt
        let verify_origin = mgr.create("verify-origin", "origin/main", &worktrees).unwrap();
        assert!(verify_origin.join("user.txt").exists(),
            "origin/main should have new source commits after sync");
        assert!(!verify_origin.join("worker.txt").exists(),
            "origin/main should not have worker's merged changes");

        std::fs::remove_dir_all(&tmp).ok();
    }

    // ─── Uncommitted changes do NOT propagate ────────────────

    #[test]
    fn uncommitted_changes_not_propagated() {
        let tmp = tmp_dir("uncommitted");
        let (source, _, worktrees, mgr) = setup_all(&tmp);

        // Modify a tracked file WITHOUT committing
        std::fs::write(source.join("README.md"), "# modified but not committed").unwrap();

        // Sync and create worktree
        mgr.sync().unwrap();
        let wt = mgr.create("task/test", "origin/main", &worktrees).unwrap();

        // Worktree should have the ORIGINAL content, not the uncommitted change
        assert_eq!(
            std::fs::read_to_string(wt.join("README.md")).unwrap(),
            "# test"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    // ─── Untracked files do NOT propagate ────────────────────

    #[test]
    fn untracked_files_not_propagated() {
        let tmp = tmp_dir("untracked");
        let (source, _, worktrees, mgr) = setup_all(&tmp);

        // Add a new file WITHOUT staging or committing
        std::fs::write(source.join("secret.env"), "API_KEY=hunter2").unwrap();

        // Sync and create worktree
        mgr.sync().unwrap();
        let wt = mgr.create("task/test", "origin/main", &worktrees).unwrap();

        // Worktree should NOT have the untracked file
        assert!(!wt.join("secret.env").exists(),
            "untracked files should not appear in worktrees");

        std::fs::remove_dir_all(&tmp).ok();
    }

    // ─── check_source_status ─────────────────────────────────

    #[test]
    fn check_source_status_clean() {
        let tmp = tmp_dir("status-clean");
        let (_source, _, _, mgr) = setup_all(&tmp);

        let status = mgr.check_source_status().unwrap();
        assert!(status.is_clean(), "fresh source should be clean");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn check_source_status_modified() {
        let tmp = tmp_dir("status-mod");
        let (source, _, _, mgr) = setup_all(&tmp);

        // Modify tracked file
        std::fs::write(source.join("README.md"), "modified").unwrap();

        let status = mgr.check_source_status().unwrap();
        assert!(!status.is_clean());
        assert_eq!(status.modified.len(), 1);
        assert!(status.modified[0].contains("README.md"));

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn check_source_status_untracked() {
        let tmp = tmp_dir("status-untracked");
        let (source, _, _, mgr) = setup_all(&tmp);

        // Create untracked file
        std::fs::write(source.join("new_file.txt"), "hello").unwrap();

        let status = mgr.check_source_status().unwrap();
        assert!(!status.is_clean());
        assert_eq!(status.untracked.len(), 1);
        assert!(status.untracked[0].contains("new_file.txt"));

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn check_source_status_staged() {
        let tmp = tmp_dir("status-staged");
        let (source, _, _, mgr) = setup_all(&tmp);

        // Stage a new file but don't commit
        std::fs::write(source.join("staged.txt"), "staged").unwrap();
        run_git(&source, &["add", "staged.txt"]);

        let status = mgr.check_source_status().unwrap();
        assert!(!status.is_clean());
        assert!(!status.modified.is_empty(), "staged files should show as modified");

        std::fs::remove_dir_all(&tmp).ok();
    }

    // ─── Worktree lifecycle (updated for origin/main) ────────

    #[test]
    fn worktree_from_origin_main() {
        let tmp = tmp_dir("origin-main");
        let (_source, _, worktrees, mgr) = setup_all(&tmp);

        // Create worktree branching from origin/main (the Gastown pattern)
        let wt = mgr.create("task/feature-1", "origin/main", &worktrees).unwrap();
        assert!(wt.exists());
        assert!(wt.join("README.md").exists());

        // Verify the branch is created
        let list = mgr.list().unwrap();
        assert!(list.iter().any(|w| w.branch == "task/feature-1"));

        // Remove and verify cleanup
        mgr.remove(&wt, true).unwrap();
        assert!(!wt.exists());

        std::fs::remove_dir_all(&tmp).ok();
    }

    // ─── Full Gastown-matching lifecycle ──────────────────────
    // This test exercises the complete flow as it happens in production:
    // 1. Source repo has initial code
    // 2. init_bare clones + configures refspec
    // 3. User commits more code to source
    // 4. sync() fetches into origin/*
    // 5. Workers branch from origin/main (see latest committed code)
    // 6. Workers commit, merge into local main
    // 7. sync() again — local main preserved, origin/main updated
    // 8. New workers branch from updated origin/main

    #[test]
    fn full_gastown_lifecycle() {
        let tmp = tmp_dir("gastown-full");
        let (source, bare, worktrees, mgr) = setup_all(&tmp);

        // ── Step 1: Source has initial commit (done by setup_all)

        // ── Step 2: User adds more code to source
        std::fs::write(source.join("lib.rs"), "fn main() {}").unwrap();
        run_git(&source, &["add", "lib.rs"]);
        run_git_with_author(&source, &["commit", "-m", "add lib.rs"]);
        let source_head = git_output(&source, &["rev-parse", "HEAD"]);

        // ── Step 3: Sync bare repo
        mgr.sync().unwrap();

        // Verify origin/main matches source HEAD
        let origin_main = git_output_bare(&bare, &["rev-parse", "origin/main"]);
        assert_eq!(origin_main, source_head, "origin/main should match source HEAD after sync");

        // ── Step 4: Spawn worker from origin/main
        let wt1 = mgr.create("task/worker-1", "origin/main", &worktrees).unwrap();
        assert!(wt1.join("lib.rs").exists(), "worker should see latest source code");

        // Worker does its work
        std::fs::write(wt1.join("feature.rs"), "// feature code").unwrap();
        run_git(&wt1, &["add", "."]);
        run_git_with_author(&wt1, &["commit", "-m", "implement feature"]);

        // ── Step 5: Merge worker into local main
        mgr.merge_branch("task/worker-1", "main").unwrap();

        // Verify local main has the worker's changes
        let verify = mgr.create("verify-1", "main", &worktrees).unwrap();
        assert!(verify.join("feature.rs").exists());
        assert!(verify.join("lib.rs").exists());
        mgr.remove(&verify, true).unwrap();

        // ── Step 6: Meanwhile, user makes another commit in source
        std::fs::write(source.join("config.toml"), "[settings]").unwrap();
        run_git(&source, &["add", "config.toml"]);
        run_git_with_author(&source, &["commit", "-m", "add config"]);

        // ── Step 7: Sync again
        mgr.sync().unwrap();

        // Local main should still have worker's feature.rs
        let verify2 = mgr.create("verify-2", "main", &worktrees).unwrap();
        assert!(verify2.join("feature.rs").exists(),
            "merged worker output survives sync");
        mgr.remove(&verify2, true).unwrap();

        // origin/main should have config.toml but NOT feature.rs
        let verify3 = mgr.create("verify-3", "origin/main", &worktrees).unwrap();
        assert!(verify3.join("config.toml").exists(),
            "origin/main has latest source changes");
        assert!(!verify3.join("feature.rs").exists(),
            "origin/main does not have worker merges");

        // ── Step 8: New worker branches from updated origin/main
        let wt2 = mgr.create("task/worker-2", "origin/main", &worktrees).unwrap();
        assert!(wt2.join("config.toml").exists(),
            "new worker sees latest source code");
        assert!(wt2.join("lib.rs").exists());
        assert!(!wt2.join("feature.rs").exists(),
            "new worker does NOT see previous worker's merged changes");

        std::fs::remove_dir_all(&tmp).ok();
    }

    // ─── Merge tests (kept, using local main as merge target) ─

    #[test]
    fn merge_fast_forward() {
        let tmp = tmp_dir("merge-ff");
        let (_source, _, worktrees, mgr) = setup_all(&tmp);

        let wt = mgr.create("task/feature-1", "origin/main", &worktrees).unwrap();
        std::fs::write(wt.join("new_file.txt"), "hello").unwrap();
        run_git(&wt, &["add", "."]);
        run_git_with_author(&wt, &["commit", "-m", "add new_file"]);

        mgr.merge_branch("task/feature-1", "main").unwrap();

        let verify = mgr.create("verify", "main", &worktrees).unwrap();
        assert!(verify.join("new_file.txt").exists());
        assert_eq!(
            std::fs::read_to_string(verify.join("new_file.txt")).unwrap(),
            "hello"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn merge_diverged() {
        let tmp = tmp_dir("merge-div");
        let (_source, _, worktrees, mgr) = setup_all(&tmp);

        let wt1 = mgr.create("task/feature-1", "origin/main", &worktrees).unwrap();
        let wt2 = mgr.create("task/feature-2", "origin/main", &worktrees).unwrap();

        std::fs::write(wt1.join("file1.txt"), "from feature-1").unwrap();
        run_git(&wt1, &["add", "."]);
        run_git_with_author(&wt1, &["commit", "-m", "add file1"]);

        std::fs::write(wt2.join("file2.txt"), "from feature-2").unwrap();
        run_git(&wt2, &["add", "."]);
        run_git_with_author(&wt2, &["commit", "-m", "add file2"]);

        mgr.merge_branch("task/feature-1", "main").unwrap();
        mgr.merge_branch("task/feature-2", "main").unwrap();

        let verify = mgr.create("verify", "main", &worktrees).unwrap();
        assert!(verify.join("file1.txt").exists());
        assert!(verify.join("file2.txt").exists());
        assert_eq!(
            std::fs::read_to_string(verify.join("file1.txt")).unwrap(),
            "from feature-1"
        );
        assert_eq!(
            std::fs::read_to_string(verify.join("file2.txt")).unwrap(),
            "from feature-2"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }
}
