use std::process::Command;

use enki_core::config::load_config;
use enki_core::db::Db;
use enki_core::copy::GitIdentity;

pub async fn init() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let config = load_config(&cwd);
    let enki_dir = cwd.join(".enki");
    let db_path = enki_dir.join("db.sqlite");
    let copies_dir = enki_dir.join("copies");

    if db_path.exists() {
        println!("already initialized at {}", enki_dir.display());
        return Ok(());
    }

    // Verify we're in a git repo.
    let is_git = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(&cwd)
        .output()
        .is_ok_and(|o| o.status.success());

    if !is_git {
        anyhow::bail!("enki requires a git repository — run `git init` first");
    }

    std::fs::create_dir_all(&enki_dir)?;
    std::fs::create_dir_all(&copies_dir)?;

    // Ignore everything inside .enki/ except this .gitignore itself.
    std::fs::write(enki_dir.join(".gitignore"), "*\n!.gitignore\n")?;

    Db::open(db_path.to_str().unwrap())?;

    // For repos with no commits (unborn HEAD), create an initial commit so
    // there's always a base to branch from and merge into.
    let head_check = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&cwd)
        .output();
    let is_unborn = match head_check {
        Ok(out) => !out.status.success(),
        Err(_) => true,
    };

    if is_unborn {
        let git_identity = GitIdentity::from_git_config(&cwd)?;
        let _ = Command::new("git")
            .args(["add", ".enki/.gitignore"])
            .current_dir(&cwd)
            .output();
        let mut cmd = Command::new("git");
        let commit_msg = if config.git.commit_suffix.is_empty() {
            "initialize project".to_string()
        } else {
            format!("initialize project\n\n{}", config.git.commit_suffix)
        };
        cmd.args(["commit", "--allow-empty", "-m", &commit_msg, "--no-verify"]);
        git_identity.apply(&mut cmd);
        let result = cmd.current_dir(&cwd).output();
        match result {
            Ok(out) if out.status.success() => {
                println!("created initial commit (repo was empty)");
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                anyhow::bail!("failed to create initial commit: {stderr}");
            }
            Err(e) => {
                anyhow::bail!("failed to create initial commit: {e}");
            }
        }
    }

    println!("Initialized enki at {}", enki_dir.display());
    Ok(())
}
