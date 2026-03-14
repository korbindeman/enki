use std::collections::HashMap;
use std::path::PathBuf;

use enki_core::types::Id;

use super::prompts::build_worker_prompt;
use super::session::CoordinatorSession;
use super::workers::discover_artifact_files;
use super::{FromCoordinator, Runtime};

/// Data collected during sync prep phase for a worker spawn.
pub(super) struct WorkerPrep {
    pub task_id: Id,
    pub title: String,
    pub description: String,
    pub tier: enki_core::types::Tier,
    pub execution_id: Id,
    pub step_id: String,
    pub upstream_outputs: Vec<(String, String)>,
    pub role: Option<String>,
    pub branch: String,
    pub copy_path: PathBuf,
    pub base_commit: Option<String>,
    pub artifact: bool,
}

/// Result of the sync prep phase before ACP session creation.
pub(super) struct PrepResult {
    pub branch: String,
    pub copy_path: PathBuf,
    pub base_commit: Option<String>,
    pub artifact: bool,
    pub agent_program: String,
    pub agent_args: Vec<String>,
    pub agent_env: HashMap<String, String>,
    pub mcp_args: Vec<String>,
}

impl Runtime {
    /// Sync prep phase for a worker spawn: create worktree/copy, assign task in DB,
    /// resolve agent command. Fast with worktrees.
    pub(super) fn prepare_worker(
        &mut self,
        task_id: &Id,
        role: Option<&str>,
    ) -> anyhow::Result<PrepResult> {
        let branch = format!("task/{}", task_id);
        let (copy_path, base_commit, base_branch) = self.copy_mgr.create_copy(&task_id.0)?;

        let agent_id = Id::new("agent");
        self.orch.db().assign_task(
            task_id, &agent_id, copy_path.to_str().unwrap(), &branch, &base_branch,
        )?;

        let role_config = role.and_then(|r| self.roles.get(r));
        let can_edit = role_config.map(|r| r.can_edit).unwrap_or(true);
        let artifact = role_config
            .map(|r| r.output == enki_core::roles::OutputMode::Artifact)
            .unwrap_or(false);

        let mut mcp_args = vec![
            "mcp".into(), "--role".into(), "worker".into(),
            "--task-id".into(), task_id.0.clone(),
        ];
        if !can_edit {
            mcp_args.push("--no-edit".into());
        }

        let agent_cmd =
            enki_core::agent_runtime::resolve_from_config(&self.config.agent_for_role("worker"))
                .map_err(|e| anyhow::anyhow!("{e}"))?;

        Ok(PrepResult {
            branch,
            copy_path,
            base_commit,
            artifact,
            agent_program: agent_cmd.program.to_str().unwrap().to_string(),
            agent_args: agent_cmd.args,
            agent_env: agent_cmd.env,
            mcp_args,
        })
    }

    /// Finalize a worker spawn after the ACP session is created: register tracker,
    /// build prompt, dispatch the worker task.
    pub(super) fn finalize_worker_spawn(
        &mut self,
        prep: WorkerPrep,
        session_id: String,
        coord: &mut CoordinatorSession,
    ) {
        self.tracker.borrow_mut().register(session_id.clone(), prep.task_id.0.clone());
        self.orch.set_step_session(&prep.execution_id.0, &prep.step_id, session_id.clone());

        tracing::info!(
            task_id = %prep.task_id, title = %prep.title,
            branch = %prep.branch, execution_id = %prep.execution_id,
            tier = %prep.tier.as_str(),
            role = prep.role.as_deref().unwrap_or("default"),
            "worker spawned"
        );

        let artifact_path = if prep.artifact {
            let artifacts_dir = self.copy_mgr.project_root()
                .join(".enki").join("artifacts").join(&prep.execution_id.0);
            let _ = std::fs::create_dir_all(&artifacts_dir);
            Some(artifacts_dir.join(format!("{}.md", prep.step_id)))
        } else {
            None
        };

        let artifact_files = discover_artifact_files(
            self.copy_mgr.project_root(),
            &prep.execution_id.0,
            &prep.upstream_outputs,
        );

        let role_config = prep.role.as_deref().and_then(|r| self.roles.get(r));
        let role_prompt = role_config.map(|r| r.system_prompt.as_str());
        let prompt = build_worker_prompt(
            &prep.title, &prep.description, &prep.upstream_outputs, &artifact_files,
            role_prompt, artifact_path.as_deref(),
        );

        let _ = self.tx.send(FromCoordinator::WorkerSpawned {
            task_id: prep.task_id.0.clone(),
            title: prep.title.clone(),
            tier: prep.tier.as_str().to_string(),
            role: prep.role.clone(),
            branch: Some(prep.branch.clone()),
            description: Some(prep.description.clone()),
        });
        coord.queue_event(format!(
            "- Worker spawned for \"{}\" ({})", prep.title, prep.task_id
        ));

        let mgr_clone = self.mgr.clone();
        let tracker_clone = self.tracker.clone();
        let done_tx = self.worker_done_tx.clone();
        let sid_for_done = session_id.clone();

        tokio::task::spawn_local(async move {
            let content = vec![enki_acp::acp_schema::ContentBlock::Text(
                enki_acp::acp_schema::TextContent::new(prompt),
            )];
            let result = mgr_clone.prompt(&session_id, content).await;
            tracker_clone.borrow_mut().remove(&session_id);
            mgr_clone.kill_session(&session_id);
            let _ = done_tx.send(super::workers::WorkerDone {
                task_id: prep.task_id,
                session_id: Some(sid_for_done),
                title: prep.title,
                branch: prep.branch,
                copy_path: prep.copy_path,
                base_commit: prep.base_commit,
                result: result.map_err(|e| e.to_string()),
                execution_id: Some(prep.execution_id),
                step_id: Some(prep.step_id),
                artifact: prep.artifact,
            });
        });
    }
}
