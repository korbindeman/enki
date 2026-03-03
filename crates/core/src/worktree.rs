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

    /// Detect the default branch name (e.g., `main` or `master`).
    ///
    /// Returns an error if the repo has no branches (empty repo with no commits).
    pub fn default_branch(&self) -> Result<String> {
        for candidate in &["main", "master"] {
            let output = Command::new("git")
                .args(["rev-parse", "--verify", candidate])
                .env("GIT_DIR", &self.bare_repo)
                .output();
            if let Ok(out) = output {
                if out.status.success() {
                    return Ok(candidate.to_string());
                }
            }
        }

        Err(WorktreeError::Git(
            "no branches found — the repo needs at least one commit before workers can run".into(),
        ))
    }

    /// The remote-tracking ref to branch workers from (e.g., `origin/main`).
    pub fn default_start_ref(&self) -> Result<String> {
        let branch = self.default_branch()?;
        // Prefer the remote-tracking ref if it exists.
        let origin_ref = format!("origin/{branch}");
        let output = Command::new("git")
            .args(["rev-parse", "--verify", &origin_ref])
            .env("GIT_DIR", &self.bare_repo)
            .output();
        if let Ok(out) = output {
            if out.status.success() {
                return Ok(origin_ref);
            }
        }
        // No remote tracking ref — fall back to the local branch.
        Ok(branch)
    }

    /// Commit any uncommitted changes in a worktree.
    ///
    /// Workers may finish without committing (the agent isn't always reliable
    /// about running `git commit`). This captures their work so it isn't lost
    /// when the worktree is removed. Returns `true` if a commit was created.
    pub fn commit_uncommitted(&self, worktree_path: &Path, message: &str) -> bool {
        // Check for dirty state (modified, untracked, deleted).
        let status = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(worktree_path)
            .output();
        let has_dirty = match status {
            Ok(ref out) => !String::from_utf8_lossy(&out.stdout).trim().is_empty(),
            Err(_) => return false,
        };
        if !has_dirty {
            return false;
        }

        // Stage everything and commit.
        let add = Command::new("git")
            .args(["add", "-A"])
            .current_dir(worktree_path)
            .output();
        if add.is_err() || !add.unwrap().status.success() {
            tracing::warn!(worktree = %worktree_path.display(), "auto-commit: git add -A failed");
            return false;
        }

        let commit = Command::new("git")
            .args(["commit", "-m", message, "--no-verify"])
            .current_dir(worktree_path)
            .output();
        match commit {
            Ok(ref out) if out.status.success() => {
                tracing::info!(worktree = %worktree_path.display(), "auto-committed uncommitted worker changes");
                true
            }
            Ok(ref out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                tracing::warn!(worktree = %worktree_path.display(), stderr = %stderr, "auto-commit: git commit failed");
                false
            }
            Err(e) => {
                tracing::warn!(worktree = %worktree_path.display(), error = %e, "auto-commit: failed to run git commit");
                false
            }
        }
    }

    /// Check whether a branch has any file changes compared to its starting ref.
    ///
    /// Returns `true` if the branch tree differs from the base ref tree (i.e. the
    /// worker actually produced output). Works against the bare repo directly —
    /// no worktree needed.
    pub fn branch_has_changes(&self, branch: &str, base_ref: &str) -> bool {
        let tree_of = |refspec: &str| -> Option<String> {
            let output = Command::new("git")
                .args(["rev-parse", &format!("{refspec}^{{tree}}")])
                .env("GIT_DIR", &self.bare_repo)
                .output()
                .ok()?;
            if output.status.success() {
                Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                None
            }
        };
        match (tree_of(base_ref), tree_of(branch)) {
            (Some(base), Some(br)) => base != br,
            _ => false, // can't determine — assume no changes
        }
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

    /// Delete a branch from the bare repo.
    pub fn delete_branch(&self, branch: &str) -> Result<()> {
        let output = Command::new("git")
            .args(["branch", "-D", branch])
            .env("GIT_DIR", &self.bare_repo)
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::Git(format!(
                "branch delete failed: {stderr}"
            )));
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

    /// Check if a branch has been merged into the target branch.
    pub fn branch_merged(&self, branch: &str, target: &str) -> bool {
        let output = Command::new("git")
            .args(["merge-base", "--is-ancestor", branch, target])
            .env("GIT_DIR", &self.bare_repo)
            .output();
        matches!(output, Ok(o) if o.status.success())
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

    /// Fetch the latest commits from origin into the bare repo, including
    /// any uncommitted/untracked changes in the source working tree.
    ///
    /// 1. Fetches committed changes (`refs/remotes/origin/*`).
    /// 2. If the source working tree is dirty, creates a snapshot commit
    ///    using plumbing commands (without touching HEAD, index, or working
    ///    tree in the source) and overwrites `origin/main` in the bare repo
    ///    so workers branch from the actual filesystem state.
    pub fn sync(&self) -> Result<()> {
        tracing::debug!(bare_repo = %self.bare_repo.display(), "syncing bare repo from source");

        let output = Command::new("git")
            .args(["fetch", "origin", "--prune"])
            .env("GIT_DIR", &self.bare_repo)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::error!(stderr = %stderr, "bare repo fetch failed");
            return Err(WorktreeError::Git(format!("fetch failed: {stderr}")));
        }

        tracing::debug!("bare repo fetch succeeded, snapshotting dirty state");

        // Snapshot dirty state so workers see the actual filesystem.
        self.snapshot_dirty_state()?;

        Ok(())
    }

    /// Create a snapshot commit of the source's dirty state and update
    /// `origin/<default_branch>` in the bare repo to point at it.
    ///
    /// Uses only plumbing commands — never touches the source repo's HEAD,
    /// index, or working tree.
    fn snapshot_dirty_state(&self) -> Result<()> {
        let source = self.source_repo()?;

        // Quick check: is the source dirty?
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&source)
            .output()?;
        let status_out = String::from_utf8_lossy(&output.stdout);
        if status_out.trim().is_empty() {
            tracing::debug!(source = %source.display(), "source is clean, skipping dirty snapshot");
            return Ok(()); // Clean — nothing to snapshot.
        }

        let dirty_file_count = status_out.lines().count();
        tracing::debug!(
            source = %source.display(),
            dirty_files = dirty_file_count,
            "source is dirty, creating snapshot commit"
        );

        let git_dir = source.join(".git");
        let tmp_index = git_dir.join("enki-snapshot-index");

        // Copy the real index as a starting point so tracked files are present.
        let real_index = git_dir.join("index");
        if real_index.exists() {
            std::fs::copy(&real_index, &tmp_index)?;
        }

        // Stage everything (tracked modifications + untracked, respects .gitignore).
        let output = Command::new("git")
            .args(["add", "-A"])
            .env("GIT_INDEX_FILE", &tmp_index)
            .current_dir(&source)
            .output()?;
        if !output.status.success() {
            let _ = std::fs::remove_file(&tmp_index);
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::Git(format!(
                "snapshot: git add -A failed: {stderr}"
            )));
        }

        // Write tree from the temp index.
        let output = Command::new("git")
            .args(["write-tree"])
            .env("GIT_INDEX_FILE", &tmp_index)
            .current_dir(&source)
            .output()?;
        let _ = std::fs::remove_file(&tmp_index);
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::Git(format!(
                "snapshot: write-tree failed: {stderr}"
            )));
        }
        let tree_hash = String::from_utf8_lossy(&output.stdout).trim().to_string();

        // Get current HEAD of source.
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&source)
            .output()?;
        if !output.status.success() {
            return Err(WorktreeError::Git("snapshot: rev-parse HEAD failed".into()));
        }
        let head_hash = String::from_utf8_lossy(&output.stdout).trim().to_string();

        // Create a commit object (doesn't move any refs in the source).
        let output = Command::new("git")
            .args([
                "commit-tree", &tree_hash,
                "-p", &head_hash,
                "-m", "enki: filesystem snapshot (uncommitted changes)",
            ])
            .env("GIT_AUTHOR_NAME", "enki")
            .env("GIT_AUTHOR_EMAIL", "enki@local")
            .env("GIT_COMMITTER_NAME", "enki")
            .env("GIT_COMMITTER_EMAIL", "enki@local")
            .current_dir(&source)
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::Git(format!(
                "snapshot: commit-tree failed: {stderr}"
            )));
        }
        let commit_hash = String::from_utf8_lossy(&output.stdout).trim().to_string();

        // Store on a temp ref in source so we can fetch it into the bare repo.
        let output = Command::new("git")
            .args(["update-ref", "refs/enki/snapshot", &commit_hash])
            .current_dir(&source)
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::Git(format!(
                "snapshot: update-ref failed: {stderr}"
            )));
        }

        // Fetch the snapshot into the bare repo, overwriting origin/<branch>.
        let default_branch = self.default_branch().unwrap_or_else(|_| "main".to_string());
        let output = Command::new("git")
            .args([
                "fetch", "origin",
                &format!("refs/enki/snapshot:refs/remotes/origin/{default_branch}"),
            ])
            .env("GIT_DIR", &self.bare_repo)
            .output()?;

        // Clean up temp ref regardless of fetch outcome.
        let _ = Command::new("git")
            .args(["update-ref", "-d", "refs/enki/snapshot"])
            .current_dir(&source)
            .output();

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::Git(format!(
                "snapshot: fetch into bare failed: {stderr}"
            )));
        }

        tracing::info!("synced dirty filesystem snapshot into bare repo");
        Ok(())
    }

    /// Pull merged work from the bare repo into the source working directory.
    ///
    /// Stashes any uncommitted changes, fetches the branch from the bare repo,
    /// resets the working tree, then pops the stash. If the stash pop conflicts,
    /// the stash is preserved and a warning is logged.
    pub fn update_source_workdir(&self, branch: &str) -> Result<()> {
        let source = self.source_repo()?;
        tracing::debug!(source = %source.display(), branch, "pulling merged work into source working directory");

        // Check for dirty state and stash if needed.
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&source)
            .output()?;
        let is_dirty = !String::from_utf8_lossy(&output.stdout).trim().is_empty();

        let did_stash = if is_dirty {
            tracing::debug!("source has uncommitted changes, stashing before reset");
            let output = Command::new("git")
                .args([
                    "stash", "push", "--include-untracked",
                    "-m", "enki: auto-stash before pulling merged work",
                ])
                .current_dir(&source)
                .output()?;
            // "No local changes to save" means stash didn't actually save anything.
            let stashed = output.status.success()
                && !String::from_utf8_lossy(&output.stdout).contains("No local changes");
            tracing::debug!(stashed, "stash result");
            stashed
        } else {
            false
        };

        // Fetch the branch from bare repo (sets FETCH_HEAD).
        let bare_str = self.bare_repo.to_string_lossy().to_string();
        let output = Command::new("git")
            .args(["fetch", &bare_str, branch])
            .current_dir(&source)
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Pop stash before returning error.
            if did_stash {
                let _ = Command::new("git")
                    .args(["stash", "pop"])
                    .current_dir(&source)
                    .output();
            }
            return Err(WorktreeError::Git(format!(
                "fetch from bare failed: {stderr}"
            )));
        }

        // Merge the fetched commit into the source working tree.
        // We use merge instead of `reset --hard` because reset recreates the
        // entire working tree, which invalidates the inode of the cwd for any
        // shell session sitting inside the repo (causing "No such file or
        // directory" errors). Merge only touches files that actually changed.
        let output = Command::new("git")
            .args(["merge", "FETCH_HEAD", "--no-edit", "--ff"])
            .env("GIT_AUTHOR_NAME", "enki")
            .env("GIT_AUTHOR_EMAIL", "enki@local")
            .env("GIT_COMMITTER_NAME", "enki")
            .env("GIT_COMMITTER_EMAIL", "enki@local")
            .current_dir(&source)
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Abort the failed merge so the repo isn't left in a conflicted state.
            let _ = Command::new("git")
                .args(["merge", "--abort"])
                .current_dir(&source)
                .output();
            if did_stash {
                let _ = Command::new("git")
                    .args(["stash", "pop"])
                    .current_dir(&source)
                    .output();
            }
            return Err(WorktreeError::Git(format!(
                "merge FETCH_HEAD failed: {stderr}"
            )));
        }

        // Restore stashed changes.
        if did_stash {
            let output = Command::new("git")
                .args(["stash", "pop"])
                .current_dir(&source)
                .output()?;
            if !output.status.success() {
                tracing::warn!(
                    "stash pop had conflicts — user's uncommitted changes preserved in git stash"
                );
            }
        }

        tracing::info!(branch, "updated source working directory from bare repo");
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
            .args(["rebase", target_branch])
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
        // If the source repo has no commits, create an initial one so the
        // bare clone has something to work with.
        let head_check = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(source_repo)
            .output();
        let is_empty = match head_check {
            Ok(out) => !out.status.success(),
            Err(_) => true,
        };
        if is_empty {
            let output = Command::new("git")
                .args([
                    "commit", "--allow-empty", "-m", "Initial commit (created by enki)",
                ])
                .current_dir(source_repo)
                .output()?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(WorktreeError::Git(format!(
                    "failed to create initial commit in empty repo: {stderr}"
                )));
            }
        }

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
    use crate::db::Db;
    use crate::refinery::{self, MergeOutcome};
    use crate::types::{Id, MergeRequest, MergeStatus, Task, TaskStatus};
    use chrono::Utc;

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

    // ─── Uncommitted changes DO propagate via dirty snapshot ──

    #[test]
    fn uncommitted_changes_propagated() {
        let tmp = tmp_dir("uncommitted");
        let (source, _, worktrees, mgr) = setup_all(&tmp);

        // Modify a tracked file WITHOUT committing
        std::fs::write(source.join("README.md"), "# modified but not committed").unwrap();

        // Sync snapshots the dirty state into origin/main
        mgr.sync().unwrap();
        let wt = mgr.create("task/test", "origin/main", &worktrees).unwrap();

        // Worktree should have the MODIFIED content
        assert_eq!(
            std::fs::read_to_string(wt.join("README.md")).unwrap(),
            "# modified but not committed"
        );

        // Source repo's actual HEAD should be unchanged (snapshot used plumbing only)
        let head_content = git_output(&source, &["show", "HEAD:README.md"]);
        assert_eq!(head_content, "# test", "source HEAD should be untouched");

        std::fs::remove_dir_all(&tmp).ok();
    }

    // ─── Untracked files DO propagate via dirty snapshot ─────

    #[test]
    fn untracked_files_propagated() {
        let tmp = tmp_dir("untracked");
        let (source, _, worktrees, mgr) = setup_all(&tmp);

        // Add a new file WITHOUT staging or committing
        std::fs::write(source.join("new_file.txt"), "hello from source").unwrap();

        // Sync snapshots the dirty state
        mgr.sync().unwrap();
        let wt = mgr.create("task/test", "origin/main", &worktrees).unwrap();

        // Worktree should have the untracked file
        assert!(wt.join("new_file.txt").exists(),
            "untracked files should appear in worktrees via dirty snapshot");
        assert_eq!(
            std::fs::read_to_string(wt.join("new_file.txt")).unwrap(),
            "hello from source"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    // ─── Gitignored files do NOT propagate ───────────────────

    #[test]
    fn gitignored_files_not_propagated() {
        let tmp = tmp_dir("gitignored");
        let (source, _, worktrees, mgr) = setup_all(&tmp);

        // Create .gitignore and an ignored file
        std::fs::write(source.join(".gitignore"), "secret.env\n").unwrap();
        run_git(&source, &["add", ".gitignore"]);
        run_git_with_author(&source, &["commit", "-m", "add gitignore"]);
        std::fs::write(source.join("secret.env"), "API_KEY=hunter2").unwrap();

        mgr.sync().unwrap();
        let wt = mgr.create("task/test", "origin/main", &worktrees).unwrap();

        assert!(!wt.join("secret.env").exists(),
            "gitignored files should not appear in worktrees");

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

    // ─── Worktree is functional for command execution ─────────
    // Verifies that a worktree created from the bare repo can actually
    // run git and shell commands — the same environment workers get.

    #[test]
    fn worktree_supports_command_execution() {
        let tmp = tmp_dir("cmd-exec");
        let (_source, _, worktrees, mgr) = setup_all(&tmp);

        let wt = mgr.create("task/cmd-test", "origin/main", &worktrees).unwrap();

        // .git file should exist (points to bare repo's worktrees dir).
        assert!(wt.join(".git").exists(), "worktree should have .git entry");

        // git status should work (basic git operation).
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&wt)
            .output()
            .unwrap();
        assert!(output.status.success(), "git status should succeed in worktree");

        // git rev-parse should report the correct branch.
        let branch = git_output(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(branch, "task/cmd-test");

        // Shell commands should work with the worktree as cwd.
        let output = Command::new("ls")
            .current_dir(&wt)
            .output()
            .unwrap();
        assert!(output.status.success(), "ls should succeed in worktree");
        let ls_output = String::from_utf8_lossy(&output.stdout);
        assert!(ls_output.contains("README.md"), "ls should show worktree files");

        // Writing files + git add + git commit should work.
        std::fs::write(wt.join("test.txt"), "hello").unwrap();
        run_git(&wt, &["add", "test.txt"]);
        run_git_with_author(&wt, &["commit", "-m", "test commit"]);
        let log = git_output(&wt, &["log", "--oneline", "-1"]);
        assert!(log.contains("test commit"), "commit should be in log");

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

    // ─── E2E refinery helpers ─────────────────────────────────

    /// Create an in-memory DB with a task and merge request, returning (db, task_id, mr_id).
    fn setup_db_with_mr(branch: &str) -> (Db, Id, Id) {
        let db = Db::open_in_memory().unwrap();
        let task_id = Id::new("tsk");
        let now = Utc::now();
        db.insert_task(&Task {
            id: task_id.clone(),
            title: "test task".into(),
            description: None,
            status: TaskStatus::Running,
            assigned_to: None,
            worktree: None,
            branch: Some(branch.to_string()),
            tier: None,
            current_activity: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();

        let mr_id = Id::new("mr");
        db.insert_merge_request(&MergeRequest {
            id: mr_id.clone(),
            task_id: task_id.clone(),
            branch: branch.to_string(),
            base_branch: "main".to_string(),
            status: MergeStatus::Queued,
            priority: 0,
            diff_stats: None,
            review_note: None,
            execution_id: None,
            step_id: None,
            queued_at: now,
            started_at: None,
            merged_at: None,
        })
        .unwrap();

        (db, task_id, mr_id)
    }

    // ─── E2E: full pipeline tests ───────────────────────────

    #[test]
    fn e2e_worker_commits_merge_and_update_source() {
        let tmp = tmp_dir("e2e-merge");
        let (source, _bare, worktrees, mgr) = setup_all(&tmp);

        mgr.sync().unwrap();

        // Worker branches from origin/main, writes code, commits.
        let wt = mgr.create("task/worker-1", "origin/main", &worktrees).unwrap();
        std::fs::write(wt.join("feature.rs"), "fn feature() {}").unwrap();
        run_git(&wt, &["add", "."]);
        run_git_with_author(&wt, &["commit", "-m", "implement feature"]);

        // Create refinery worktree and run process_merge.
        let refinery_wt = mgr.create("refinery", "main", &worktrees).unwrap();
        let (db, _task_id, mr_id) = setup_db_with_mr("task/worker-1");

        let outcome = refinery::process_merge(&refinery_wt, "task/worker-1", "main", &db, &mr_id);
        assert!(
            matches!(outcome, MergeOutcome::Merged),
            "expected Merged, got {outcome:?}"
        );

        // Bare repo's main should contain worker's file.
        let verify = mgr.create("verify-main", "main", &worktrees).unwrap();
        assert!(
            verify.join("feature.rs").exists(),
            "bare repo main should contain worker's feature.rs"
        );
        assert_eq!(
            std::fs::read_to_string(verify.join("feature.rs")).unwrap(),
            "fn feature() {}"
        );

        // Pull into source working directory.
        mgr.update_source_workdir("main").unwrap();

        // Source should now have the worker's file.
        assert!(
            source.join("feature.rs").exists(),
            "source should have feature.rs after update_source_workdir"
        );
        assert_eq!(
            std::fs::read_to_string(source.join("feature.rs")).unwrap(),
            "fn feature() {}"
        );

        // Source's git log should include the worker's commit.
        let log = git_output(&source, &["log", "--oneline"]);
        assert!(
            log.contains("implement feature"),
            "source git log should contain worker's commit message"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn e2e_empty_worker_rejected_by_refinery() {
        let tmp = tmp_dir("e2e-empty");
        let (_source, bare, worktrees, mgr) = setup_all(&tmp);

        mgr.sync().unwrap();

        // Record main's commit before merge attempt.
        let main_before = git_output_bare(&bare, &["rev-parse", "main"]);

        // Worker branches from origin/main but does NOT commit anything.
        let _wt = mgr.create("task/empty-worker", "origin/main", &worktrees).unwrap();

        // Refinery should reject this.
        let refinery_wt = mgr.create("refinery", "main", &worktrees).unwrap();
        let (db, _task_id, mr_id) = setup_db_with_mr("task/empty-worker");

        let outcome =
            refinery::process_merge(&refinery_wt, "task/empty-worker", "main", &db, &mr_id);
        assert!(
            matches!(outcome, MergeOutcome::Failed(ref msg) if msg.contains("no file changes")),
            "expected Failed with 'no file changes', got {outcome:?}"
        );

        // Bare repo's main should be unchanged.
        let main_after = git_output_bare(&bare, &["rev-parse", "main"]);
        assert_eq!(main_before, main_after, "main should not move on empty worker");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn e2e_dirty_source_snapshot_does_not_create_false_merge() {
        let tmp = tmp_dir("e2e-snapshot");
        let (source, bare, worktrees, mgr) = setup_all(&tmp);

        // Source has dirty (uncommitted) files.
        std::fs::write(source.join("dirty.txt"), "uncommitted work").unwrap();

        // Sync — this creates a snapshot commit on origin/main that differs
        // from local main (origin/main has dirty.txt, local main does not).
        mgr.sync().unwrap();

        // Verify origin/main diverged from local main.
        let local_main = git_output_bare(&bare, &["rev-parse", "main"]);
        let origin_main = git_output_bare(&bare, &["rev-parse", "origin/main"]);
        assert_ne!(
            local_main, origin_main,
            "snapshot should make origin/main diverge from local main"
        );

        // Worker branches from origin/main (includes snapshot) but does NOT commit.
        let _wt = mgr.create("task/snapshot-worker", "origin/main", &worktrees).unwrap();

        // Refinery should reject — the branch has the snapshot commit but no
        // actual worker file changes vs origin/main.
        let refinery_wt = mgr.create("refinery", "main", &worktrees).unwrap();
        let (db, _task_id, mr_id) = setup_db_with_mr("task/snapshot-worker");

        let outcome =
            refinery::process_merge(&refinery_wt, "task/snapshot-worker", "main", &db, &mr_id);
        assert!(
            matches!(outcome, MergeOutcome::Failed(ref msg) if msg.contains("no file changes")),
            "snapshot-only branch should be rejected, got {outcome:?}"
        );

        // Local main should NOT have moved to the snapshot commit.
        let main_after = git_output_bare(&bare, &["rev-parse", "main"]);
        assert_eq!(
            local_main, main_after,
            "local main must not move to snapshot commit"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn e2e_multiple_workers_sequential_merge() {
        let tmp = tmp_dir("e2e-multi");
        let (source, _bare, worktrees, mgr) = setup_all(&tmp);

        mgr.sync().unwrap();

        // Worker 1 commits file A.
        let wt1 = mgr.create("task/worker-1", "origin/main", &worktrees).unwrap();
        std::fs::write(wt1.join("file_a.rs"), "// file A").unwrap();
        run_git(&wt1, &["add", "."]);
        run_git_with_author(&wt1, &["commit", "-m", "add file A"]);

        let refinery_wt = mgr.create("refinery", "main", &worktrees).unwrap();
        let (db, _, mr_id1) = setup_db_with_mr("task/worker-1");

        let outcome1 = refinery::process_merge(&refinery_wt, "task/worker-1", "main", &db, &mr_id1);
        assert!(matches!(outcome1, MergeOutcome::Merged), "worker 1: {outcome1:?}");

        // Worker 2 commits file B.
        let wt2 = mgr.create("task/worker-2", "origin/main", &worktrees).unwrap();
        std::fs::write(wt2.join("file_b.rs"), "// file B").unwrap();
        run_git(&wt2, &["add", "."]);
        run_git_with_author(&wt2, &["commit", "-m", "add file B"]);

        let (db2, _, mr_id2) = setup_db_with_mr("task/worker-2");

        let outcome2 =
            refinery::process_merge(&refinery_wt, "task/worker-2", "main", &db2, &mr_id2);
        assert!(matches!(outcome2, MergeOutcome::Merged), "worker 2: {outcome2:?}");

        // Pull into source.
        mgr.update_source_workdir("main").unwrap();

        // Source should have both files.
        assert!(source.join("file_a.rs").exists(), "source should have file A");
        assert!(source.join("file_b.rs").exists(), "source should have file B");
        assert_eq!(
            std::fs::read_to_string(source.join("file_a.rs")).unwrap(),
            "// file A"
        );
        assert_eq!(
            std::fs::read_to_string(source.join("file_b.rs")).unwrap(),
            "// file B"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn e2e_source_dirty_state_roundtrip() {
        let tmp = tmp_dir("e2e-dirty-rt");
        let (source, _bare, worktrees, mgr) = setup_all(&tmp);

        // Source has uncommitted changes (modified + untracked).
        std::fs::write(source.join("README.md"), "# user edits in progress").unwrap();
        std::fs::write(source.join("notes.txt"), "user notes").unwrap();

        // Sync snapshots the dirty state.
        mgr.sync().unwrap();

        // Worker branches, writes code, commits.
        let wt = mgr.create("task/worker-1", "origin/main", &worktrees).unwrap();
        // Worker should see the dirty state.
        assert_eq!(
            std::fs::read_to_string(wt.join("README.md")).unwrap(),
            "# user edits in progress",
            "worker should see user's uncommitted edits"
        );
        assert!(
            wt.join("notes.txt").exists(),
            "worker should see user's untracked files"
        );

        // Worker adds its own file.
        std::fs::write(wt.join("worker_output.rs"), "fn worker() {}").unwrap();
        run_git(&wt, &["add", "."]);
        run_git_with_author(&wt, &["commit", "-m", "worker output"]);

        // Merge via refinery.
        let refinery_wt = mgr.create("refinery", "main", &worktrees).unwrap();
        let (db, _, mr_id) = setup_db_with_mr("task/worker-1");

        let outcome = refinery::process_merge(&refinery_wt, "task/worker-1", "main", &db, &mr_id);
        assert!(matches!(outcome, MergeOutcome::Merged), "merge: {outcome:?}");

        // Pull into source.
        mgr.update_source_workdir("main").unwrap();

        // Source should have worker's new file.
        assert!(
            source.join("worker_output.rs").exists(),
            "source should have worker's output"
        );

        // Source should preserve user's uncommitted changes.
        assert_eq!(
            std::fs::read_to_string(source.join("README.md")).unwrap(),
            "# user edits in progress",
            "user's uncommitted edits should be preserved"
        );
        assert_eq!(
            std::fs::read_to_string(source.join("notes.txt")).unwrap(),
            "user notes",
            "user's untracked files should be preserved"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    // ─── update_source_workdir ───────────────────────────────

    #[test]
    fn update_source_workdir_pulls_merged_work() {
        let tmp = tmp_dir("update-src");
        let (source, _, worktrees, mgr) = setup_all(&tmp);

        // Worker makes a change and merges into bare's main.
        let wt = mgr.create("task/worker-1", "origin/main", &worktrees).unwrap();
        std::fs::write(wt.join("feature.rs"), "// new feature").unwrap();
        run_git(&wt, &["add", "."]);
        run_git_with_author(&wt, &["commit", "-m", "add feature"]);
        mgr.merge_branch("task/worker-1", "main").unwrap();

        // Source should NOT have feature.rs yet.
        assert!(!source.join("feature.rs").exists());

        // Pull merged work into source.
        mgr.update_source_workdir("main").unwrap();

        // Source should now have feature.rs.
        assert!(source.join("feature.rs").exists());
        assert_eq!(
            std::fs::read_to_string(source.join("feature.rs")).unwrap(),
            "// new feature"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn update_source_workdir_preserves_dirty_state() {
        let tmp = tmp_dir("update-dirty");
        let (source, _, worktrees, mgr) = setup_all(&tmp);

        // Worker makes a change and merges.
        let wt = mgr.create("task/worker-1", "origin/main", &worktrees).unwrap();
        std::fs::write(wt.join("feature.rs"), "// new feature").unwrap();
        run_git(&wt, &["add", "."]);
        run_git_with_author(&wt, &["commit", "-m", "add feature"]);
        mgr.merge_branch("task/worker-1", "main").unwrap();

        // User has uncommitted work in source.
        std::fs::write(source.join("README.md"), "# user edits").unwrap();
        std::fs::write(source.join("scratch.txt"), "user notes").unwrap();

        // Pull merged work.
        mgr.update_source_workdir("main").unwrap();

        // Merged work should be present.
        assert!(source.join("feature.rs").exists());

        // User's uncommitted changes should be restored.
        assert_eq!(
            std::fs::read_to_string(source.join("README.md")).unwrap(),
            "# user edits"
        );
        assert_eq!(
            std::fs::read_to_string(source.join("scratch.txt")).unwrap(),
            "user notes"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }
}
