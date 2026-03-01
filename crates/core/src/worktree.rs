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
}

pub type Result<T> = std::result::Result<T, WorktreeError>;

#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: String,
    pub is_bare: bool,
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
    pub fn create(&self, branch: &str, base_ref: &str, worktree_dir: &Path) -> Result<PathBuf> {
        let worktree_path = worktree_dir.join(branch);

        let output = Command::new("git")
            .args(["worktree", "add", "-b", branch])
            .arg(&worktree_path)
            .arg(base_ref)
            .env("GIT_DIR", &self.bare_repo)
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

    /// Initialize a bare repo from an existing repo.
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

        assert_eq!(
            result[1].path,
            PathBuf::from("/tmp/worktrees/task/feature-auth")
        );
        assert_eq!(result[1].branch, "task/feature-auth");
        assert!(!result[1].is_bare);

        assert_eq!(
            result[2].path,
            PathBuf::from("/tmp/worktrees/task/fix-login")
        );
        assert_eq!(result[2].branch, "task/fix-login");
    }

    #[test]
    fn bare_repo_not_found() {
        let result = WorktreeManager::new("/nonexistent/path");
        assert!(matches!(result, Err(WorktreeError::BareRepoNotFound(_))));
    }

    // Integration tests (require real git) are gated behind a feature or run manually.
    // They create temp dirs, init bare repos, add/remove worktrees.
    #[test]
    fn integration_worktree_lifecycle() {
        let tmp = std::env::temp_dir().join(format!("enki-wt-test-{}", ulid::Ulid::new()));
        let source = tmp.join("source");
        let bare = tmp.join("repo.git");
        let worktrees = tmp.join("worktrees");

        // Setup: create a source repo with an initial commit
        std::fs::create_dir_all(&source).unwrap();
        run_git(&source, &["init"]);
        run_git(&source, &["checkout", "-b", "main"]);
        std::fs::write(source.join("README.md"), "# test").unwrap();
        run_git(&source, &["add", "."]);
        run_git(&source, &["-c", "user.name=test", "-c", "user.email=test@test.com", "commit", "-m", "init"]);

        // Clone to bare
        WorktreeManager::init_bare(&source, &bare).unwrap();

        // Create worktree manager
        let mgr = WorktreeManager::new(&bare).unwrap();
        std::fs::create_dir_all(&worktrees).unwrap();

        // Create a worktree
        let wt_path = mgr.create("task/feature-1", "main", &worktrees).unwrap();
        assert!(wt_path.exists());
        assert!(wt_path.join("README.md").exists());

        // List worktrees
        let list = mgr.list().unwrap();
        // bare + our worktree
        assert!(list.len() >= 2);
        let wt = list.iter().find(|w| w.branch == "task/feature-1");
        assert!(wt.is_some());

        // Remove worktree
        mgr.remove(&wt_path, true).unwrap();
        assert!(!wt_path.exists());

        // Cleanup
        std::fs::remove_dir_all(&tmp).ok();
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
}
