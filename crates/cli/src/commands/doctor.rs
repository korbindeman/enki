use std::process::Command;

use enki_core::worktree::WorktreeManager;

pub async fn doctor() -> anyhow::Result<()> {
    let mut ok = true;

    // 1. Check .enki/ directory
    match super::enki_dir() {
        Ok(dir) => check_pass(&format!(".enki directory: {}", dir.display())),
        Err(e) => {
            check_fail(&format!(".enki directory: {e}"));
            // Can't continue without .enki
            println!("\nRun `enki init` to initialize the project.");
            return Ok(());
        }
    }

    // 2. Check database
    match super::open_db() {
        Ok(db) => {
            let tasks = db.list_tasks().unwrap_or_default();
            check_pass(&format!("database: {} tasks", tasks.len()));
        }
        Err(e) => {
            check_fail(&format!("database: {e}"));
            ok = false;
        }
    }

    // 3. Check bare repo
    let bare_path = super::bare_path()?;
    if bare_path.exists() {
        check_pass(&format!("bare repo: {}", bare_path.display()));

        match WorktreeManager::new(&bare_path) {
            Ok(wt_mgr) => {
                // Check origin remote
                match wt_mgr.source_repo() {
                    Ok(source) => {
                        if source.exists() {
                            check_pass(&format!("origin remote: {}", source.display()));
                        } else {
                            check_fail(&format!("origin remote: {} (not found)", source.display()));
                            ok = false;
                        }
                    }
                    Err(e) => {
                        check_fail(&format!("origin remote: {e}"));
                        ok = false;
                    }
                }

                // Check default branch
                match wt_mgr.default_branch() {
                    Ok(branch) => check_pass(&format!("default branch: {branch}")),
                    Err(e) => {
                        check_fail(&format!("default branch: {e}"));
                        ok = false;
                    }
                }

                // Check fetch config
                let output = Command::new("git")
                    .args(["config", "--get", "remote.origin.fetch"])
                    .env("GIT_DIR", &bare_path)
                    .output();
                match output {
                    Ok(out) if out.status.success() => {
                        let refspec = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        if refspec.contains("refs/remotes/origin") {
                            check_pass(&format!("fetch refspec: {refspec}"));
                        } else {
                            check_warn(&format!(
                                "fetch refspec: {refspec} (expected refs/remotes/origin/* mapping)"
                            ));
                        }
                    }
                    _ => {
                        check_fail("fetch refspec: not configured");
                        ok = false;
                    }
                }

                // Check worktrees
                match wt_mgr.list() {
                    Ok(worktrees) => {
                        let non_bare: Vec<_> = worktrees.iter().filter(|w| !w.is_bare).collect();
                        if non_bare.is_empty() {
                            check_pass("worktrees: none active");
                        } else {
                            check_pass(&format!("worktrees: {} active", non_bare.len()));
                            for wt in &non_bare {
                                let exists = wt.path.exists();
                                if exists {
                                    check_pass(&format!("  {} (branch: {})", wt.path.display(), wt.branch));
                                } else {
                                    check_fail(&format!("  {} (branch: {}) — path missing!", wt.path.display(), wt.branch));
                                    ok = false;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        check_fail(&format!("worktrees: {e}"));
                        ok = false;
                    }
                }

                // Check sync health
                match wt_mgr.sync() {
                    Ok(()) => check_pass("sync: bare repo synced successfully"),
                    Err(e) => {
                        check_fail(&format!("sync: {e}"));
                        ok = false;
                    }
                }
            }
            Err(e) => {
                check_fail(&format!("bare repo: {e}"));
                ok = false;
            }
        }
    } else {
        check_fail(&format!("bare repo: {} (not found)", bare_path.display()));
        ok = false;
    }

    // 4. Check agent binary
    let agent_cmd = "bunx";
    match Command::new("which").arg(agent_cmd).output() {
        Ok(out) if out.status.success() => {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            check_pass(&format!("agent ({agent_cmd}): {path}"));
        }
        _ => {
            check_fail(&format!("agent ({agent_cmd}): not found in PATH"));
            ok = false;
        }
    }

    // 5. Check log file
    let log_path = super::global_dir().join("logs").join("enki.log");
    if log_path.exists() {
        let meta = std::fs::metadata(&log_path)?;
        let size_kb = meta.len() / 1024;
        check_pass(&format!("log file: {} ({size_kb} KB)", log_path.display()));
    } else {
        check_warn(&format!("log file: {} (not found — run TUI once to create)", log_path.display()));
    }

    // Summary
    println!();
    if ok {
        println!("All checks passed.");
    } else {
        println!("Some checks failed. Review the output above.");
    }

    Ok(())
}

fn check_pass(msg: &str) {
    println!("  OK  {msg}");
}

fn check_fail(msg: &str) {
    println!("  FAIL  {msg}");
}

fn check_warn(msg: &str) {
    println!("  WARN  {msg}");
}
