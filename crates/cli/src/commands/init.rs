use enki_core::db::Db;
use enki_core::worktree::WorktreeManager;

pub async fn init() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let enki_dir = cwd.join(".enki");
    let db_path = enki_dir.join("db.sqlite");
    let bare_path = enki_dir.join("bare.git");

    let has_db = db_path.exists();
    let has_bare = bare_path.exists();

    if has_db && has_bare {
        println!("already initialized at {}", enki_dir.display());
        return Ok(());
    }

    std::fs::create_dir_all(&enki_dir)?;

    // Create .gitignore so DB and git artifacts aren't tracked
    std::fs::write(
        enki_dir.join(".gitignore"),
        "db.sqlite*\nbare.git/\nworktrees/\n",
    )?;

    if !has_db {
        Db::open(db_path.to_str().unwrap())?;
    }

    if !has_bare {
        if has_db {
            eprintln!("detected partial initialization (missing bare.git), repairing...");
        }
        WorktreeManager::init_bare(&cwd, &bare_path)?;
    }

    println!("Initialized enki at {}", enki_dir.display());
    Ok(())
}
