use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum CopyError {
    #[error("git command failed: {0}")]
    Git(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("merge conflict: {0}")]
    Conflict(String),
}

pub type Result<T> = std::result::Result<T, CopyError>;

/// The user's git identity, read once from `git config`.
#[derive(Debug, Clone)]
pub struct GitIdentity {
    pub name: String,
    pub email: String,
}

impl GitIdentity {
    /// Read `user.name` and `user.email` from git config in the given directory.
    pub fn from_git_config(dir: &Path) -> Result<Self> {
        let name = git_config_get(dir, "user.name")?;
        let email = git_config_get(dir, "user.email")?;
        Ok(Self { name, email })
    }

    /// Apply this identity as env vars on a `Command`.
    pub fn apply<'a>(&self, cmd: &'a mut Command) -> &'a mut Command {
        cmd.env("GIT_AUTHOR_NAME", &self.name)
            .env("GIT_AUTHOR_EMAIL", &self.email)
            .env("GIT_COMMITTER_NAME", &self.name)
            .env("GIT_COMMITTER_EMAIL", &self.email)
    }
}

fn git_config_get(dir: &Path, key: &str) -> Result<String> {
    git(dir, &["config", key]).map_err(|_| {
        CopyError::Git(format!(
            "git config {key} not set — configure it with: git config --global {key} <value>"
        ))
    })
}

/// Run a git command in `dir`, returning trimmed stdout on success.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| CopyError::Git(format!("git {}: {e}", args.first().unwrap_or(&""))))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(CopyError::Git(format!(
            "git {} failed: {stderr}",
            args.join(" ")
        )))
    }
}

/// Like `git()` but returns `None` instead of `Err` on failure.
fn git_ok(dir: &Path, args: &[&str]) -> Option<String> {
    git(dir, args).ok()
}

/// Get the current HEAD sha of a repo, or None if unborn/not a repo.
pub fn head_sha(dir: &Path) -> Option<String> {
    git_ok(dir, &["rev-parse", "HEAD"])
}

/// Collect all top-level symlinks in a directory and remove them.
/// Returns a list of `(name, target)` pairs for later restoration.
#[cfg(unix)]
fn hide_symlinks(dir: &Path) -> Vec<(std::ffi::OsString, PathBuf)> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut saved = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_symlink() {
            if let Ok(target) = std::fs::read_link(&path) {
                if std::fs::remove_file(&path).is_ok() {
                    saved.push((entry.file_name(), target));
                }
            }
        }
    }
    saved
}

/// Restore previously removed symlinks.
#[cfg(unix)]
fn restore_symlinks(dir: &Path, saved: &[(std::ffi::OsString, PathBuf)]) {
    for (name, target) in saved {
        let link_path = dir.join(name);
        if let Err(e) = std::os::unix::fs::symlink(target, &link_path) {
            tracing::warn!(
                link = %link_path.display(),
                target = %target.display(),
                error = %e,
                "failed to restore symlink after staging"
            );
        }
    }
}

/// Manages git worktrees for worker isolation.
///
/// Each worker gets a worktree at `.enki/copies/<task_id>` that shares the
/// source repo's `.git` object store. Top-level gitignored directories
/// (build caches like `target/`, `node_modules/`) are symlinked from the
/// source project for fast access without copying.
pub struct CopyManager {
    project_root: PathBuf,
    copies_dir: PathBuf,
    git_identity: GitIdentity,
}

impl CopyManager {
    pub fn new(project_root: PathBuf, copies_dir: PathBuf, git_identity: GitIdentity) -> Self {
        Self {
            project_root,
            copies_dir,
            git_identity,
        }
    }

    /// Create a git worktree for a worker.
    ///
    /// 1. Record the current HEAD commit and branch name
    /// 2. `git worktree add .enki/copies/<task_id> -b task/<task_id>`
    /// 3. Symlink top-level gitignored directories from the source
    ///
    /// Returns `(worktree_path, base_commit, base_branch)`.
    pub fn create_copy(&self, task_id: &str) -> Result<(PathBuf, Option<String>, String)> {
        let base_commit = git_ok(&self.project_root, &["rev-parse", "HEAD"]);
        let base_branch = self.current_branch()?;

        std::fs::create_dir_all(&self.copies_dir)?;

        let copy_path = self.copies_dir.join(task_id);
        let branch = format!("task/{task_id}");

        // Clean up stale worktree from a prior crashed session.
        if copy_path.exists() {
            let _ = git(
                &self.project_root,
                &["worktree", "remove", "--force", &copy_path.to_string_lossy()],
            );
            if copy_path.exists() {
                std::fs::remove_dir_all(&copy_path)?;
            }
            let _ = git(&self.project_root, &["worktree", "prune"]);
        }

        // Delete stale branch if it exists from a prior run.
        let _ = git(&self.project_root, &["branch", "-D", &branch]);

        // Create worktree with a new branch from HEAD.
        if let Err(e) = git(
            &self.project_root,
            &[
                "worktree",
                "add",
                &copy_path.to_string_lossy(),
                "-b",
                &branch,
            ],
        ) {
            return Err(CopyError::Git(format!("worktree add failed: {e}")));
        }

        // Symlink gitignored directories for build caches (non-fatal).
        if let Err(e) = self.symlink_gitignored(&copy_path) {
            tracing::warn!(error = %e, "failed to symlink gitignored dirs into worktree");
        }

        Ok((copy_path, base_commit, base_branch))
    }

    /// Symlink top-level gitignored directories from the source into a worktree.
    ///
    /// Uses `git ls-files` to discover which directories are gitignored, then
    /// creates symlinks so workers can access build caches without copying them.
    fn symlink_gitignored(&self, worktree_path: &Path) -> Result<()> {
        let output = Command::new("git")
            .args([
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
                "--no-empty-directory",
            ])
            .current_dir(&self.project_root)
            .output()
            .map_err(|e| CopyError::Git(format!("ls-files: {e}")))?;

        if !output.status.success() {
            return Ok(());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let entry = line.trim_end_matches('/');
            if entry.is_empty() || entry.contains('/') {
                continue;
            }

            let source_path = self.project_root.join(entry);
            let target_path = worktree_path.join(entry);

            if source_path.is_dir() && !target_path.exists() {
                #[cfg(unix)]
                if let Err(e) = std::os::unix::fs::symlink(&source_path, &target_path) {
                    tracing::warn!(
                        source = %source_path.display(),
                        target = %target_path.display(),
                        error = %e,
                        "failed to symlink gitignored dir"
                    );
                }
            }
        }
        Ok(())
    }

    /// Commit all changes in a worktree. Returns true if a commit was created.
    ///
    /// Temporarily removes top-level symlinks before staging so that content
    /// inside symlinked gitignored directories (e.g. `target/`, `node_modules/`)
    /// is never committed.
    pub fn commit_copy(&self, copy_path: &Path, message: &str) -> bool {
        let status = git_ok(copy_path, &["status", "--porcelain"]);
        let has_dirty = status.as_ref().is_some_and(|s| !s.is_empty());
        if !has_dirty {
            return false;
        }

        // Hide symlinked dirs so `git add -A` doesn't stage their contents.
        let saved_symlinks = hide_symlinks(copy_path);

        let add_ok = git(copy_path, &["add", "-A"]).is_ok();

        // Always restore symlinks, even if git add failed.
        restore_symlinks(copy_path, &saved_symlinks);

        if !add_ok {
            tracing::warn!(copy = %copy_path.display(), "auto-commit: git add -A failed");
            return false;
        }

        let mut cmd = Command::new("git");
        cmd.args(["commit", "-m", message, "--no-verify"]);
        self.git_identity.apply(&mut cmd);
        match cmd.current_dir(copy_path).output() {
            Ok(out) if out.status.success() => {
                tracing::info!(copy = %copy_path.display(), "auto-committed worker changes");
                true
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                tracing::warn!(copy = %copy_path.display(), stderr = %stderr, "auto-commit failed");
                false
            }
            Err(e) => {
                tracing::warn!(copy = %copy_path.display(), error = %e, "auto-commit failed");
                false
            }
        }
    }

    /// Check if a worktree has any file changes vs the base commit.
    pub fn has_changes(&self, copy_path: &Path, base_commit: Option<&str>) -> bool {
        match base_commit {
            Some(base) => git_ok(copy_path, &["diff", "--stat", base, "HEAD"])
                .is_some_and(|s| !s.is_empty()),
            None => git_ok(copy_path, &["rev-parse", "HEAD"]).is_some(),
        }
    }

    /// Remove a worktree, its branch, and prune stale metadata.
    pub fn remove_copy(&self, copy_path: &Path) -> Result<()> {
        // Derive branch name from the directory name (e.g. "task-01" → "task/task-01").
        let branch = copy_path
            .file_name()
            .map(|n| format!("task/{}", n.to_string_lossy()));

        if copy_path.exists() {
            let _ = git(
                &self.project_root,
                &["worktree", "remove", "--force", &copy_path.to_string_lossy()],
            );
            // Fallback if git worktree remove didn't work.
            if copy_path.exists() {
                std::fs::remove_dir_all(copy_path)?;
                let _ = git(&self.project_root, &["worktree", "prune"]);
            }
        }

        // Always attempt branch cleanup, even if the directory was already gone.
        if let Some(branch) = branch {
            let _ = self.delete_branch(&branch);
        }

        Ok(())
    }

    /// Get the current branch name in the source repo.
    pub fn current_branch(&self) -> Result<String> {
        git(&self.project_root, &["symbolic-ref", "--short", "HEAD"]).map_err(|_| {
            CopyError::Git("HEAD is detached or unborn — cannot determine current branch".into())
        })
    }

    /// Delete a branch from the source repo.
    pub fn delete_branch(&self, branch: &str) -> Result<()> {
        if let Err(e) = git(&self.project_root, &["branch", "-D", branch]) {
            tracing::warn!("failed to delete branch {branch}: {e}");
        }
        Ok(())
    }

    /// Remove all worktrees in the copies directory.
    ///
    /// Called on session exit to clean up stale worker directories.
    pub fn cleanup_all_worktrees(&self) {
        if !self.copies_dir.exists() {
            return;
        }
        let entries = match std::fs::read_dir(&self.copies_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let name_str = entry.file_name();
            // Skip .merge-* temp dirs (handled by refinery cleanup).
            if name_str.to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                // remove_copy handles both worktree removal and branch deletion.
                let _ = self.remove_copy(&path);
            }
        }
        let _ = git(&self.project_root, &["worktree", "prune"]);
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// Remove orphaned `.merge-*` directories that are older than `max_age`.
    ///
    /// Merge temp dirs are created by the refinery for squash merges and conflict
    /// resolution. During active conflict resolution, `std::mem::forget` is used
    /// to keep the temp dir alive while the merger agent works. If the agent dies
    /// or the session is killed, the dir leaks. This method cleans them up.
    pub fn cleanup_orphaned_merge_dirs(&self, max_age: std::time::Duration) -> Vec<PathBuf> {
        let mut removed = Vec::new();
        if !self.copies_dir.exists() {
            return removed;
        }
        let entries = match std::fs::read_dir(&self.copies_dir) {
            Ok(e) => e,
            Err(_) => return removed,
        };
        let now = std::time::SystemTime::now();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.starts_with(".merge-") {
                continue;
            }
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            // Check modification time to avoid removing active merge dirs.
            let dominated = match path.metadata().and_then(|m| m.modified()) {
                Ok(mtime) => now.duration_since(mtime).unwrap_or_default() > max_age,
                Err(_) => true, // Can't read mtime — treat as stale.
            };
            if dominated {
                tracing::info!(dir = %path.display(), "removing orphaned merge temp dir");
                if std::fs::remove_dir_all(&path).is_ok() {
                    removed.push(path);
                } else {
                    tracing::warn!(dir = %path.display(), "failed to remove orphaned merge dir");
                }
            }
        }
        removed
    }

    pub fn copies_dir(&self) -> &Path {
        &self.copies_dir
    }

    pub fn git_identity(&self) -> &GitIdentity {
        &self.git_identity
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::RngExt;

    fn tmp_dir(prefix: &str) -> PathBuf {
        let bytes: [u8; 4] = rand::rng().random();
        let hex = format!("{:07x}", u32::from_be_bytes(bytes) >> 4);
        std::env::temp_dir().join(format!("enki-{prefix}-{hex}"))
    }

    fn setup_source(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        run_git(path, &["init"]);
        run_git(path, &["checkout", "-b", "main"]);
        std::fs::write(path.join("README.md"), "# test").unwrap();
        run_git(path, &["add", "."]);
        run_git_with_author(path, &["commit", "-m", "init"]);
    }

    fn setup_copy_manager(tmp: &Path) -> (PathBuf, CopyManager) {
        let source = tmp.join("source");
        let copies = tmp.join("copies");
        setup_source(&source);
        // Create .enki dir in source (simulates real project).
        std::fs::create_dir_all(source.join(".enki")).unwrap();
        std::fs::write(source.join(".enki/db.sqlite"), "fake db").unwrap();
        let identity = GitIdentity {
            name: "test".into(),
            email: "test@test.com".into(),
        };
        let mgr = CopyManager::new(source.clone(), copies, identity);
        (source, mgr)
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

    #[test]
    fn create_copy_produces_worktree() {
        let tmp = tmp_dir("copy-create");
        let (source, mgr) = setup_copy_manager(&tmp);

        // Add a .gitignored directory (should be symlinked, not copied).
        std::fs::write(source.join(".gitignore"), "build/\n").unwrap();
        run_git(&source, &["add", ".gitignore"]);
        run_git_with_author(&source, &["commit", "-m", "add gitignore"]);
        std::fs::create_dir_all(source.join("build")).unwrap();
        std::fs::write(source.join("build/output.bin"), "binary data").unwrap();

        let (copy, _base, _branch) = mgr.create_copy("task-01").unwrap();

        // Tracked files should exist.
        assert!(copy.join("README.md").exists());

        // .enki/ should NOT be in the worktree.
        assert!(!copy.join(".enki").exists());

        // Should be on a task branch.
        let branch = git_output(&copy, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(branch, "task/task-01");

        // Gitignored dir should be a symlink.
        let build_path = copy.join("build");
        assert!(build_path.is_symlink(), "build/ should be a symlink");
        assert_eq!(
            std::fs::read_to_string(build_path.join("output.bin")).unwrap(),
            "binary data"
        );

        // Branch should be visible in source repo.
        let branches = git_output(&source, &["branch", "--list", "task/task-01"]);
        assert!(branches.contains("task/task-01"));

        // .git should be a file (worktree), not a directory.
        let git_path = copy.join(".git");
        assert!(git_path.exists());
        assert!(git_path.is_file(), ".git should be a file in worktrees");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn commit_copy_captures_worker_changes() {
        let tmp = tmp_dir("copy-commit");
        let (_source, mgr) = setup_copy_manager(&tmp);

        let (copy, _base, _branch) = mgr.create_copy("task-02").unwrap();

        std::fs::write(copy.join("feature.rs"), "fn feature() {}").unwrap();

        let committed = mgr.commit_copy(&copy, "implement feature");
        assert!(committed);

        let log = git_output(&copy, &["log", "--oneline", "-1"]);
        assert!(log.contains("implement feature"));

        let committed_again = mgr.commit_copy(&copy, "no changes");
        assert!(!committed_again);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn commit_copy_excludes_symlinked_gitignored_dirs() {
        let tmp = tmp_dir("copy-symlink-exclude");
        let (source, mgr) = setup_copy_manager(&tmp);

        // Set up a gitignored directory.
        std::fs::write(source.join(".gitignore"), "build/\n").unwrap();
        run_git(&source, &["add", ".gitignore"]);
        run_git_with_author(&source, &["commit", "-m", "add gitignore"]);
        std::fs::create_dir_all(source.join("build")).unwrap();
        std::fs::write(source.join("build/output.bin"), "original").unwrap();

        let (copy, _base, _branch) = mgr.create_copy("task-sym").unwrap();
        assert!(copy.join("build").is_symlink());

        // Worker writes a tracked file AND modifies a file through the symlink.
        std::fs::write(copy.join("feature.rs"), "fn feature() {}").unwrap();
        std::fs::write(copy.join("build/output.bin"), "modified by worker").unwrap();

        let committed = mgr.commit_copy(&copy, "worker changes");
        assert!(committed);

        // The tracked file should be in the commit.
        let diff = git_output(&copy, &["diff", "--name-only", "HEAD~1", "HEAD"]);
        assert!(diff.contains("feature.rs"), "tracked file should be committed");

        // The symlinked gitignored file should NOT be in the commit.
        assert!(
            !diff.contains("build/"),
            "gitignored symlinked content should not be committed, got: {diff}"
        );

        // Symlink should still exist after commit.
        assert!(copy.join("build").is_symlink(), "symlink should be restored");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn remove_copy_deletes_branch() {
        let tmp = tmp_dir("copy-remove-branch");
        let (source, mgr) = setup_copy_manager(&tmp);

        let (copy, _base, _branch) = mgr.create_copy("task-rb").unwrap();
        let branches = git_output(&source, &["branch", "--list", "task/task-rb"]);
        assert!(branches.contains("task/task-rb"));

        mgr.remove_copy(&copy).unwrap();
        assert!(!copy.exists());

        let branches = git_output(&source, &["branch", "--list", "task/task-rb"]);
        assert!(branches.is_empty(), "branch should be deleted by remove_copy");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn has_changes_detects_worker_output() {
        let tmp = tmp_dir("copy-changes");
        let (_source, mgr) = setup_copy_manager(&tmp);

        let (copy, base, _branch) = mgr.create_copy("task-03").unwrap();

        assert!(!mgr.has_changes(&copy, base.as_deref()));

        std::fs::write(copy.join("output.txt"), "worker output").unwrap();
        mgr.commit_copy(&copy, "worker output");

        assert!(mgr.has_changes(&copy, base.as_deref()));

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn remove_copy_cleans_up() {
        let tmp = tmp_dir("copy-remove");
        let (_source, mgr) = setup_copy_manager(&tmp);

        let (copy, _base, _branch) = mgr.create_copy("task-04").unwrap();
        assert!(copy.exists());

        mgr.remove_copy(&copy).unwrap();
        assert!(!copy.exists());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn current_branch_detection() {
        let tmp = tmp_dir("copy-branch");
        let (_source, mgr) = setup_copy_manager(&tmp);

        let branch = mgr.current_branch().unwrap();
        assert_eq!(branch, "main");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn worktree_branch_visible_in_source() {
        let tmp = tmp_dir("copy-wt-branch");
        let (source, mgr) = setup_copy_manager(&tmp);

        let (copy, _base, _branch) = mgr.create_copy("task-05").unwrap();

        // Worker makes changes and commits.
        std::fs::write(copy.join("feature.rs"), "fn feature() {}").unwrap();
        run_git(&copy, &["add", "."]);
        run_git_with_author(&copy, &["commit", "-m", "implement feature"]);

        // Branch should already be in source (shared .git).
        let branches = git_output(&source, &["branch", "--list", "task/task-05"]);
        assert!(
            branches.contains("task/task-05"),
            "worktree branch should be visible in source"
        );

        // Shared clone should also see it.
        let clone_dir = tmp.join("clone");
        run_git(
            &source,
            &[
                "clone",
                "--shared",
                &source.to_string_lossy(),
                &clone_dir.to_string_lossy(),
            ],
        );
        let remote_branches = git_output(&clone_dir, &["branch", "-r"]);
        assert!(
            remote_branches.contains("origin/task/task-05"),
            "shared clone should see worktree branch as remote"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn e2e_worktree_workflow() {
        let tmp = tmp_dir("copy-e2e");
        let (source, mgr) = setup_copy_manager(&tmp);

        // Add gitignored build artifacts.
        std::fs::write(source.join(".gitignore"), "node_modules/\n").unwrap();
        run_git(&source, &["add", ".gitignore"]);
        run_git_with_author(&source, &["commit", "-m", "add gitignore"]);
        std::fs::create_dir_all(source.join("node_modules/pkg")).unwrap();
        std::fs::write(
            source.join("node_modules/pkg/index.js"),
            "module.exports = {}",
        )
        .unwrap();

        // Worker 1: create worktree, do work, commit.
        let (copy1, _base1, _branch1) = mgr.create_copy("task-w1").unwrap();
        // Gitignored dir should be symlinked.
        assert!(
            copy1.join("node_modules").is_symlink(),
            "node_modules should be a symlink"
        );
        assert!(
            copy1.join("node_modules/pkg/index.js").exists(),
            "worker should see node_modules contents via symlink"
        );
        std::fs::write(copy1.join("feature_a.rs"), "// feature A").unwrap();
        run_git(&copy1, &["add", "."]);
        run_git_with_author(&copy1, &["commit", "-m", "add feature A"]);

        // Branch already in source — merge directly.
        run_git(&source, &["merge", "task/task-w1", "--no-edit"]);
        assert!(source.join("feature_a.rs").exists());

        // Clean up.
        mgr.remove_copy(&copy1).unwrap();
        mgr.delete_branch("task/task-w1").unwrap();

        // Worker 2: sees worker 1's merged changes.
        let (copy2, _base2, _branch2) = mgr.create_copy("task-w2").unwrap();
        assert!(
            copy2.join("feature_a.rs").exists(),
            "worker 2 should see worker 1's merged changes"
        );
        std::fs::write(copy2.join("feature_b.rs"), "// feature B").unwrap();
        run_git(&copy2, &["add", "."]);
        run_git_with_author(&copy2, &["commit", "-m", "add feature B"]);

        run_git(&source, &["merge", "task/task-w2", "--no-edit"]);
        assert!(source.join("feature_a.rs").exists());
        assert!(source.join("feature_b.rs").exists());

        mgr.remove_copy(&copy2).unwrap();
        mgr.delete_branch("task/task-w2").unwrap();

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn stale_worktree_handled_on_recreate() {
        let tmp = tmp_dir("copy-stale");
        let (_source, mgr) = setup_copy_manager(&tmp);

        // Create a worktree.
        let (copy, _base, _branch) = mgr.create_copy("task-stale").unwrap();
        assert!(copy.exists());

        // Simulate crash: worktree dir exists but we try to create again.
        let (copy2, _base2, _branch2) = mgr.create_copy("task-stale").unwrap();
        assert!(copy2.exists());
        assert_eq!(copy, copy2);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn cleanup_all_worktrees_removes_everything() {
        let tmp = tmp_dir("copy-cleanup");
        let (source, mgr) = setup_copy_manager(&tmp);

        mgr.create_copy("task-a").unwrap();
        mgr.create_copy("task-b").unwrap();

        // Both worktrees exist.
        assert!(mgr.copies_dir().join("task-a").exists());
        assert!(mgr.copies_dir().join("task-b").exists());

        mgr.cleanup_all_worktrees();

        assert!(!mgr.copies_dir().join("task-a").exists());
        assert!(!mgr.copies_dir().join("task-b").exists());

        // Branches should be cleaned up too.
        let branches = git_output(&source, &["branch", "--list", "task/*"]);
        assert!(branches.is_empty(), "all task branches should be deleted");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn cleanup_orphaned_merge_dirs_removes_stale() {
        let tmp = tmp_dir("copy-merge-cleanup");
        let (_source, mgr) = setup_copy_manager(&tmp);

        let copies = mgr.copies_dir();
        std::fs::create_dir_all(copies).unwrap();

        // Create two .merge-* dirs — both are "stale" relative to max_age=0.
        let merge_a = copies.join(".merge-aaa");
        std::fs::create_dir_all(&merge_a).unwrap();
        std::fs::write(merge_a.join("dummy"), "data").unwrap();

        let merge_b = copies.join(".merge-bbb");
        std::fs::create_dir_all(&merge_b).unwrap();
        std::fs::write(merge_b.join("dummy"), "data").unwrap();

        // Create a regular worktree dir (should not be touched).
        let task_dir = copies.join("task-xyz");
        std::fs::create_dir_all(&task_dir).unwrap();

        // A hidden non-merge dir (should not be touched).
        let hidden_dir = copies.join(".other");
        std::fs::create_dir_all(&hidden_dir).unwrap();

        // With max_age=0, all .merge-* dirs are "stale".
        let removed = mgr.cleanup_orphaned_merge_dirs(std::time::Duration::ZERO);

        assert_eq!(removed.len(), 2, "should remove both .merge-* dirs");
        assert!(!merge_a.exists(), "merge dir a should be removed");
        assert!(!merge_b.exists(), "merge dir b should be removed");
        assert!(task_dir.exists(), "non-merge dir should be untouched");
        assert!(hidden_dir.exists(), "non-merge hidden dir should be untouched");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn cleanup_orphaned_merge_dirs_preserves_recent() {
        let tmp = tmp_dir("copy-merge-recent");
        let (_source, mgr) = setup_copy_manager(&tmp);

        let copies = mgr.copies_dir();
        std::fs::create_dir_all(copies).unwrap();

        // Create a fresh .merge-* dir.
        let merge_dir = copies.join(".merge-fresh");
        std::fs::create_dir_all(&merge_dir).unwrap();

        // With max_age=1h, a just-created dir should be preserved.
        let removed = mgr.cleanup_orphaned_merge_dirs(std::time::Duration::from_secs(3600));

        assert!(removed.is_empty(), "fresh merge dir should not be removed");
        assert!(merge_dir.exists(), "fresh merge dir should still exist");

        std::fs::remove_dir_all(&tmp).ok();
    }
}
