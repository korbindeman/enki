use std::process::Command;

use enki_core::db::Db;
use enki_core::copy::GitIdentity;

pub async fn init() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let enki_dir = cwd.join(".enki");
    let db_path = enki_dir.join("db.sqlite");
    let copies_dir = enki_dir.join("copies");

    if db_path.exists() {
        println!("already initialized at {}", enki_dir.display());
        return Ok(());
    }

    std::fs::create_dir_all(&enki_dir)?;
    std::fs::create_dir_all(&copies_dir)?;

    // Ignore everything inside .enki/ except this .gitignore itself
    std::fs::write(enki_dir.join(".gitignore"), "*\n!.gitignore\n")?;

    Db::open(db_path.to_str().unwrap())?;

    // If the repo has no commits (unborn HEAD), create an initial commit so
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
        cmd.args(["commit", "--allow-empty", "-m", "enki: initialize project", "--no-verify"]);
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
