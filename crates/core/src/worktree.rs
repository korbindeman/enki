use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("git command failed: {0}")]
    Git(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("merge conflict: {0}")]
    Conflict(String),
}

pub type Result<T> = std::result::Result<T, WorktreeError>;

/// Manages APFS copy-on-write clones of the project for worker isolation.
///
/// Each worker gets `cp -Rc <project_root> .enki/copies/<task_id>` — an instant
/// filesystem clone that includes everything (build artifacts, .gitignored files,
/// node_modules, etc.). Git is only used to commit changes at task completion
/// and merge them back into the source repo.
pub struct CopyManager {
    project_root: PathBuf,
    copies_dir: PathBuf,
}

impl CopyManager {
    pub fn new(project_root: PathBuf, copies_dir: PathBuf) -> Self {
        Self {
            project_root,
            copies_dir,
        }
    }

    /// Create an APFS clone of the project for a worker.
    ///
    /// 1. `cp -Rc <project_root> .enki/copies/<task_id>`
    /// 2. Remove `.enki/` from the copy (avoid nested copies + DB conflicts)
    /// 3. `git checkout -b task/<task_id>` inside the copy
    pub fn create_copy(&self, task_id: &str) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.copies_dir)?;

        let copy_path = self.copies_dir.join(task_id);
        if copy_path.exists() {
            std::fs::remove_dir_all(&copy_path)?;
        }

        // APFS clone — instant, zero extra space on macOS (copy-on-write).
        let output = Command::new("cp")
            .args(["-Rc"])
            .arg(&self.project_root)
            .arg(&copy_path)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::Git(format!("cp -Rc failed: {stderr}")));
        }

        // Remove .enki/ from copy to avoid nested DB/copies.
        let nested_enki = copy_path.join(".enki");
        if nested_enki.exists() {
            std::fs::remove_dir_all(&nested_enki)?;
        }

        // Create a new branch for the worker's changes.
        let branch = format!("task/{task_id}");
        let output = Command::new("git")
            .args(["checkout", "-b", &branch])
            .current_dir(&copy_path)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Clean up on failure.
            let _ = std::fs::remove_dir_all(&copy_path);
            return Err(WorktreeError::Git(format!(
                "git checkout -b {branch} failed: {stderr}"
            )));
        }

        Ok(copy_path)
    }

    /// Commit all changes in a copy. Returns true if a commit was created.
    ///
    /// Workers may finish without committing. This captures their work so
    /// we can merge it back.
    pub fn commit_copy(&self, copy_path: &Path, message: &str) -> bool {
        // Check for dirty state.
        let status = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(copy_path)
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
            .current_dir(copy_path)
            .output();
        if add.is_err() || !add.unwrap().status.success() {
            tracing::warn!(copy = %copy_path.display(), "auto-commit: git add -A failed");
            return false;
        }

        let commit = Command::new("git")
            .args(["commit", "-m", message, "--no-verify"])
            .env("GIT_AUTHOR_NAME", "enki")
            .env("GIT_AUTHOR_EMAIL", "enki@local")
            .env("GIT_COMMITTER_NAME", "enki")
            .env("GIT_COMMITTER_EMAIL", "enki@local")
            .current_dir(copy_path)
            .output();
        match commit {
            Ok(ref out) if out.status.success() => {
                tracing::info!(copy = %copy_path.display(), "auto-committed worker changes");
                true
            }
            Ok(ref out) => {
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

    /// Check if a copy has any file changes vs the branch point.
    ///
    /// Uses `git diff --stat` between the merge-base (where the branch forked
    /// from the default branch) and HEAD. If there are any changed files, the
    /// worker produced output.
    pub fn has_changes(&self, copy_path: &Path, _branch: &str) -> bool {
        // Find the default branch to compare against.
        let default = self.default_branch().unwrap_or_else(|_| "main".into());

        // Use diff --stat to check for file changes between merge-base and HEAD.
        let output = Command::new("git")
            .args(["diff", "--stat", &default, "HEAD"])
            .current_dir(copy_path)
            .output();
        match output {
            Ok(out) => !String::from_utf8_lossy(&out.stdout).trim().is_empty(),
            Err(_) => false,
        }
    }

    /// Remove a copy directory.
    pub fn remove_copy(&self, copy_path: &Path) -> Result<()> {
        if copy_path.exists() {
            std::fs::remove_dir_all(copy_path)?;
        }
        Ok(())
    }

    /// Detect the default branch name (main or master) in the source repo.
    pub fn default_branch(&self) -> Result<String> {
        for candidate in &["main", "master"] {
            let output = Command::new("git")
                .args(["rev-parse", "--verify", candidate])
                .current_dir(&self.project_root)
                .output();
            if let Ok(out) = output {
                if out.status.success() {
                    return Ok(candidate.to_string());
                }
            }
        }
        Err(WorktreeError::Git(
            "no default branch found (tried main, master)".into(),
        ))
    }

    /// Fetch a worker's branch from a copy into the source repo.
    ///
    /// Runs `git fetch <copy_path> <branch>:<branch>` in the source repo,
    /// making the worker's commits available for merging.
    pub fn fetch_branch(&self, copy_path: &Path, branch: &str) -> Result<()> {
        let copy_str = copy_path.to_string_lossy();
        let output = Command::new("git")
            .args(["fetch", &copy_str, &format!("{branch}:{branch}")])
            .current_dir(&self.project_root)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::Git(format!(
                "fetch from copy failed: {stderr}"
            )));
        }

        Ok(())
    }

    /// Delete a branch from the source repo.
    pub fn delete_branch(&self, branch: &str) -> Result<()> {
        let output = Command::new("git")
            .args(["branch", "-D", branch])
            .current_dir(&self.project_root)
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!("failed to delete branch {branch}: {stderr}");
        }
        Ok(())
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("enki-{prefix}-{}", ulid::Ulid::new()))
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
        let mgr = CopyManager::new(source.clone(), copies);
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

        let copy = mgr.create_copy("task-01").unwrap();

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

        let copy = mgr.create_copy("task-02").unwrap();

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

        let copy = mgr.create_copy("task-03").unwrap();
        let branch = "task/task-03";

        // No changes yet.
        assert!(!mgr.has_changes(&copy, branch));

        // Worker writes and commits.
        std::fs::write(copy.join("output.txt"), "worker output").unwrap();
        mgr.commit_copy(&copy, "worker output");

        assert!(mgr.has_changes(&copy, branch));

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn remove_copy_cleans_up() {
        let tmp = tmp_dir("copy-remove");
        let (_source, mgr) = setup_copy_manager(&tmp);

        let copy = mgr.create_copy("task-04").unwrap();
        assert!(copy.exists());

        mgr.remove_copy(&copy).unwrap();
        assert!(!copy.exists());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn default_branch_detection() {
        let tmp = tmp_dir("copy-branch");
        let (_source, mgr) = setup_copy_manager(&tmp);

        let branch = mgr.default_branch().unwrap();
        assert_eq!(branch, "main");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn fetch_branch_brings_changes_to_source() {
        let tmp = tmp_dir("copy-fetch");
        let (source, mgr) = setup_copy_manager(&tmp);

        let copy = mgr.create_copy("task-05").unwrap();

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
        let copy1 = mgr.create_copy("task-w1").unwrap();
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
        let copy2 = mgr.create_copy("task-w2").unwrap();
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

        let copy = mgr.create_copy("task-dirty").unwrap();

        // Copy should see the dirty state.
        assert_eq!(
            std::fs::read_to_string(copy.join("README.md")).unwrap(),
            "# modified but not committed"
        );
        assert!(copy.join("notes.txt").exists());

        std::fs::remove_dir_all(&tmp).ok();
    }
}
