use enki_core::types::Id;

use super::prompts::build_merger_prompt;
use super::workers::MergerAgentDone;
use super::Runtime;

impl Runtime {
    pub(super) async fn spawn_merger_agent(
        &mut self,
        mr_id: &str,
        task_id: &Id,
        temp_dir: &std::path::Path,
        default_branch: &str,
        conflict_files: &[String],
        conflict_diff: &str,
        task_desc: &str,
    ) -> anyhow::Result<()> {
        let merger_mcp = vec![enki_acp::acp_schema::McpServer::Stdio(
            enki_acp::acp_schema::McpServerStdio::new("enki", &self.enki_bin)
                .args(vec![
                    "mcp".into(), "--role".into(), "merger".into(),
                    "--task-id".into(), task_id.0.clone(),
                ]),
        )];

        let agent_cmd =
            enki_core::agent_runtime::resolve_from_config(&self.config.agent).map_err(|e| anyhow::anyhow!("{e}"))?;
        let args_ref: Vec<&str> = agent_cmd.args.iter().map(|s| s.as_str()).collect();
        let session_id = self.mgr
            .start_session_with_mcp(
                agent_cmd.program.to_str().unwrap(), &args_ref,
                temp_dir.to_path_buf(), merger_mcp, &format!("merger-{mr_id}"),
                self.config.workers.sonnet_only,
            )
            .await?;

        self.tracker.borrow_mut().register(session_id.clone(), task_id.0.clone());

        let prompt = build_merger_prompt(task_desc, conflict_files, conflict_diff);
        let mgr_clone = self.mgr.clone();
        let tracker_clone = self.tracker.clone();
        let done_tx = self.merger_agent_done_tx.clone();
        let mr_id_owned = Id(mr_id.to_string());
        let temp_dir_owned = temp_dir.to_path_buf();
        let default_branch_owned = default_branch.to_string();
        let sid_clone = session_id.clone();

        tokio::task::spawn_local(async move {
            let content = vec![enki_acp::acp_schema::ContentBlock::Text(
                enki_acp::acp_schema::TextContent::new(prompt),
            )];
            let result = mgr_clone.prompt(&session_id, content).await;
            tracker_clone.borrow_mut().remove(&session_id);
            mgr_clone.kill_session(&session_id);

            match result {
                Ok(_) => {
                    let _ = done_tx.send(MergerAgentDone {
                        mr_id: mr_id_owned,
                        temp_dir: temp_dir_owned,
                        default_branch: default_branch_owned,
                        session_id: sid_clone,
                    });
                }
                Err(e) => {
                    tracing::error!(error = %e, "merger agent failed");
                    // Clean up and report failure through the merge done channel.
                    let _ = std::fs::remove_dir_all(&temp_dir_owned);
                }
            }
        });

        Ok(())
    }
}
