// Modified by Eden Agent for its terminal library; see frontend/grok/EDEN-FRONTEND.md.
//! Direct typed presentation channel. Eden owns lifecycle; no local JSON-RPC transport exists.
use agent_client_protocol as acp;
use anyhow::Result;
use eden_session_workspace::{
    Request, SessionOperation, SessionWorkspace, ViewEvent, ViewEventKind,
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use xai_acp_lib::{AcpAgentMessage, acp_channels, acp_send};

fn error(error: impl std::fmt::Display) -> acp::Error {
    acp::Error::new(-32603, error.to_string())
}
fn fault_error(fault: eden_session_workspace::Fault) -> acp::Error {
    if fault.code == "NotAccepted" {
        acp::Error::new(-32603, fault.message).data(json!({"edenNotAccepted": true}))
    } else {
        error(&fault)
    }
}
fn selected(view: Option<String>, operation: SessionOperation) -> Request {
    Request::View { view, operation }
}
fn meta_text(meta: &Option<acp::Meta>, key: &str) -> Option<String> {
    meta.as_ref()
        .and_then(|meta| meta.get(key))
        .and_then(Value::as_str)
        .map(str::to_owned)
}
fn extension(request: &acp::ExtRequest) -> Result<Request, acp::Error> {
    let data: Value = serde_json::from_str(request.params.get()).map_err(error)?;
    let view = data["sessionId"]
        .as_str()
        .or_else(|| data["session_id"].as_str())
        .map(str::to_owned);
    Ok(match request.method.trim_start_matches('_') {
        "eden/ui" => Request::Form(data),
        "x.ai/session/list" => Request::Catalog(data),
        "x.ai/prompt_history" => selected(view, SessionOperation::PromptHistory),
        "x.ai/compact_conversation" => selected(view, SessionOperation::Compact),
        "x.ai/session/rename" => selected(
            view,
            SessionOperation::Rename {
                title: data["title"]
                    .as_str()
                    .ok_or_else(|| error("missing title"))?
                    .into(),
            },
        ),
        "eden/model/default" => selected(
            view,
            SessionOperation::DefaultModel {
                model_id: data["modelId"]
                    .as_str()
                    .ok_or_else(|| error("missing model"))?
                    .into(),
                effort: data["_meta"]["reasoningEffort"].as_str().map(str::to_owned),
            },
        ),
        method => return Err(error(format!("Unsupported presentation action: {method}"))),
    })
}

pub(crate) async fn connect(
    workspace: Arc<SessionWorkspace>,
    mut events: mpsc::UnboundedReceiver<ViewEvent>,
    cancel: CancellationToken,
) -> Result<(super::AcpConnection, tokio::task::JoinHandle<Result<()>>)> {
    let initialized = workspace
        .request(selected(None, SessionOperation::Initialize))
        .await?;
    let models = serde_json::from_value::<acp::SessionModelState>(
        initialized["_meta"]["modelState"].clone(),
    )?;
    let commands = serde_json::from_value(initialized["_meta"]["availableCommands"].clone())?;
    let (client, mut agent) = acp_channels();
    let stop = cancel.clone();
    let task = tokio::spawn(async move {
        let tx = agent.tx.clone();
        let notifications = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                match event {
                    ViewEvent::Barrier(done) => {
                        let _ = done.send(());
                    }
                    ViewEvent::Notification { kind, data } => match kind {
                        ViewEventKind::Update => {
                            let update: acp::SessionNotification = serde_json::from_value(data)?;
                            acp_send(update, &tx)
                                .await
                                .map_err(|e| anyhow::anyhow!("{e}"))?;
                        }
                        kind => {
                            let method = match kind {
                                ViewEventKind::Title => "eden/session/title",
                                ViewEventKind::Context => "eden/context/state",
                                ViewEventKind::Models => "eden/model/state",
                                ViewEventKind::PromptComplete => "x.ai/session/prompt_complete",
                                ViewEventKind::Queue => "x.ai/queue/changed",
                                ViewEventKind::CatalogProgress => "eden/session/list_progress",
                                ViewEventKind::Update => unreachable!(),
                            };
                            let notification = acp::ExtNotification::new(
                                method,
                                serde_json::value::to_raw_value(&data)?.into(),
                            );
                            acp_send(notification, &tx)
                                .await
                                .map_err(|e| anyhow::anyhow!("{e}"))?;
                        }
                    },
                }
            }
            Ok::<_, anyhow::Error>(())
        });
        let mut requests = tokio::task::JoinSet::new();
        loop {
            let message = tokio::select! {
                _ = stop.cancelled() => break,
                message = agent.rx.recv() => match message { Some(message) => message, None => break },
            };
            let workspace = workspace.clone();
            requests.spawn(async move {
                macro_rules! respond {
                    ($args:expr, $request:expr) => {{
                        let result = workspace
                            .request($request)
                            .await
                            .map_err(fault_error)
                            .and_then(|value| serde_json::from_value(value).map_err(error));
                        let _ = $args.response_tx.send(result);
                    }};
                }
                match message {
                    AcpAgentMessage::Initialize(args) => {
                        respond!(args, selected(None, SessionOperation::Initialize))
                    }
                    AcpAgentMessage::Authenticate(args) => {
                        respond!(args, selected(None, SessionOperation::Authenticate))
                    }
                    AcpAgentMessage::LoadSession(args) => respond!(
                        args,
                        selected(
                            Some(args.request.session_id.to_string()),
                            SessionOperation::Load
                        )
                    ),
                    AcpAgentMessage::NewSession(args) => respond!(args, Request::New),
                    AcpAgentMessage::Prompt(args) => {
                        let input = &args.request;
                        let blocks = input
                            .prompt
                            .iter()
                            .map(serde_json::to_value)
                            .collect::<Result<Vec<_>, _>>();
                        match blocks {
                            Ok(blocks) => respond!(
                                args,
                                selected(
                                    Some(input.session_id.to_string()),
                                    SessionOperation::Prompt {
                                        blocks,
                                        prompt_id: meta_text(&input.meta, "promptId")
                                    }
                                )
                            ),
                            Err(e) => {
                                let _ = args.response_tx.send(Err(error(e)));
                            }
                        }
                    }
                    AcpAgentMessage::Cancel(args) => {
                        let result = workspace
                            .request(selected(
                                Some(args.request.session_id.to_string()),
                                SessionOperation::Cancel {
                                    prompt_id: meta_text(&args.request.meta, "promptId"),
                                },
                            ))
                            .await
                            .map(|_| ())
                            .map_err(error);
                        let _ = args.response_tx.send(result);
                    }
                    AcpAgentMessage::SetSessionModel(args) => respond!(
                        args,
                        selected(
                            Some(args.request.session_id.to_string()),
                            SessionOperation::SelectModel {
                                model_id: args.request.model_id.to_string(),
                                effort: meta_text(&args.request.meta, "reasoningEffort")
                            }
                        )
                    ),
                    AcpAgentMessage::ExtMethod(args) => {
                        let result = match extension(&args.request) {
                            Ok(request) => workspace
                                .request(request)
                                .await
                                .map_err(error)
                                .and_then(|value| {
                                    serde_json::value::to_raw_value(&value)
                                        .map(|raw| acp::ExtResponse::new(raw.into()))
                                        .map_err(error)
                                }),
                            Err(e) => Err(e),
                        };
                        let _ = args.response_tx.send(result);
                    }
                    AcpAgentMessage::SetSessionMode(args) => {
                        let _ = args
                            .response_tx
                            .send(Err(error("Use Eden configuration for session modes")));
                    }
                    AcpAgentMessage::ExtNotification(args) => {
                        let _ = args.response_tx.send(Ok(()));
                    }
                }
            });
            while requests.try_join_next().is_some() {}
        }
        requests.shutdown().await;
        let result = workspace.close().await;
        notifications.abort();
        let _ = notifications.await;
        result.map_err(Into::into)
    });
    Ok((
        super::AcpConnection {
            tx: client.tx,
            rx: client.rx,
            models: Some(models).into(),
            available_commands: commands,
            is_grok_shell: false,
            auth_methods: vec![],
            cancel,
            agent_thread: None,
            needs_login: false,
            login_label: None,
            login_method_id: None,
            auth_start_mode: super::AuthStartMode::Pending,
            auth_meta: None,
            leader_status_rx: None,
            cancel_rewind_enabled: false,
            session_recap_available: false,
            feedback_trace_offer: false,
            auth_manager: None,
        },
        task,
    ))
}
