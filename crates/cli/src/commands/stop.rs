use enki_core::types::TaskStatus;

use super::{enki_dir, open_db};

pub async fn stop() -> anyhow::Result<()> {
    let dir = enki_dir()?;

    // Write the sentinel file for the coordinator to pick up.
    let stop_file = dir.join("stop");
    std::fs::write(&stop_file, "")?;

    // Also mark all Running tasks as Failed in the DB so `enki status`
    // reflects the change immediately, even before the coordinator reacts.
    let db = open_db()?;
    let tasks = db.list_tasks()?;
    let mut count = 0;
    for task in &tasks {
        if task.status == TaskStatus::Running {
            db.update_task_status(&task.id, TaskStatus::Failed)?;
            count += 1;
        }
    }

    if count == 0 {
        println!("Stop signal sent. No running tasks found.");
    } else {
        println!(
            "Stop signal sent. {} running task{} marked as failed.",
            count,
            if count == 1 { "" } else { "s" }
        );
    }

    Ok(())
}
