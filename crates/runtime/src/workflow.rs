use super::*;
#[derive(Serialize, Deserialize)]
struct Definition {
    workflow: Workflow,
    input: Value,
}
impl Harness {
    /// Start a durable workflow coordinator. Its leaf runs share one turn budget.
    pub async fn start_workflow(&self, workflow: Workflow, input: Value) -> Result<RunHandle> {
        self.validate_workflow(&workflow, 0)?;
        let definition = serde_json::to_string(&Definition { workflow, input })?;
        anyhow::ensure!(
            definition.len() <= self.limits.context_bytes,
            "workflow definition exceeds context limit"
        );
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .context("active run limit reached")?;
        let session = self.store.create_session("Workflow mission").await?;
        let run_id = id();
        let workspace = if self.isolated {
            self.workspace.join(&run_id)
        } else {
            self.workspace.clone()
        };
        tokio::fs::create_dir_all(&workspace).await?;
        let run = Run {
            id: run_id,
            session_id: session.id,
            parent_id: None,
            agent: Agent {
                name: "workflow".into(),
                instructions: definition,
                provider: "__workflow".into(),
                tools: vec![],
                output_schema: None,
            },
            status: RunStatus::Queued,
            created_at: now(),
            error: None,
            workspace,
        };
        self.store.save_run(&run).await?;
        self.launch(
            run,
            Scope {
                cancel: CancellationToken::new(),
                turns: Arc::new(AtomicUsize::new(0)),
                depth: 0,
                children: Arc::new(AtomicUsize::new(0)),
            },
            permit,
        )
        .await
    }
    fn validate_workflow(&self, w: &Workflow, depth: usize) -> Result<()> {
        anyhow::ensure!(depth < 32, "workflow nesting exceeds 32 levels");
        match w {
            Workflow::Agent { agent, input } => {
                anyhow::ensure!(
                    self.agents.contains_key(agent),
                    "unknown workflow agent {agent}"
                );
                anyhow::ensure!(
                    input.len() <= self.limits.context_bytes / 2,
                    "workflow prompt exceeds input limit"
                );
            }
            Workflow::Sequence { steps } => {
                anyhow::ensure!(
                    !steps.is_empty() && steps.len() <= 100,
                    "sequence requires 1–100 steps"
                );
                for s in steps {
                    self.validate_workflow(s, depth + 1)?;
                }
            }
            Workflow::Parallel { branches } => {
                anyhow::ensure!(
                    !branches.is_empty() && branches.len() <= self.limits.children,
                    "parallel branch limit exceeded"
                );
                for s in branches {
                    self.validate_workflow(s, depth + 1)?;
                }
            }
            Workflow::Branch {
                then_step,
                else_step,
                ..
            } => {
                self.validate_workflow(then_step, depth + 1)?;
                self.validate_workflow(else_step, depth + 1)?;
            }
        }
        Ok(())
    }
    pub(super) async fn workflow_engine(&self, root: &Run, scope: &Scope) -> Result<()> {
        let definition: Definition = serde_json::from_str(&root.agent.instructions)?;
        let result = self
            .workflow_node(
                &definition.workflow,
                definition.input,
                root,
                scope,
                "root".into(),
            )
            .await?;
        self.store
            .add_message(
                &root.session_id,
                &Message::text("assistant", result.to_string()),
            )
            .await?;
        self.emit(&root.id, EventKind::Text(result.to_string()))
            .await?;
        Ok(())
    }
    fn workflow_node<'a>(
        &'a self,
        w: &'a Workflow,
        input: Value,
        root: &'a Run,
        scope: &'a Scope,
        path: String,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            anyhow::ensure!(!scope.cancel.is_cancelled(), "cancelled");
            let saved = self.store.workflow_step(&root.id, &path).await?;
            if let Some(output) = saved.as_ref().and_then(|s| s.get("output")) {
                return Ok(output.clone());
            }
            let output = match w {
                Workflow::Agent {
                    agent,
                    input: template,
                } => {
                    let child = if let Some(run) = saved.as_ref().and_then(|s| s.get("run")) {
                        serde_json::from_value::<Run>(run.clone())?
                    } else {
                        let prompt = template.replace("{{input}}", &input.to_string());
                        anyhow::ensure!(
                            prompt.len() <= self.limits.context_bytes / 2,
                            "expanded workflow prompt exceeds context limit"
                        );
                        let session = self.store.create_session(&prompt).await?;
                        let run = Run {
                            id: id(),
                            session_id: session.id,
                            parent_id: Some(root.id.clone()),
                            agent: self
                                .agents
                                .get(agent)
                                .context("workflow agent missing")?
                                .clone(),
                            status: RunStatus::Queued,
                            created_at: now(),
                            error: None,
                            workspace: root.workspace.clone(),
                        };
                        self.store
                            .add_message(&run.session_id, &Message::text("user", prompt))
                            .await?;
                        self.store
                            .save_workflow_step(&root.id, &path, json!({"run":run}))
                            .await?;
                        run
                    };
                    let mut child = self.store.run(&child.id).await.unwrap_or(child);
                    if child.status == RunStatus::NeedsReconciliation {
                        return Err(UncertainEffect(format!(
                            "workflow child {} requires reconciliation",
                            child.id
                        ))
                        .into());
                    }
                    if child.status != RunStatus::Completed {
                        anyhow::ensure!(
                            !self.active.lock().await.contains_key(&child.id),
                            "workflow child is already active"
                        );
                        let permit = self
                            .slots
                            .clone()
                            .try_acquire_owned()
                            .context("active run limit reached")?;
                        child.status = RunStatus::Queued;
                        self.store.save_run(&child).await?;
                        self.emit(
                            &root.id,
                            EventKind::Child {
                                id: child.id.clone(),
                                name: child.agent.name.clone(),
                            },
                        )
                        .await?;
                        let child_scope = Scope {
                            cancel: scope.cancel.child_token(),
                            turns: scope.turns.clone(),
                            depth: scope.depth + 1,
                            children: Arc::new(AtomicUsize::new(0)),
                        };
                        let mut handle = self.launch(child.clone(), child_scope, permit).await?;
                        let status = handle.wait().await;
                        if status == RunStatus::NeedsReconciliation {
                            return Err(UncertainEffect(format!(
                                "workflow child {} requires reconciliation",
                                child.id
                            ))
                            .into());
                        }
                        anyhow::ensure!(
                            status == RunStatus::Completed,
                            "workflow child {} ended with {:?}",
                            child.id,
                            status
                        );
                    }
                    json!({"run_id":child.id,"result":self.store.messages(&child.session_id).await?.last().map(|m|&m.text)})
                }
                Workflow::Sequence { steps } => {
                    let mut value = input;
                    for (i, step) in steps.iter().enumerate() {
                        value = self
                            .workflow_node(step, value, root, scope, format!("{path}/{i}"))
                            .await?;
                    }
                    value
                }
                Workflow::Parallel { branches } => {
                    let mut work = FuturesUnordered::new();
                    let group = scope.cancel.child_token();
                    for (i, branch) in branches.iter().enumerate() {
                        let input = input.clone();
                        let child_scope = Scope {
                            cancel: group.child_token(),
                            ..scope.clone()
                        };
                        let path = format!("{path}/{i}");
                        work.push(async move {
                            (
                                i,
                                self.workflow_node(branch, input, root, &child_scope, path)
                                    .await,
                            )
                        });
                    }
                    let mut out = vec![Value::Null; branches.len()];
                    let mut error = None;
                    while let Some((i, r)) = work.next().await {
                        match r {
                            Ok(v) => out[i] = v,
                            Err(e) => {
                                group.cancel();
                                if error.is_none() {
                                    error = Some(e);
                                }
                            }
                        }
                    }
                    if let Some(e) = error {
                        return Err(e);
                    }
                    json!(out)
                }
                Workflow::Branch {
                    contains,
                    then_step,
                    else_step,
                } => {
                    let yes = input.to_string().contains(contains);
                    self.workflow_node(
                        if yes { then_step } else { else_step },
                        input,
                        root,
                        scope,
                        format!("{path}/{}", if yes { "then" } else { "else" }),
                    )
                    .await?
                }
            };
            self.store
                .save_workflow_step(&root.id, &path, json!({"output":output}))
                .await?;
            Ok(output)
        })
    }
    pub async fn workflow(
        &self,
        w: &Workflow,
        input: Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        let mut handle = self.start_workflow(w.clone(), input).await?;
        let status = tokio::select! {_=cancel.cancelled()=>{handle.cancel.cancel();handle.wait().await},s=handle.wait()=>s};
        anyhow::ensure!(
            status == RunStatus::Completed,
            "workflow {} ended with {:?}",
            handle.id,
            status
        );
        let run = self.store.run(&handle.id).await?;
        let messages = self.store.messages(&run.session_id).await?;
        Ok(serde_json::from_str(
            &messages.last().context("missing workflow result")?.text,
        )?)
    }
}
