use enki_core::db::Db;

use super::workspace_dir;

pub async fn init() -> anyhow::Result<()> {
    let ws_dir = workspace_dir();
    std::fs::create_dir_all(&ws_dir)?;

    let db_path = ws_dir.join("db.sqlite");
    if db_path.exists() {
        println!("workspace already initialized at {}", ws_dir.display());
        return Ok(());
    }

    Db::open(db_path.to_str().unwrap())?;
    println!("initialized enki workspace at {}", ws_dir.display());
    Ok(())
}
