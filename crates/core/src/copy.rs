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

/// Clone a single filesystem entry using platform-appropriate copy-on-write.
///
/// - macOS (APFS): `cp -Rc` — instant CoW via `clonefile(2)`
/// - Linux (btrfs/XFS): `cp --reflink=auto -a` — CoW where supported, regular copy otherwise
/// - Other: `cp -a` — regular recursive copy
fn clone_entry(src: &Path, dst: &Path) -> Result<()> {
    let output = if cfg!(target_os = "macos") {
        Command::new("cp")
            .args(["-Rc"])
            .arg(src)
            .arg(dst)
            .output()?
    } else if cfg!(target_os = "linux") {
        Command::new("cp")
            .args(["--reflink=auto", "-a"])
            .arg(src)
            .arg(dst)
            .output()?
    } else {
        Command::new("cp")
            .args(["-a"])
            .arg(src)
            .arg(dst)
            .output()?
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CopyError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("cp failed for {}: {stderr}", src.display()),
        )));
    }
    Ok(())
}

/// Get the current HEAD sha of a repo, or None if unborn/not a repo.
pub fn head_sha(dir: &Path) -> Option<String> {
    git_ok(dir, &["rev-parse", "HEAD"])
}

/// Manages copy-on-write clones of the project for worker isolation.
///
/// Each worker gets a clone of the project at `.enki/copies/<task_id>` that
/// includes everything (build artifacts, .gitignored files, node_modules, etc.).
/// Uses platform-appropriate CoW (APFS on macOS, reflink on Linux) for instant,
/// space-efficient clones. Git is only used to commit changes at task completion
/// and merge them back into the source repo.
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

    /// Create a copy-on-write clone of the project for a worker.
    ///
    /// 1. Record the current HEAD commit (the "base") and branch name
    /// 2. Clone each top-level entry except `.enki/` into a temp directory
    /// 3. Atomically rename the temp directory to the final path
    /// 4. `git checkout -b task/<task_id>` inside the copy
    ///
    /// Skipping `.enki/` avoids copying nested copies and the database.
    /// Using a temp directory + rename ensures the final path only appears
    /// atomically — a crash mid-copy won't leave a partial copy at the
    /// canonical path.
    ///
    /// Returns `(copy_path, base_commit, base_branch)`. `base_commit` is `None`
    /// for unborn repos. `base_branch` is the branch to merge back into.
    pub fn create_copy(&self, task_id: &str) -> Result<(PathBuf, Option<String>, String)> {
        let base_commit = git_ok(&self.project_root, &["rev-parse", "HEAD"]);
        let base_branch = self.current_branch()?;

        std::fs::create_dir_all(&self.copies_dir)?;

        let copy_path = self.copies_dir.join(task_id);
        if copy_path.exists() {
            std::fs::remove_dir_all(&copy_path)?;
        }

        // Build into a temp directory, then atomically rename.
        let tmp_path = self.copies_dir.join(format!(".tmp-{task_id}"));
        if tmp_path.exists() {
            std::fs::remove_dir_all(&tmp_path)?;
        }
        std::fs::create_dir_all(&tmp_path)?;

        // Clone each top-level entry except .enki/ — preserves CoW semantics
        // while skipping nested copies and the database.
        for entry in std::fs::read_dir(&self.project_root)? {
            let entry = entry?;
            let name = entry.file_name();
            if name == ".enki" {
                continue;
            }
            if let Err(e) = clone_entry(&entry.path(), &tmp_path.join(&name)) {
                let _ = std::fs::remove_dir_all(&tmp_path);
                return Err(e);
            }
        }

        // Atomic rename (same filesystem, instant).
        std::fs::rename(&tmp_path, &copy_path)?;

        // Create a new branch for the worker's changes.
        let branch = format!("task/{task_id}");
        if let Err(e) = git(&copy_path, &["checkout", "-b", &branch]) {
            let _ = std::fs::remove_dir_all(&copy_path);
            return Err(e);
        }

        Ok((copy_path, base_commit, base_branch))
    }

    /// Commit all changes in a copy. Returns true if a commit was created.
    ///
    /// Workers may finish without committing. This captures their work so
    /// we can merge it back.
    pub fn commit_copy(&self, copy_path: &Path, message: &str) -> bool {
        // Check for dirty state.
        let status = git_ok(copy_path, &["status", "--porcelain"]);
        let has_dirty = status.as_ref().is_some_and(|s| !s.is_empty());
        if !has_dirty {
            return false;
        }

        // Stage everything.
        if git(copy_path, &["add", "-A"]).is_err() {
            tracing::warn!(copy = %copy_path.display(), "auto-commit: git add -A failed");
            return false;
        }

        // Commit with identity.
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

    /// Check if a copy has any file changes vs the base commit.
    ///
    /// `base_commit` is the HEAD hash from when the copy was created. If `None`
    /// (unborn repo), any commit on the task branch counts as changes.
    pub fn has_changes(&self, copy_path: &Path, base_commit: Option<&str>) -> bool {
        match base_commit {
            Some(base) => git_ok(copy_path, &["diff", "--stat", base, "HEAD"])
                .is_some_and(|s| !s.is_empty()),
            None => git_ok(copy_path, &["rev-parse", "HEAD"]).is_some(),
        }
    }

    /// Remove a copy directory.
    pub fn remove_copy(&self, copy_path: &Path) -> Result<()> {
        if copy_path.exists() {
            std::fs::remove_dir_all(copy_path)?;
        }
        Ok(())
    }

    /// Get the current branch name in the source repo.
    ///
    /// Returns whatever branch HEAD points to — no assumptions about naming.
    /// This is the branch workers merge back into.
    pub fn current_branch(&self) -> Result<String> {
        git(&self.project_root, &["symbolic-ref", "--short", "HEAD"]).map_err(|_| {
            CopyError::Git("HEAD is detached or unborn — cannot determine current branch".into())
        })
    }

    /// Fetch a worker's branch from a copy into the source repo.
    ///
    /// Runs `git fetch <copy_path> <branch>:<branch>` in the source repo,
    /// making the worker's commits available for merging.
    pub fn fetch_branch(&self, copy_path: &Path, branch: &str) -> Result<()> {
        let copy_str = copy_path.to_string_lossy();
        git(
            &self.project_root,
            &["fetch", &copy_str, &format!("{branch}:{branch}")],
        )?;
        Ok(())
    }

    /// Delete a branch from the source repo.
    pub fn delete_branch(&self, branch: &str) -> Result<()> {
        if let Err(e) = git(&self.project_root, &["branch", "-D", branch]) {
            tracing::warn!("failed to delete branch {branch}: {e}");
        }
        Ok(())
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
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
    use rand::Rng;

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
        // Create .enki dir in source (simulates real project)
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
    fn create_copy_produces_full_filesystem_clone() {
        let tmp = tmp_dir("copy-create");
        let (source, mgr) = setup_copy_manager(&tmp);

        // Add a .gitignored file (this is the whole point — workers should see it)
        std::fs::write(source.join(".gitignore"), "build/\n").unwrap();
        run_git(&source, &["add", ".gitignore"]);
        run_git_with_author(&source, &["commit", "-m", "add gitignore"]);
        std::fs::create_dir_all(source.join("build")).unwrap();
        std::fs::write(source.join("build/output.bin"), "binary data").unwrap();

        let (copy, _base, _branch) = mgr.create_copy("task-01").unwrap();

        // Copy should have all files including .gitignored ones.
        assert!(copy.join("README.md").exists());
        assert!(copy.join("build/output.bin").exists());
        assert_eq!(
            std::fs::read_to_string(copy.join("build/output.bin")).unwrap(),
            "binary data"
        );

        // .enki/ should NOT be in the copy.
        assert!(!copy.join(".enki").exists());

        // Should be on a task branch.
        let branch = git_output(&copy, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(branch, "task/task-01");

        // git status should work.
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&copy)
            .output()
            .unwrap();
        assert!(output.status.success());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn commit_copy_captures_worker_changes() {
        let tmp = tmp_dir("copy-commit");
        let (_source, mgr) = setup_copy_manager(&tmp);

        let (copy, _base, _branch) = mgr.create_copy("task-02").unwrap();

        // Worker makes changes.
        std::fs::write(copy.join("feature.rs"), "fn feature() {}").unwrap();

        // Commit should succeed.
        let committed = mgr.commit_copy(&copy, "implement feature");
        assert!(committed);

        // Verify commit is in log.
        let log = git_output(&copy, &["log", "--oneline", "-1"]);
        assert!(log.contains("implement feature"));

        // Second commit_copy with no changes should return false.
        let committed_again = mgr.commit_copy(&copy, "no changes");
        assert!(!committed_again);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn has_changes_detects_worker_output() {
        let tmp = tmp_dir("copy-changes");
        let (_source, mgr) = setup_copy_manager(&tmp);

        let (copy, base, _branch) = mgr.create_copy("task-03").unwrap();

        // No changes yet.
        assert!(!mgr.has_changes(&copy, base.as_deref()));

        // Worker writes and commits.
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
    fn fetch_branch_brings_changes_to_source() {
        let tmp = tmp_dir("copy-fetch");
        let (source, mgr) = setup_copy_manager(&tmp);

        let (copy, _base, _branch) = mgr.create_copy("task-05").unwrap();

        // Worker makes changes and commits.
        std::fs::write(copy.join("feature.rs"), "fn feature() {}").unwrap();
        run_git(&copy, &["add", "."]);
        run_git_with_author(&copy, &["commit", "-m", "implement feature"]);

        // Fetch branch back to source.
        mgr.fetch_branch(&copy, "task/task-05").unwrap();

        // Source should now have the branch.
        let branches = git_output(&source, &["branch", "--list", "task/task-05"]);
        assert!(
            branches.contains("task/task-05"),
            "source should have the fetched branch"
        );

        // Checkout and verify.
        run_git(&source, &["checkout", "task/task-05"]);
        assert!(source.join("feature.rs").exists());
        assert_eq!(
            std::fs::read_to_string(source.join("feature.rs")).unwrap(),
            "fn feature() {}"
        );

        // Switch back to main.
        run_git(&source, &["checkout", "main"]);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn e2e_copy_based_workflow() {
        let tmp = tmp_dir("copy-e2e");
        let (source, mgr) = setup_copy_manager(&tmp);

        // Add some build artifacts that should be visible to workers.
        std::fs::write(source.join(".gitignore"), "node_modules/\n").unwrap();
        run_git(&source, &["add", ".gitignore"]);
        run_git_with_author(&source, &["commit", "-m", "add gitignore"]);
        std::fs::create_dir_all(source.join("node_modules/pkg")).unwrap();
        std::fs::write(
            source.join("node_modules/pkg/index.js"),
            "module.exports = {}",
        )
        .unwrap();

        // Worker 1: create copy, do work, commit.
        let (copy1, _base1, _branch1) = mgr.create_copy("task-w1").unwrap();
        assert!(
            copy1.join("node_modules/pkg/index.js").exists(),
            "worker should see node_modules"
        );
        std::fs::write(copy1.join("feature_a.rs"), "// feature A").unwrap();
        run_git(&copy1, &["add", "."]);
        run_git_with_author(&copy1, &["commit", "-m", "add feature A"]);

        // Fetch and merge into source.
        mgr.fetch_branch(&copy1, "task/task-w1").unwrap();
        run_git(&source, &["merge", "task/task-w1", "--no-edit"]);

        assert!(source.join("feature_a.rs").exists());

        // Clean up copy and branch.
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

        mgr.fetch_branch(&copy2, "task/task-w2").unwrap();
        run_git(&source, &["merge", "task/task-w2", "--no-edit"]);

        assert!(source.join("feature_a.rs").exists());
        assert!(source.join("feature_b.rs").exists());

        mgr.remove_copy(&copy2).unwrap();
        mgr.delete_branch("task/task-w2").unwrap();

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn uncommitted_source_changes_visible_in_copy() {
        let tmp = tmp_dir("copy-dirty");
        let (source, mgr) = setup_copy_manager(&tmp);

        // User has uncommitted changes.
        std::fs::write(source.join("README.md"), "# modified but not committed").unwrap();
        std::fs::write(source.join("notes.txt"), "user notes").unwrap();

        let (copy, _base, _branch) = mgr.create_copy("task-dirty").unwrap();

        // Copy should see the dirty state.
        assert_eq!(
            std::fs::read_to_string(copy.join("README.md")).unwrap(),
            "# modified but not committed"
        );
        assert!(copy.join("notes.txt").exists());

        std::fs::remove_dir_all(&tmp).ok();
    }
}
