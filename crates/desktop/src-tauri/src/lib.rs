mod bridge;

pub fn run() {
    init_logging();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            bridge::send_prompt,
            bridge::interrupt,
            bridge::stop_all,
            bridge::get_project_dir,
            bridge::open_project,
        ])
        .setup(|app| {
            bridge::setup(app.handle())?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn init_logging() {
    let log_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".enki")
        .join("logs");
    std::fs::create_dir_all(&log_dir).ok();

    let file_appender = tracing_appender::rolling::never(&log_dir, "enki-desktop.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    // Leak the guard so the logger lives for the process lifetime.
    // In the desktop app, the process exits cleanly via Tauri's event loop.
    std::mem::forget(_guard);

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("enki=debug,enki_core=debug,enki_acp=debug,enki_desktop=debug")
            }),
        )
        .init();
}
