//! Session model intent belongs to the active history path, independently of global defaults.
use super::*;
use eden_protocol::models as m;
#[cfg(test)]
use serde_json::Value;
use serde_json::json;

fn public_auth_terminal(private: &Terminal) -> Terminal {
    let mut public = private.clone();
    public.partial_result = None;
    if let Outcome::Completed(value) = &mut public.outcome {
        // Whitelist status metadata so future private reply fields stay private by default.
        *value = ["operation_id", "provider", "status", "source"]
            .into_iter()
            .filter_map(|key| value.get(key).map(|field| (key.to_owned(), field.clone())))
            .collect::<serde_json::Map<_, _>>()
            .into();
    }
    public
}

fn saved_selection(records: &[c::Record]) -> Result<Option<m::ModelSelection>, Fault> {
    eden_protocol::history::active_path(records)?
        .iter()
        .rev()
        .find(|r| r.kind == "model_selection")
        .map(|r| {
            serde_json::from_value(r.payload["selection"].clone()).map_err(|_| {
                Fault::new(
                    "InvalidHistory",
                    "model-selection",
                    "invalid saved model selection",
                )
            })
        })
        .transpose()
}
impl Session {
    /// Query the selected manager without changing session selection or replaying mutations.
    pub async fn managed_models(&self) -> Result<m::ManagerReply, Fault> {
        self.service(0, m::MODEL_MANAGER, &m::ManagerRequest::List)
            .await
    }
    /// Manage remote models as an owned run. Cancel and shutdown await remote-stop cleanup;
    /// cleanup errors mean the remote state is unknown and require an explicit reconnect.
    pub fn manage_models(&self, request: m::ManagerRequest) -> Result<u64, Fault> {
        self.start(true, move |session, run_id, cancel| async move {
            session
                .0
                .kernel
                .invoke(
                    Request {
                        execution: None,
                        session_id: session.id(),
                        run_id,
                        contract: m::MODEL_MANAGER.into(),
                        payload: json!(request),
                    },
                    cancel,
                )
                .await
        })
    }

    /// Read model metadata without inference or authentication side effects.
    pub async fn models(&self) -> Result<m::CatalogReply, Fault> {
        self.service(0, m::MODEL_CATALOG, &m::CatalogRequest::List)
            .await
    }
    /// Resolve a selection from the active branch; the result never contains credentials.
    pub async fn model_selection(&self) -> Result<Option<m::ModelSelection>, Fault> {
        saved_selection(&self.history().await?)
    }
    /// Commit a selection while idle. Completion means the history writer acknowledged it.
    pub fn select_model(&self, mut selection: m::ModelSelection) -> Result<u64, Fault> {
        self.start(true, move |session, run_id, _| async move {
            management::as_terminal(
                async {
                    if selection.thinking.is_none() {
                        let records =
                            eden_protocol::history::active_path(&session.history().await?)?;
                        selection.thinking = records
                            .iter()
                            .rev()
                            .filter(|record| record.kind == "model_selection")
                            .filter_map(|record| {
                                serde_json::from_value::<m::ModelSelection>(
                                    record.payload["selection"].clone(),
                                )
                                .ok()
                            })
                            .find(|prior| {
                                prior.provider == selection.provider
                                    && prior.model == selection.model
                            })
                            .and_then(|prior| prior.thinking);
                    }
                    let reply: m::CatalogReply = session
                        .service(
                            run_id,
                            m::MODEL_CATALOG,
                            &m::CatalogRequest::Resolve {
                                selection: Some(selection.clone()),
                            },
                        )
                        .await?;
                    let target = reply.target.ok_or_else(|| {
                        Fault::new("Unavailable", "model-selection", "model is not selectable")
                    })?;
                    session
                        .commit(
                            run_id,
                            "model_selection",
                            json!({ "selection": selection, "target": target }),
                        )
                        .await?;
                    Ok(json!(target))
                }
                .await,
            )
        })
    }
    /// Catalog mutations are managed runs, so cancellation and settled completion share the SDK contract.
    pub fn catalog(&self, request: m::CatalogRequest) -> Result<u64, Fault> {
        self.start(true, move |session, run_id, cancel| async move {
            session
                .0
                .kernel
                .invoke(
                    Request {
                        execution: None,
                        session_id: session.id(),
                        run_id,
                        contract: m::MODEL_CATALOG.into(),
                        payload: json!(request),
                    },
                    cancel,
                )
                .await
        })
    }
    /// Authentication input and wait/inspect results are private; events and history contain only status metadata.
    pub fn authenticate(&self, request: m::AuthRequest) -> Result<u64, Fault> {
        self.start_with_public_result(
            true,
            public_auth_terminal,
            move |session, run_id, cancel| async move {
                session
                    .0
                    .kernel
                    .invoke(
                        Request {
                            execution: None,
                            session_id: session.id(),
                            run_id,
                            contract: m::AUTH.into(),
                            payload: json!(request),
                        },
                        cancel,
                    )
                    .await
            },
        )
    }
    /// Inspect a transient operation while its managed wait is active, without claiming the session.
    pub async fn auth_status(&self, operation_id: &str) -> Result<m::AuthReply, Fault> {
        self.service(
            0,
            m::AUTH,
            &m::AuthRequest::Status {
                operation_id: operation_id.into(),
            },
        )
        .await
    }
    /// Pass private pasted input to an existing login while its managed wait owns the session.
    ///
    /// This does not start a second run. Keep the wait alive to observe completion and cleanup;
    /// operation identifiers are scoped to this session and cannot be resumed after reopening it.
    pub async fn submit_auth_input(
        &self,
        operation_id: &str,
        input: String,
    ) -> Result<m::AuthReply, Fault> {
        self.service(
            0,
            m::AUTH,
            &m::AuthRequest::Submit {
                operation_id: operation_id.into(),
                input,
            },
        )
        .await
    }
    pub(crate) async fn freeze_model(&self, run_id: u64) -> Result<Option<m::ModelTarget>, Fault> {
        if !self.has_role(m::MODEL_CATALOG) {
            return Ok(None);
        }
        let selection = self.model_selection().await?;
        let reply: m::CatalogReply = match self
            .service(
                run_id,
                m::MODEL_CATALOG,
                &m::CatalogRequest::Resolve {
                    selection: selection.clone(),
                },
            )
            .await
        {
            Ok(reply) => reply,
            Err(error) if selection.is_some() && error.code == "ModelUnavailable" => {
                let fallback: m::CatalogReply = self
                    .service(
                        run_id,
                        m::MODEL_CATALOG,
                        &m::CatalogRequest::Resolve { selection: None },
                    )
                    .await?;
                if let Some(target) = &fallback.target {
                    let selection = m::ModelSelection {
                        provider: target.provider.clone(),
                        model: target.model.clone(),
                        thinking: target.thinking.requested.clone(),
                    };
                    self.commit(
                        run_id,
                        "model_selection",
                        json!({
                            "selection": selection,
                            "target": target,
                            "reason": "saved model unavailable",
                        }),
                    )
                    .await?;
                    self.0.events.push(
                        run_id,
                        "model_fallback",
                        json!({ "target": target, "reason": "saved model unavailable" }),
                    );
                }
                fallback
            }
            Err(error) => return Err(error),
        };
        if selection.is_none()
            && let Some(target) = &reply.target
        {
            let selection = m::ModelSelection {
                provider: target.provider.clone(),
                model: target.model.clone(),
                thinking: target.thinking.requested.clone(),
            };
            self.commit(
                run_id,
                "model_selection",
                json!({ "selection": selection, "target": target, "reason": "initial selection" }),
            )
            .await?;
        }
        Ok(reply.target)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn managed_auth_result_stays_private_while_settled_event_is_redacted() {
        let events = Events::new(1);
        let session = Session(Arc::new(Inner {
            maintenance: Default::default(),
            previews: Mutex::new(BTreeMap::new()),
            id: 1,
            kernel: generation::Generation::empty(),
            workspace_options: WorkspaceOptions::default(),
            history_path: None,
            offline_records: Mutex::new(vec![]),
            events,
            interactions: Arc::new(interaction::Interactions::default()),
            presentation: Arc::new(presentation::Hub::default()),
            input_cancel: Cancellation::default(),
            state: Mutex::new(State {
                closed: false,
                next: 1,
                active: None,
                shells: BTreeMap::new(),
                commands: BTreeMap::new(),
                management: false,
                pending_inputs: 0,
                terminals: BTreeMap::new(),
                shutdown_result: None,
            }),
            settled: tokio::sync::Notify::new(),
            shutdown: tokio::sync::Mutex::new(()),
            coding: false,
            cwd: ".".into(),
        }));
        let private = Terminal {
            outcome: Outcome::Completed(json!({
                "status": "awaiting_authorization",
                "interaction": {
                    "url": "https://fixture/?state=private-state",
                    "user_code": "private-code",
                },
            })),
            cleanup_errors: vec![],
            partial_result: None,
        };
        let (release, ready) = tokio::sync::oneshot::channel();
        let run = session
            .start_with_public_result(true, public_auth_terminal, move |_, _, _| async move {
                ready.await.unwrap()
            })
            .unwrap();
        assert!(session.inspect(run).is_none());
        assert!(
            session
                .start(true, |_, _, _| async {
                    Terminal::failed(Fault::new("unused", "test", "unused"))
                })
                .is_err()
        );
        release.send(private.clone()).unwrap();
        assert_eq!(session.wait(run).await.unwrap(), private);
        assert_eq!(session.inspect(run).unwrap(), private);
        let settled = session
            .events()
            .into_iter()
            .find(|event| event.kind == "settled")
            .unwrap();
        assert_eq!(settled.payload, json!(public_auth_terminal(&private)));
        assert!(!json!(session.events()).to_string().contains("private-"));
        let ordinary = private.clone();
        let run = session
            .start(true, move |_, _, _| async { ordinary })
            .unwrap();
        assert_eq!(session.wait(run).await.unwrap(), private);
        assert_eq!(session.events().last().unwrap().payload, json!(private));
        session.shutdown().await.unwrap();
    }
    #[test]
    fn authentication_public_result_omits_interaction_and_unknown_private_fields() {
        let private = Terminal {
            outcome: Outcome::Completed(json!({
                "operation_id": "login-1",
                "provider": "fixture",
                "status": "awaiting_authorization",
                "source": null,
                "challenge": "oauth",
                "interaction": {
                    "url": "https://fixture/?state=private-state",
                    "user_code": "private-code",
                    "manual_input": true,
                    "expires_at": 123,
                },
                "future_private_field": "private-material",
            })),
            cleanup_errors: vec![Fault::new("Cleanup", "fixture", "cleanup failed")],
            partial_result: Some(json!({ "secret": "private-partial" })),
        };
        let public = public_auth_terminal(&private);
        assert!(public.partial_result.is_none());
        assert_eq!(
            public.outcome,
            Outcome::Completed(json!({
                "operation_id": "login-1",
                "provider": "fixture",
                "status": "awaiting_authorization",
                "source": null,
            }))
        );
        assert_eq!(public.cleanup_errors, private.cleanup_errors);
        assert!(json!(private)["outcome"]["value"]["interaction"]["url"].is_string());
    }
    #[test]
    fn authentication_public_result_preserves_failure_and_cancellation() {
        for outcome in [
            Outcome::Cancelled,
            Outcome::Failed(Fault::new("Auth", "fixture", "authorization denied")),
        ] {
            let terminal = Terminal {
                outcome,
                cleanup_errors: vec![],
                partial_result: None,
            };
            assert_eq!(public_auth_terminal(&terminal), terminal);
        }
    }
    #[test]
    fn branch_selection_uses_ancestors_not_latest_selection_on_another_branch() {
        let row = |sequence, parent_id, kind: &str, payload: Value| c::Record {
            schema_version: 2,
            session_id: 1,
            sequence,
            run_id: 1,
            parent_id,
            branch: "main".into(),
            kind: kind.into(),
            payload,
        };
        let records = vec![
            row(1, None, "session", json!({})),
            row(
                2,
                Some(1),
                "model_selection",
                json!({ "selection": { "provider": "a", "model": "one" } }),
            ),
            row(
                3,
                Some(2),
                "model_selection",
                json!({ "selection": { "provider": "b", "model": "two" } }),
            ),
            row(
                4,
                Some(2),
                "branch_selected",
                json!({ "target": 2, "branch": "main" }),
            ),
        ];
        assert_eq!(saved_selection(&records).unwrap().unwrap().provider, "a");
    }
}
