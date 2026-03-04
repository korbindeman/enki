use enki_core::db::Db;

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

    // Create .gitignore so DB and worker copies aren't tracked
    std::fs::write(
        enki_dir.join(".gitignore"),
        "db.sqlite*\ncopies/\nevents/\nlogs/\n",
    )?;

    Db::open(db_path.to_str().unwrap())?;

    println!("Initialized enki at {}", enki_dir.display());
    Ok(())
}
