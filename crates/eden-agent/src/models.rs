//! Session model intent belongs to the active history path, independently of global defaults.
use super::*;
use eden_protocol::models as m;
#[cfg(test)]
use serde_json::Value;
use serde_json::json;

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
    /// Authentication input travels only through the private contract; terminal status is redacted.
    pub fn authenticate(&self, request: m::AuthRequest) -> Result<u64, Fault> {
        self.start(true, move |session, run_id, cancel| async move {
            session
                .0
                .kernel
                .invoke(
                    Request {
                        session_id: session.id(),
                        run_id,
                        contract: m::AUTH.into(),
                        payload: json!(request),
                    },
                    cancel,
                )
                .await
        })
    }
    pub(crate) async fn freeze_model(&self, run_id: u64) -> Result<Option<m::ModelTarget>, Fault> {
        if self.role(m::MODEL_CATALOG).is_err() {
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
