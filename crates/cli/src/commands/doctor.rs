use std::process::Command;

pub async fn doctor() -> anyhow::Result<()> {
    let mut ok = true;

    // 1. Check .enki/ directory
    match super::enki_dir() {
        Ok(dir) => check_pass(&format!(".enki directory: {}", dir.display())),
        Err(e) => {
            check_fail(&format!(".enki directory: {e}"));
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

    // 3. Check copies directory
    let copies_dir = super::copies_dir()?;
    if copies_dir.exists() {
        let count = std::fs::read_dir(&copies_dir)
            .map(|rd| rd.filter_map(|e| e.ok()).count())
            .unwrap_or(0);
        check_pass(&format!("copies dir: {} ({count} copies)", copies_dir.display()));
    } else {
        check_warn(&format!(
            "copies dir: {} (not found — will be created on first worker spawn)",
            copies_dir.display()
        ));
    }

    // 4. Check git repo
    let project_root = super::project_root()?;
    let output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(&project_root)
        .output();
    match output {
        Ok(out) if out.status.success() => {
            check_pass(&format!("git repo: {}", project_root.display()));

            // Check default branch
            for candidate in &["main", "master"] {
                let out = Command::new("git")
                    .args(["rev-parse", "--verify", candidate])
                    .current_dir(&project_root)
                    .output();
                if let Ok(o) = out {
                    if o.status.success() {
                        check_pass(&format!("default branch: {candidate}"));
                        break;
                    }
                }
            }
        }
        _ => {
            check_fail(&format!("git repo: {} (not a git repo)", project_root.display()));
            ok = false;
        }
    }

    // 5. Check agent binary
    match enki_core::agent_runtime::resolve() {
        Ok(cmd) => {
            check_pass(&format!(
                "agent: {} {}",
                cmd.program.display(),
                cmd.args.join(" ")
            ));
        }
        Err(e) => {
            check_fail(&format!("agent: {e}"));
            ok = false;
        }
    }

    // 6. Check log file
    let log_path = super::global_dir().join("logs").join("enki.log");
    if log_path.exists() {
        let meta = std::fs::metadata(&log_path)?;
        let size_kb = meta.len() / 1024;
        check_pass(&format!("log file: {} ({size_kb} KB)", log_path.display()));
    } else {
        check_warn(&format!(
            "log file: {} (not found — run TUI once to create)",
            log_path.display()
        ));
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
