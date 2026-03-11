use super::*;

fn test_orchestrator() -> Orchestrator {
    let db = crate::db::Db::open_in_memory().unwrap();
    Orchestrator::new(db, crate::scheduler::Limits::default(), "test-session".into())
}

#[test]
fn create_single_task_spawns_worker() {
    let mut orch = test_orchestrator();
    let events = orch.handle(Command::CreateTask {
        title: "Fix bug".into(),
        description: Some("Fix the auth bug".into()),
        tier: Tier::Standard,
    });

    let spawns: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Event::SpawnWorker { .. }))
        .collect();
    assert_eq!(spawns.len(), 1);

    if let Event::SpawnWorker { title, tier, .. } = &spawns[0] {
        assert_eq!(title, "Fix bug");
        assert_eq!(*tier, Tier::Standard);
    }
}

#[test]
fn create_execution_spawns_root_tasks() {
    let mut orch = test_orchestrator();
    let events = orch.handle(Command::CreateExecution {
        steps: vec![
            StepDef {
                id: "design".into(),
                title: "Design".into(),
                description: "Design the feature".into(),
                tier: Tier::Heavy,
                needs: vec![],
                checkpoint: false,
                role: None,
            },
            StepDef {
                id: "implement".into(),
                title: "Implement".into(),
                description: "Implement the feature".into(),
                tier: Tier::Standard,
                needs: vec!["design".into()],
                checkpoint: false,
                role: None,
            },
            StepDef {
                id: "test".into(),
                title: "Test".into(),
                description: "Write tests".into(),
                tier: Tier::Light,
                needs: vec!["design".into()],
                checkpoint: false,
                role: None,
            },
        ],
    });

    // Only design should spawn (it's the root with no deps).
    let spawns: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Event::SpawnWorker { .. }))
        .collect();
    assert_eq!(spawns.len(), 1);
    if let Event::SpawnWorker { title, tier, .. } = &spawns[0] {
        assert_eq!(title, "Design");
        assert_eq!(*tier, Tier::Heavy);
    }

    // DB should have 3 tasks and 1 execution.
    let tasks = orch.db().list_tasks().unwrap();
    assert_eq!(tasks.len(), 3);
}

#[test]
fn worker_success_queues_merge() {
    let mut orch = test_orchestrator();

    // Create a task to get an execution/step context.
    let events = orch.handle(Command::CreateTask {
        title: "Fix bug".into(),
        description: Some("desc".into()),
        tier: Tier::Standard,
    });
    let (task_id, exec_id, step_id) = match &events[0] {
        Event::SpawnWorker {
            task_id,
            execution_id,
            step_id,
            ..
        } => (task_id.clone(), execution_id.clone(), step_id.clone()),
        _ => panic!("expected SpawnWorker"),
    };

    // Worker succeeds.
    let events = orch.handle(Command::WorkerDone(WorkerResult {
        task_id: task_id.clone(),
        execution_id: Some(exec_id),
        step_id: Some(step_id),
        title: "Fix bug".into(),
        branch: "task/fix-bug".into(),
        outcome: WorkerOutcome::Success {
            output: Some("Fixed the bug".into()),
        },
    }));

    assert!(events.iter().any(|e| matches!(e, Event::WorkerCompleted { .. })));
    assert!(events.iter().any(|e| matches!(e, Event::QueueMerge(_))));

    // Task output stored in DB.
    let output = orch.db().get_task_output(&task_id).unwrap();
    assert_eq!(output.as_deref(), Some("Fixed the bug"));
}

#[test]
fn worker_no_changes_retries_then_fails() {
    let mut orch = test_orchestrator();
    let events = orch.handle(Command::CreateTask {
        title: "Fix bug".into(),
        description: None,
        tier: Tier::Standard,
    });
    let task_id = match &events[0] {
        Event::SpawnWorker { task_id, .. } => task_id.clone(),
        _ => panic!("expected SpawnWorker"),
    };

    let make_result = || WorkerResult {
        task_id: task_id.clone(),
        execution_id: None,
        step_id: None,
        title: "Fix bug".into(),
        branch: "task/fix-bug".into(),
        outcome: WorkerOutcome::NoChanges,
    };

    // First NoChanges: should retry (status goes to Pending).
    let events = orch.handle(Command::WorkerDone(make_result()));
    assert!(events.iter().any(|e| matches!(e, Event::WorkerFailed { error, .. } if error.contains("retrying"))));
    let task = orch.db().get_task(&task_id).unwrap();
    assert_eq!(task.status, TaskStatus::Pending);

    // Second NoChanges: should retry again.
    let events = orch.handle(Command::WorkerDone(make_result()));
    assert!(events.iter().any(|e| matches!(e, Event::WorkerFailed { error, .. } if error.contains("retrying"))));
    let task = orch.db().get_task(&task_id).unwrap();
    assert_eq!(task.status, TaskStatus::Pending);

    // Third NoChanges: retry budget exhausted, permanently fails.
    let events = orch.handle(Command::WorkerDone(make_result()));
    assert!(events.iter().any(|e| matches!(e, Event::WorkerFailed { error, .. } if !error.contains("retrying"))));
    let task = orch.db().get_task(&task_id).unwrap();
    assert_eq!(task.status, TaskStatus::Failed);
}

#[test]
fn merge_landed_advances_dag() {
    let mut orch = test_orchestrator();

    // Create a 2-step execution: a → b.
    let events = orch.handle(Command::CreateExecution {
        steps: vec![
            StepDef {
                id: "a".into(),
                title: "Step A".into(),
                description: "First".into(),
                tier: Tier::Standard,
                needs: vec![],
                checkpoint: false,
                role: None,
            },
            StepDef {
                id: "b".into(),
                title: "Step B".into(),
                description: "Second".into(),
                tier: Tier::Standard,
                needs: vec!["a".into()],
                checkpoint: false,
                role: None,
            },
        ],
    });

    let (task_id_a, exec_id, step_id_a) = match &events[0] {
        Event::SpawnWorker {
            task_id,
            execution_id,
            step_id,
            ..
        } => (task_id.clone(), execution_id.clone(), step_id.clone()),
        _ => panic!("expected SpawnWorker"),
    };

    // Worker A succeeds.
    let events = orch.handle(Command::WorkerDone(WorkerResult {
        task_id: task_id_a.clone(),
        execution_id: Some(exec_id.clone()),
        step_id: Some(step_id_a),
        title: "Step A".into(),
        branch: "task/step-a".into(),
        outcome: WorkerOutcome::Success {
            output: Some("A output".into()),
        },
    }));
    // Should get QueueMerge but NOT SpawnWorker for B yet (merge hasn't landed).
    assert!(events.iter().any(|e| matches!(e, Event::QueueMerge(_))));
    let b_spawns: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Event::SpawnWorker { title, .. } if title == "Step B"))
        .collect();
    assert!(b_spawns.is_empty());

    // Get the merge request.
    let mr = match events.iter().find(|e| matches!(e, Event::QueueMerge(_))) {
        Some(Event::QueueMerge(mr)) => mr.clone(),
        _ => panic!("expected QueueMerge"),
    };

    // Merge lands.
    let events = orch.handle(Command::MergeDone(MergeResult {
        mr_id: mr.id.clone(),
        outcome: crate::refinery::MergeOutcome::Merged,
    }));

    // Now B should spawn.
    let b_spawns: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Event::SpawnWorker { title, .. } if title == "Step B"))
        .collect();
    assert_eq!(b_spawns.len(), 1);

    // B should have upstream output from A.
    if let Event::SpawnWorker {
        upstream_outputs, ..
    } = &b_spawns[0]
    {
        assert_eq!(upstream_outputs.len(), 1);
        assert_eq!(upstream_outputs[0].0, "Step A");
        assert_eq!(upstream_outputs[0].1, "A output");
    }
}

#[test]
fn worker_timeout_retries() {
    let mut orch = test_orchestrator();
    let events = orch.handle(Command::CreateTask {
        title: "Slow task".into(),
        description: None,
        tier: Tier::Standard,
    });
    let (task_id, exec_id, step_id) = match &events[0] {
        Event::SpawnWorker {
            task_id,
            execution_id,
            step_id,
            ..
        } => (task_id.clone(), execution_id.clone(), step_id.clone()),
        _ => panic!("expected SpawnWorker"),
    };

    // First failure: timeout → should retry.
    let events = orch.handle(Command::WorkerDone(WorkerResult {
        task_id: task_id.clone(),
        execution_id: Some(exec_id.clone()),
        step_id: Some(step_id.clone()),
        title: "Slow task".into(),
        branch: "task/slow".into(),
        outcome: WorkerOutcome::Failed {
            error: "worker timed out after 30 minutes".into(),
        },
    }));
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::WorkerFailed { error, .. } if error.contains("retrying"))));

    // Retry resets the DAG node, and tick_scheduler re-dispatches it immediately.
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::SpawnWorker { .. })));
    let task = orch.db().get_task(&task_id).unwrap();
    assert_eq!(task.status, TaskStatus::Running);
}

#[test]
fn worker_non_timeout_failure_no_retry() {
    let mut orch = test_orchestrator();
    let events = orch.handle(Command::CreateTask {
        title: "Bad task".into(),
        description: None,
        tier: Tier::Standard,
    });
    let task_id = match &events[0] {
        Event::SpawnWorker { task_id, .. } => task_id.clone(),
        _ => panic!("expected SpawnWorker"),
    };

    let events = orch.handle(Command::WorkerDone(WorkerResult {
        task_id: task_id.clone(),
        execution_id: None,
        step_id: None,
        title: "Bad task".into(),
        branch: "task/bad".into(),
        outcome: WorkerOutcome::Failed {
            error: "compilation error".into(),
        },
    }));
    // Should NOT retry non-timeout errors.
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::WorkerFailed { error, .. } if !error.contains("retrying"))));

    let task = orch.db().get_task(&task_id).unwrap();
    assert_eq!(task.status, TaskStatus::Failed);
}

#[test]
fn stop_all_aborts_everything() {
    let mut orch = test_orchestrator();
    orch.handle(Command::CreateTask {
        title: "Task 1".into(),
        description: None,
        tier: Tier::Standard,
    });

    let events = orch.handle(Command::StopAll);
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::AllStopped { count } if *count > 0)));
}

#[test]
fn discover_from_db_wraps_orphan_tasks() {
    let mut orch = test_orchestrator();
    let now = chrono::Utc::now();

    // Insert an orphan ready task in the current session (not part of any execution).
    let task_id = crate::types::Id::new("task");
    let task = crate::types::Task {
        id: task_id.clone(),
        session_id: Some("test-session".into()),
        title: "Orphan task".into(),
        description: None,
        status: TaskStatus::Pending,
        assigned_to: None,
        copy_path: None,
        branch: None,
        base_branch: None,
        tier: Some(Tier::Light),
        current_activity: None,
        created_at: now,
        updated_at: now,
    };
    orch.db().insert_task(&task).unwrap();

    let events = orch.handle(Command::DiscoverFromDb);
    let spawns: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Event::SpawnWorker { .. }))
        .collect();
    assert_eq!(spawns.len(), 1);
}

#[test]
fn merge_conflicted_marks_task_blocked() {
    let mut orch = test_orchestrator();
    let events = orch.handle(Command::CreateTask {
        title: "Conflict task".into(),
        description: None,
        tier: Tier::Standard,
    });
    let (task_id, exec_id, step_id) = match &events[0] {
        Event::SpawnWorker {
            task_id,
            execution_id,
            step_id,
            ..
        } => (task_id.clone(), execution_id.clone(), step_id.clone()),
        _ => panic!("expected SpawnWorker"),
    };

    // Worker succeeds → merge queued.
    let events = orch.handle(Command::WorkerDone(WorkerResult {
        task_id: task_id.clone(),
        execution_id: Some(exec_id),
        step_id: Some(step_id),
        title: "Conflict task".into(),
        branch: "task/conflict".into(),
        outcome: WorkerOutcome::Success { output: None },
    }));
    let mr = match events.iter().find(|e| matches!(e, Event::QueueMerge(_))) {
        Some(Event::QueueMerge(mr)) => mr.clone(),
        _ => panic!("expected QueueMerge"),
    };

    // Merge conflicts.
    let events = orch.handle(Command::MergeDone(MergeResult {
        mr_id: mr.id.clone(),
        outcome: crate::refinery::MergeOutcome::Conflicted("conflict in main.rs".into()),
    }));
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::MergeConflicted { .. })));

    let task = orch.db().get_task(&task_id).unwrap();
    assert_eq!(task.status, TaskStatus::Blocked);
}

#[test]
fn pause_resume_execution() {
    let mut orch = test_orchestrator();
    let events = orch.handle(Command::CreateExecution {
        steps: vec![
            StepDef {
                id: "a".into(),
                title: "A".into(),
                description: "first".into(),
                tier: Tier::Standard,
                needs: vec![],
                checkpoint: false,
                role: None,
            },
            StepDef {
                id: "b".into(),
                title: "B".into(),
                description: "second".into(),
                tier: Tier::Standard,
                needs: vec!["a".into()],
                checkpoint: false,
                role: None,
            },
        ],
    });
    let exec_id = match &events[0] {
        Event::SpawnWorker { execution_id, .. } => execution_id.0.clone(),
        _ => panic!("expected SpawnWorker"),
    };

    // Pause should produce no errors.
    let events = orch.handle(Command::Pause(Target::Execution(exec_id.clone())));
    assert!(events.is_empty());

    // Resume should re-evaluate.
    let events = orch.handle(Command::Resume(Target::Execution(exec_id)));
    // No new spawns expected since A is still running (not completed).
    let spawns: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Event::SpawnWorker { .. }))
        .collect();
    assert!(spawns.is_empty());
}
