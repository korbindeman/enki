use super::{parse_signal_target, Event, Orchestrator};

impl Orchestrator {
    pub(crate) fn check_signals(&mut self) -> Vec<Event> {
        let events_dir = match &self.events_dir {
            Some(d) => d.clone(),
            None => return Vec::new(),
        };

        if !events_dir.exists() {
            return Vec::new();
        }

        let entries = match std::fs::read_dir(&events_dir) {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };

        let mut found = false;
        let mut events = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            // Read and delete the signal file.
            if let Ok(content) = std::fs::read_to_string(&path) {
                let _ = std::fs::remove_file(&path);
                if let Ok(signal) = serde_json::from_str::<serde_json::Value>(&content) {
                    let signal_type = signal
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    match signal_type {
                        "execution_created" | "task_created" | "steps_added" => {
                            found = true;
                        }
                        "resume" => {
                            let target = parse_signal_target(&signal);
                            if let Some(t) = target {
                                events.extend(self.resume(t));
                            }
                        }
                        "stop_all" => {
                            return self.stop_all();
                        }
                        "pause" => {
                            let target = parse_signal_target(&signal);
                            if let Some(t) = target {
                                events.extend(self.pause(t));
                            }
                        }
                        "cancel" => {
                            let target = parse_signal_target(&signal);
                            if let Some(t) = target {
                                events.extend(self.cancel(t));
                            }
                        }
                        "worker_report" => {
                            if let (Some(tid), Some(status)) = (
                                signal.get("task_id").and_then(|v| v.as_str()),
                                signal.get("status").and_then(|v| v.as_str()),
                            ) {
                                events.push(Event::WorkerReport {
                                    task_id: tid.to_string(),
                                    status: status.to_string(),
                                });
                            }
                        }
                        "mail" => {
                            if let (Some(from), Some(to), Some(subject)) = (
                                signal.get("from").and_then(|v| v.as_str()),
                                signal.get("to").and_then(|v| v.as_str()),
                                signal.get("subject").and_then(|v| v.as_str()),
                            ) {
                                let msg_id = signal.get("message_id").and_then(|v| v.as_str()).unwrap_or("");
                                let priority = signal.get("priority").and_then(|v| v.as_str()).unwrap_or("normal");
                                events.push(Event::Mail {
                                    message_id: msg_id.to_string(),
                                    from: from.to_string(),
                                    to: to.to_string(),
                                    subject: subject.to_string(),
                                    priority: priority.to_string(),
                                });
                            }
                        }
                        _ => {
                            tracing::warn!(path = %path.display(), signal_type, "unknown signal type");
                        }
                    }
                }
            }
        }

        if found {
            // Re-discover from DB to pick up whatever was created.
            events.extend(self.discover_from_db());
        }
        events
    }
}
