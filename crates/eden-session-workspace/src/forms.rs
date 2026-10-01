//! Transient management forms use existing host transactions and never become prompt history.
use crate::{Adapter, fault, projection::text, sessions::Server};
use eden_protocol::Fault;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::Mutex;

#[derive(Clone)]
enum Flow {
    AuthProviders,
    AuthMethods(String),
    AuthPending {
        operation: String,
        api_key: bool,
        submitted: bool,
        wait_run: Option<u64>,
    },
    ConfigInstances,
    Config {
        view: Value,
        node: Value,
    },
    Histories,
    Resources,
    History(String),
    Directory,
    Stop(crate::Opened),
    Tree {
        path: String,
        fork: bool,
    },
    Navigate {
        path: String,
        target: u64,
    },
    Copy {
        path: String,
        kind: String,
        target: Option<u64>,
    },
    Delete {
        plan: crate::RemovalPlan,
    },
    Restore {
        entry: crate::TrashEntry,
    },
    Rename {
        path: String,
    },
    CopyPreview(String),
    Done,
}
struct Form {
    source_session: String,
    owner: Arc<Adapter>,
    flow: Flow,
    revision: u64,
    busy: bool,
}
#[derive(Default)]
pub(crate) struct Forms(Mutex<BTreeMap<String, Form>>);

fn action(id: impl AsRef<str>, label: impl AsRef<str>) -> Value {
    json!({ "id": id.as_ref(), "label": label.as_ref() })
}
fn panel(
    title: &str,
    description: impl AsRef<str>,
    fields: Vec<Value>,
    actions: Vec<Value>,
) -> Value {
    json!({
        "title": title,
        "description": description.as_ref(),
        "fields": fields,
        "actions": actions,
    })
}
fn entry(path: &str, label: &str, control: &str, value: Value) -> Value {
    json!({
        "path": path,
        "label": label,
        "control": control,
        "value": value,
        "writable": true,
        "options": [],
    })
}
fn done(message: &str) -> (Flow, Value) {
    (Flow::Done, panel("Eden", message, vec![], vec![]))
}
async fn operation(owner: &Adapter, route: &str, mut body: Value) -> Result<Value, Fault> {
    body["request_id"] = json!(owner.request_id());
    let reply = owner.post(route, body).await?;
    owner
        .complete(
            reply["run_id"]
                .as_u64()
                .ok_or_else(|| fault("Missing operation identity"))?,
        )
        .await
}
async fn auth(owner: &Adapter, request: Value) -> Result<Value, Fault> {
    operation(owner, "/auth/start", json!({ "request": request })).await
}
fn find_form(nodes: &[Value], instance: &str) -> Option<Value> {
    for node in nodes {
        if node["kind"] == "configuration_form" && node["binding"]["instance"] == instance {
            return Some(node.clone());
        }
        if let Some(found) = node["children"]
            .as_array()
            .and_then(|children| find_form(children, instance))
        {
            return Some(found);
        }
    }
    None
}
async fn config(owner: &Adapter, instance: &str) -> Result<(Flow, Value), Fault> {
    owner
        .post("/configuration/open", json!({ "instance": instance }))
        .await?;
    let snapshot = owner.client.snapshot().await?;
    for view in serde_json::to_value(snapshot.presentation).map_err(|e| fault(e.to_string()))?
        ["views"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let Some(node) = view["nodes"]
            .as_array()
            .and_then(|nodes| find_form(nodes, instance))
        {
            let fields = node["fields"].as_array().cloned().unwrap_or_default();
            let result = panel(
                &format!("Configuration: {instance}"),
                "Scope: current Session override. Validate and preview show application/restart \
                 and affected instances. Ctrl+U clears a field; Ctrl+R restores inheritance.",
                fields,
                vec![
                    action("validate", "Validate"),
                    action("preview", "Preview application"),
                    action("apply", "Apply to this Session"),
                    action("refresh", "Refresh values"),
                ],
            );
            return Ok((
                Flow::Config {
                    view: view.clone(),
                    node,
                },
                result,
            ));
        }
    }
    Err(fault("The host did not publish a configuration form"))
}
async fn auth_pending(
    owner: &Adapter,
    reply: Value,
    api_key: bool,
) -> Result<(Flow, Value), Fault> {
    let operation = text(&reply["operation_id"]).to_owned();
    if operation.is_empty() {
        return Err(fault("Authentication did not return an operation"));
    }
    let wait_run = if !api_key {
        let reply = owner
            .post(
                "/auth/start",
                json!({
                    "request_id": owner.request_id(),
                    "request": { "action": "wait", "operation_id": operation },
                }),
            )
            .await?;
        Some(
            reply["run_id"]
                .as_u64()
                .ok_or_else(|| fault("Missing OAuth wait run"))?,
        )
    } else {
        None
    };
    Ok(auth_page(&reply, api_key, false, wait_run))
}

fn auth_page(
    reply: &Value,
    api_key: bool,
    submitted: bool,
    wait_run: Option<u64>,
) -> (Flow, Value) {
    let operation = text(&reply["operation_id"]).to_owned();
    let interaction = &reply["interaction"];
    let description = if api_key {
        "Enter the API key in this private field.".to_owned()
    } else {
        format!(
            "Open: {}\nUser code: {}\nExpires: {}\nComplete authorization in your browser, then \
             check status. Escape cancels this login.",
            text(&interaction["url"]),
            text(&interaction["user_code"]),
            interaction["expires_at"]
        )
    };
    let mut fields = vec![];
    let mut actions = vec![];
    if !submitted && (api_key || interaction["manual_input"] == true) {
        fields.push(entry(
            "input",
            if api_key {
                "API key"
            } else {
                "Authorization code / redirect URL"
            },
            "secret",
            Value::Null,
        ));
        actions.push(action("submit", "Submit private input"));
    }
    if !api_key {
        actions.push(action("open_browser", "Open authorization page"));
        actions.push(action("copy_url", "Copy authorization URL"));
    }
    actions.push(action("status", "Check status"));
    let mut page = panel(
        "Provider authentication",
        format!("Status: {}\n{description}", text(&reply["status"])),
        fields,
        actions,
    );
    page["authOperation"] = json!(operation);
    page["authUrl"] = interaction["url"].clone();
    page["preserveEdits"] = json!(!submitted);
    (
        Flow::AuthPending {
            operation,
            api_key,
            submitted,
            wait_run,
        },
        page,
    )
}

impl Forms {
    pub(crate) async fn dispatch(&self, server: &Server, params: &Value) -> Result<Value, Fault> {
        if params["catalogCancel"] == true {
            server.cancel_catalog();
            return Ok(json!({ "closed": true }));
        }
        if let Some(open) = params["open"].as_str() {
            let owner = server.target(params["sessionId"].as_str(), false).await?;
            let flow = match open {
                "auth" => Flow::AuthProviders,
                "config" => Flow::ConfigInstances,
                "sessions" => Flow::Histories,
                "history-remove" => Flow::Delete {
                    plan: server
                        .removal_for_reference(text(&params["reference"]))
                        .await?,
                },
                "history-restore" => Flow::Restore {
                    entry: server
                        .trash_for_reference(text(&params["reference"]))
                        .await?,
                },
                "history-rename" => Flow::Rename {
                    path: server.path_for_reference(text(&params["reference"]))?,
                },
                "history-details" => {
                    Flow::History(server.path_for_reference(text(&params["reference"]))?)
                }
                "resources" => Flow::Resources,
                _ => return Err(fault("Unknown management form")),
            };
            let id = text(&params["formId"]).to_owned();
            if id.is_empty() {
                return Err(fault("Missing form identity"));
            }
            {
                let mut forms = self.0.lock().await;
                if forms.contains_key(&id) {
                    return Err(fault("Form already open"));
                }
                forms.insert(
                    id.clone(),
                    Form {
                        source_session: text(&params["sessionId"]).to_owned(),
                        owner: owner.clone(),
                        flow: flow.clone(),
                        revision: 0,
                        busy: true,
                    },
                );
            }
            let result = execute(
                server,
                &owner,
                flow,
                if open == "history-details" {
                    "details"
                } else {
                    "open"
                },
                &json!({}),
            )
            .await;
            let mut forms = self.0.lock().await;
            let Some(form) = forms.get_mut(&id) else {
                drop(forms);
                if let Ok((flow, _)) = &result {
                    cleanup(server, &owner, flow).await;
                }
                return Ok(json!({ "closed": true }));
            };
            form.busy = false;
            let (flow, mut page) = result?;
            form.flow = flow;
            page["formId"] = json!(id);
            page["revision"] = json!(0);
            return Ok(page);
        }
        let id = text(&params["formId"]);
        let command = text(&params["action"]);
        let (owner, flow) = {
            let mut forms = self.0.lock().await;
            let form = forms
                .get_mut(id)
                .ok_or_else(|| fault("This form has closed; open it again"))?;
            if params["sessionId"] != form.source_session {
                return Err(fault("Form belongs to another Session"));
            }
            if command == "close" {
                let form = forms.remove(id).ok_or_else(|| fault("Form has closed"))?;
                drop(forms);
                cleanup(server, &form.owner, &form.flow).await;
                return Ok(json!({ "closed": true }));
            }
            if form.busy || params["revision"].as_u64() != Some(form.revision) {
                return Err(fault("Form operation is pending or stale"));
            }
            form.busy = true;
            (form.owner.clone(), form.flow.clone())
        };
        let result = execute(server, &owner, flow, command, params).await;
        let mut forms = self.0.lock().await;
        let Some(form) = forms.get_mut(id) else {
            drop(forms);
            if let Ok((flow, _)) = &result {
                cleanup(server, &owner, flow).await;
            }
            return Ok(json!({ "closed": true }));
        };
        form.busy = false;
        let (flow, mut page) = result?;
        form.flow = flow;
        form.revision += 1;
        page["formId"] = json!(id);
        page["revision"] = json!(form.revision);
        Ok(page)
    }
    pub(crate) async fn close_all(&self, server: &Server) {
        let forms = std::mem::take(&mut *self.0.lock().await);
        for (_, form) in forms {
            cleanup(server, &form.owner, &form.flow).await;
        }
    }
}
async fn cleanup(server: &Server, owner: &Adapter, flow: &Flow) {
    match flow {
        Flow::AuthPending {
            operation,
            wait_run,
            ..
        } => {
            if let Some(run) = wait_run {
                let _ = owner.client.cancel(*run).await;
                let _ = owner.client.wait(*run).await;
            }
            let _ = auth(
                owner,
                json!({ "action": "cancel", "operation_id": operation }),
            )
            .await;
        }
        Flow::CopyPreview(id) => {
            let _ = server
                .manage("/manage/copy/discard", json!({ "preview_id": id }))
                .await;
        }
        _ => {}
    }
}
async fn execute(
    server: &Server,
    owner: &Arc<Adapter>,
    flow: Flow,
    command: &str,
    params: &Value,
) -> Result<(Flow, Value), Fault> {
    match flow {
        Flow::Resources => {
            let mut diagnostic = None;
            if command == "reload" {
                if let Err(error) = operation(owner, "/resources/reload", json!({})).await {
                    diagnostic = Some(format!(
                        "Reload failed; previous inventory retained.\n{error}\n\n"
                    ));
                }
            } else if !matches!(command, "open" | "" | "refresh") {
                return Err(fault("Unknown resource action"));
            }
            let snapshot = owner.resource_inventory().await?;
            owner.publish_resources(&snapshot, false);
            let mut description = diagnostic.unwrap_or_default();
            description.push_str(&owner.resource_description(&snapshot).await?);
            let read_only = owner.view.lock().await.snapshot.state.read_only;
            Ok((
                Flow::Resources,
                panel(
                    "Skills and templates",
                    description,
                    vec![],
                    if read_only {
                        vec![]
                    } else {
                        vec![
                            action("reload", "Reload from disk"),
                            action("refresh", "Refresh inventory"),
                        ]
                    },
                ),
            ))
        }
        Flow::AuthProviders => {
            if let Some(provider) = command.strip_prefix("provider:") {
                return auth_methods(owner, provider).await;
            }
            let catalog = owner.post("/models/list", json!({})).await?;
            let providers: std::collections::BTreeSet<_> = catalog["models"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m["target"]["provider"].as_str())
                .collect();
            Ok((
                Flow::AuthProviders,
                panel(
                    "Provider authentication",
                    "Choose a provider. Authentication is stored privately by Eden; current model \
                     and new-session default are unchanged.",
                    vec![],
                    providers
                        .into_iter()
                        .map(|p| action(format!("provider:{p}"), p))
                        .collect(),
                ),
            ))
        }
        Flow::AuthMethods(provider) => {
            let methods = owner
                .post("/auth/methods", json!({ "provider": provider }))
                .await?;
            if !methods["methods"].as_array().is_some_and(|methods| {
                methods
                    .iter()
                    .any(|method| method.as_str().and_then(auth_method) == Some(command))
            }) {
                return Err(fault("Authentication method is not available"));
            }
            if command == "logout" || command == "refresh" {
                auth(owner, json!({ "action": command, "provider": provider })).await?;
                return Ok(done(if command == "logout" {
                    "Provider credentials removed. Model selections are unchanged."
                } else {
                    "OAuth credentials refreshed."
                }));
            }
            let api_key = command == "api_key";
            let request = if api_key {
                json!({ "action": "start", "provider": provider })
            } else {
                json!({ "action": "login", "provider": provider, "method": command })
            };
            auth_pending(owner, auth(owner, request).await?, api_key).await
        }
        Flow::AuthPending {
            operation: id,
            api_key,
            mut submitted,
            wait_run,
        } => {
            if command == "submit" {
                let input = params["inputs"]["input"]
                    .as_str()
                    .ok_or_else(|| fault("Enter private input"))?;
                let reply = owner
                    .post(
                        "/auth/input",
                        json!({ "operation_id": id, "api_key": api_key, "input": input }),
                    )
                    .await
                    .map_err(|_| {
                        fault(
                            "Private authentication input failed; check status or cancel and retry",
                        )
                    })?;
                if api_key {
                    owner
                        .complete(
                            reply["run_id"]
                                .as_u64()
                                .ok_or_else(|| fault("Missing authentication run"))?,
                        )
                        .await
                        .map_err(|_| {
                            fault("Authentication failed; check provider configuration and retry")
                        })?;
                }
                submitted = true;
            } else if command != "status" {
                return Err(fault("Unknown authentication action"));
            }
            let status = owner
                .post("/auth/status", json!({ "operation_id": id }))
                .await?;
            let description = format!("Authentication status: {}", text(&status["status"]));
            let complete = matches!(
                text(&status["status"]),
                "authenticated" | "completed" | "cancelled" | "failed" | "expired"
            );
            if complete {
                if let Some(run) = wait_run {
                    let terminal = owner.client.wait(run).await?;
                    if let Some(error) = terminal.cleanup_errors.first() {
                        return Err(error.clone());
                    }
                }
                return Ok(done(&description));
            }
            Ok(auth_page(&status, api_key, submitted, wait_run))
        }
        Flow::ConfigInstances => {
            if let Some(instance) = command.strip_prefix("instance:") {
                return config(owner, instance).await;
            }
            let inspection = owner.post("/configuration/inspect", json!({})).await?;
            let actions = inspection["instances"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|item| {
                    action(
                        format!("instance:{}", text(&item["id"])),
                        format!("{} · {}", text(&item["id"]), text(&item["state"])),
                    )
                })
                .collect();
            Ok((
                Flow::ConfigInstances,
                panel(
                    "Business configuration",
                    "Choose an instance. Values, sources, validation and application are owned by \
                     Eden.",
                    vec![],
                    actions,
                ),
            ))
        }
        Flow::Config { view, node } => {
            let instance = text(&node["binding"]["instance"]);
            if command == "refresh" {
                return config(owner, instance).await;
            }
            if !matches!(command, "validate" | "preview" | "apply") {
                return Err(fault("Unknown configuration action"));
            }
            let edits = params["edits"].as_array().cloned().unwrap_or_default();
            let private = params["privateEdits"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let request = json!({
                "session_id": owner.session,
                "owner": view["owner"],
                "view_id": view["id"],
                "revision": view["revision"],
                "action": format!("{}:{command}", text(&node["id"])),
                "request_id": owner.request_id(),
                "values": { "binding": node["binding"], "edits": edits },
            });
            let result = if private.is_empty() {
                owner.post("/action", request).await
            } else {
                owner
                    .post(
                        "/private-input",
                        json!({ "request": request, "inputs": private }),
                    )
                    .await
            }
            .map_err(|error| {
                if private.is_empty() {
                    error
                } else {
                    fault(
                        "Private configuration operation failed; refresh the form and re-enter \
                         private fields",
                    )
                }
            })?;
            let (next, mut page) = if command == "apply" {
                config(owner, instance).await?
            } else {
                (
                    Flow::Config {
                        view: view.clone(),
                        node: node.clone(),
                    },
                    panel(
                        &format!("Configuration: {instance}"),
                        "",
                        node["fields"].as_array().cloned().unwrap_or_default(),
                        vec![
                            action("validate", "Validate"),
                            action("preview", "Preview application"),
                            action("apply", "Apply to this Session"),
                            action("refresh", "Refresh values"),
                        ],
                    ),
                )
            };
            page["description"] = json!(configuration_feedback(command, &result));
            page["preserveEdits"] = json!(command != "apply");
            Ok((next, page))
        }
        Flow::Histories | Flow::Directory => {
            if command == "new" {
                let identity = server.create(owner).await?;
                return Ok((
                    Flow::Done,
                    json!({ "closed": true, "loadSession": identity }),
                ));
            }
            if command == "directory" {
                return Ok((
                    Flow::Directory,
                    panel(
                        "Session directory",
                        "Choose a directory to inspect explicitly.",
                        vec![entry(
                            "directory",
                            "Directory",
                            "text",
                            json!(owner.view.lock().await.snapshot.state.cwd),
                        )],
                        vec![action("list", "List this directory")],
                    ),
                ));
            }
            if let Some(path) = command.strip_prefix("history:") {
                let info = server.info(path).await?;
                return Ok((
                    Flow::History(path.into()),
                    panel(
                        "Saved session",
                        format!(
                            "{}\n{path}\nSession {} · {}\nTags: {}\nOwner: {}\nDiagnostic: \
                             {}\nOpen with its saved binding or read without execution. Copy \
                             operations preserve the source.",
                            text(&info["name"]),
                            info["session_id"],
                            text(&info["status"]),
                            info["tags"],
                            info["owner"],
                            info["diagnostic"]
                        ),
                        vec![entry("name", "Session name", "text", info["name"].clone())],
                        vec![
                            action("open", "Open / resume"),
                            action("read", "Read-only"),
                            action("stop", "Stop live host"),
                            action("rename", "Rename saved session"),
                            action("tree", "Tree / branch"),
                            action("fork", "Fork ancestry"),
                            action("clone", "Clone complete tree"),
                            action("migrate", "Preview migrated copy"),
                            action("recover", "Recover valid prefix to copy"),
                            action("upgrade", "Upgrade format to copy"),
                            action("delete", "Move to Trash"),
                        ],
                    ),
                ));
            }
            let listing = server
                .list(&json!({
                    "sessionId": owner.identity,
                    "directory": params["inputs"]["directory"],
                }))
                .await?;
            let mut actions = vec![
                action("new", "New Session in this cwd"),
                action("directory", "Choose directory"),
            ];
            actions.extend(
                listing["sessions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|row| {
                        action(
                            text(&row["sessionId"]),
                            format!(
                                "{} · {}{}",
                                text(&row["summary"]),
                                text(&row["sessionId"]).trim_start_matches("history:"),
                                row["_meta"]["edenDiagnostic"]
                                    .as_str()
                                    .map(|d| format!(" · {d}"))
                                    .unwrap_or_default()
                            ),
                        )
                    }),
            );
            Ok((
                Flow::Histories,
                panel(
                    "Eden sessions",
                    format!(
                        "Select a saved history. Escape returns to the current Session.{}",
                        listing["_meta"]["edenDiagnostics"]
                            .as_array()
                            .filter(|errors| !errors.is_empty())
                            .map(|errors| format!(
                                "\nDirectory diagnostics: {}",
                                errors
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .collect::<Vec<_>>()
                                    .join("; ")
                            ))
                            .unwrap_or_default()
                    ),
                    vec![],
                    actions,
                ),
            ))
        }
        Flow::History(path) => match command {
            "details" => {
                let info = server.info(&path).await?;
                Ok((
                    Flow::History(path.clone()),
                    panel(
                        "Session details",
                        format!(
                            "{}\n\n{} messages · {}\n{}\n{}",
                            text(&info["summary"]),
                            info["messages"],
                            text(&info["status"]),
                            path,
                            text(&info["diagnostic"])
                        ),
                        vec![entry(
                            "name",
                            "Name",
                            "text",
                            if info["has_name"] == true {
                                info["name"].clone()
                            } else {
                                json!("")
                            },
                        )],
                        vec![
                            action("open", "Resume"),
                            action("read", "Read-only"),
                            action("stop", "Stop live host"),
                            action("recover", "Recover a copy"),
                            action("upgrade", "Upgrade a copy"),
                            action("rename", "Save name"),
                            action("tree", "Branches"),
                            action("fork", "Fork"),
                            action("clone", "Clone"),
                            action("migrate", "Migrate a copy"),
                            action("delete", "Move to Trash"),
                        ],
                    ),
                ))
            }

            "open" | "read" => {
                let identity = server.open_history(&path, command == "read").await?;
                Ok((
                    Flow::Done,
                    json!({ "loadSession": identity, "closed": true }),
                ))
            }
            "stop" => Ok((
                Flow::Stop(server.owner(&path).await?),
                panel(
                    "Stop live host",
                    format!(
                        "Stop the writer for {path}. Accepted work will be cancelled and cleaned \
                         up by its owner. The saved history remains available."
                    ),
                    vec![entry(
                        "confirmed",
                        "Stop this host",
                        "boolean",
                        json!(false),
                    )],
                    vec![action("stop", "Stop confirmed host")],
                ),
            )),
            "rename" => {
                server
                    .manage(
                        "/manage/rename",
                        json!({
                            "request_id": owner.request_id(),
                            "path": path,
                            "name": params["inputs"]["name"],
                            "preserve_tags": true,
                            "tags": [],
                        }),
                    )
                    .await?;
                Ok(done(
                    "Saved session renamed. Reopen /sessions to see the current name.",
                ))
            }
            "tree" | "fork" => {
                let records = server
                    .manage("/manage/tree", json!({ "path": path }))
                    .await?;
                let actions = records
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|r| {
                        action(
                            r["sequence"].to_string(),
                            format!(
                                "#{} ← {} · {} · {}",
                                r["sequence"],
                                r["parent_id"],
                                text(&r["branch"]),
                                text(&r["kind"])
                            ),
                        )
                    })
                    .collect();
                Ok((
                    Flow::Tree {
                        path,
                        fork: command == "fork",
                    },
                    panel(
                        "Session tree",
                        "Select a record. Parent and branch identify its ancestry; navigation \
                         never replays tools.",
                        vec![],
                        actions,
                    ),
                ))
            }
            "clone" | "migrate" | "recover" | "upgrade" => {
                Ok(copy_page(path, command.into(), None))
            }
            "delete" => {
                let plan = server.removal_for_path(&path).await?;
                Ok((Flow::Delete { plan: plan.clone() }, removal_page(&plan)))
            }
            _ => Err(fault("Unknown saved-session action")),
        },
        Flow::Tree { path, fork } => {
            let target = command
                .parse::<u64>()
                .map_err(|_| fault("Select a tree record"))?;
            if fork {
                return Ok(copy_page(path, "fork".into(), Some(target)));
            }
            Ok((
                Flow::Navigate { path, target },
                panel(
                    "Branch from selected record",
                    format!(
                        "Target #{target}; choose a branch label. Existing branches retain their \
                         history."
                    ),
                    vec![
                        entry("branch", "Branch label", "text", json!("branch")),
                        entry(
                            "summarize",
                            "Summarize before navigation",
                            "boolean",
                            json!(false),
                        ),
                    ],
                    vec![action("navigate", "Navigate to this head")],
                ),
            ))
        }
        Flow::Navigate { path, target } => {
            let selected = server.selected(&path).await?;
            operation(
                &selected,
                "/manage/navigate",
                json!({
                    "target": target,
                    "branch": params["inputs"]["branch"],
                    "summarize":
                        params["inputs"]["summarize"].as_bool().unwrap_or(false),
                }),
            )
            .await?;
            Ok((
                Flow::Done,
                json!({ "closed": true, "loadSession": selected.identity }),
            ))
        }
        Flow::Copy { path, kind, target } => {
            let reply = server
                .manage(
                    "/manage/copy/preview",
                    json!({
                        "request_id": owner.request_id(),

                        "source": path,
                        "destination": params["inputs"]["destination"],
                        "kind": kind,
                        "target": target,
                        "cwd": params["inputs"]["cwd"],
                    }),
                )
                .await?;
            Ok((
                Flow::CopyPreview(text(&reply["preview_id"]).into()),
                panel(
                    "Review session copy",
                    migration_preview(&reply["plan"]),
                    vec![],
                    vec![action("apply", "Create this copy and open it")],
                ),
            ))
        }
        Flow::Stop(opened) => {
            if params["inputs"]["confirmed"] != true {
                return Err(fault("Confirm the selected host before stopping it"));
            }
            server.stop_history(&opened).await?;
            Ok(done(
                "Host stopped. Cleanup completed and its writer released; history is preserved.",
            ))
        }
        Flow::Delete { plan } => {
            if command == "open" {
                return Ok((Flow::Delete { plan: plan.clone() }, removal_page(&plan)));
            }
            if command == "stop_legacy" && !plan.reviewed_stop {
                let opened = plan
                    .owner
                    .as_ref()
                    .ok_or_else(|| fault("The selected host is no longer available"))?;
                server.stop_history(opened).await?;
                let next = server
                    .removal_for_path(&plan.history.path.to_string_lossy())
                    .await?;
                return Ok((Flow::Delete { plan: next.clone() }, removal_page(&next)));
            }
            if command != "remove" {
                return Err(fault("Choose Move to Trash or cancel"));
            }
            let current =
                owner.opened.as_ref().and_then(|o| o.history.as_ref()) == Some(&plan.history.path);
            let item = server.remove_reviewed(plan).await?;
            let mut page = json!({
                "closed": true,
                "notice": format!("Moved {} to Trash", item.title),
                "showSessions": !current,
                "preserveDraft": current,
            });
            if current {
                match server.create(owner).await {
                    Ok(load) => page["loadSession"] = json!(load),
                    Err(error) => {
                        page["closed"] = json!(false);
                        page["title"] = json!("Session moved to Trash");
                        page["description"] = json!(format!(
                            "The deletion completed. Starting the new draft failed: {error}. Your \
                             history can be restored from Trash."
                        ));
                        page["actions"] = json!([]);
                    }
                }
            }
            Ok((Flow::Done, page))
        }
        Flow::Restore { entry } => {
            if command == "open" {
                return Ok((
                    Flow::Restore {
                        entry: entry.clone(),
                    },
                    panel(
                        "Restore session",
                        format!(
                            "{}\n\nRestore this session to its original project. Existing history \
                             is never overwritten.\n{}",
                            entry.title,
                            entry.original.display()
                        ),
                        vec![],
                        vec![action("restore", "Restore session")],
                    ),
                ));
            }
            if command != "restore" {
                return Err(fault("Choose Restore session or cancel"));
            }
            server.restore_reviewed(&entry).await?;
            Ok((
                Flow::Done,
                json!({ "closed": true, "notice": "Session restored", "showSessions": true }),
            ))
        }
        Flow::Rename { path } => {
            if command == "open" {
                let info = server.info(&path).await?;
                return Ok((
                    Flow::Rename { path },
                    panel(
                        "Rename session",
                        "Give this history a name you can find later.",
                        vec![entry(
                            "name",
                            "Name",
                            "text",
                            if info["has_name"] == true {
                                info["name"].clone()
                            } else {
                                json!("")
                            },
                        )],
                        vec![action("rename", "Save name")],
                    ),
                ));
            }
            server
                .manage(
                    "/manage/rename",
                    json!({
                        "path": path,
                        "name": params["inputs"]["name"],
                        "preserve_tags": true,
                        "request_id": owner.request_id(),
                    }),
                )
                .await?;
            Ok((
                Flow::Done,
                json!({ "closed": true, "notice": "Session renamed", "showSessions": true }),
            ))
        }
        Flow::CopyPreview(preview) => {
            if command != "apply" {
                return Err(fault("Review the copy before applying"));
            }
            let result = server
                .manage(
                    "/manage/copy/apply",
                    json!({
                        "request_id": owner.request_id(),
                        "preview_id": preview,
                        "confirmed": true,
                    }),
                )
                .await?;
            let created = text(&result["created"]);
            let identity = server.open_history(created, false).await?;
            Ok((
                Flow::Done,
                json!({ "closed": true, "loadSession": identity }),
            ))
        }
        Flow::Done => Ok(done("Completed.")),
    }
}
async fn auth_methods(owner: &Adapter, provider: &str) -> Result<(Flow, Value), Fault> {
    let methods = owner
        .post("/auth/methods", json!({ "provider": provider }))
        .await?;
    let actions: Vec<_> = methods["methods"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|method| auth_method(method).map(|id| action(id, method)))
        .collect();
    Ok((
        Flow::AuthMethods(provider.into()),
        panel("Authentication method", provider, vec![], actions),
    ))
}

fn auth_method(label: &str) -> Option<&'static str> {
    match label {
        "API key" => Some("api_key"),
        "Browser login" => Some("browser"),
        "Device login" => Some("device"),
        "Refresh OAuth" => Some("refresh"),
        "Log out" => Some("logout"),
        _ => None,
    }
}

fn lines(value: &Value) -> String {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}
fn migration_preview(plan: &Value) -> String {
    let mut result = format!(
        "Source: {}\nNew copy: {}\nWorking directory: {}\n\nPreserves:\n",
        text(&plan["source"]),
        text(&plan["destination"]),
        text(&plan["cwd"])
    );
    for item in plan["preserved"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        result.push_str(&format!("• {item}\n"));
    }
    result.push_str("\nChanges / losses:\n");
    for item in plan["losses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        result.push_str(&format!("• {item}\n"));
    }
    result
}
fn configuration_feedback(command: &str, result: &Value) -> String {
    let mut message = "Scope: current Session override.\n".to_owned();
    match command {
        "validate" => {
            let errors = result["errors"].as_array();
            if errors.is_none_or(Vec::is_empty) {
                message.push_str("Validation passed.");
            } else {
                message.push_str("Validation failed:\n");
                for error in errors.into_iter().flatten() {
                    message.push_str(&format!(
                        "{}: {} ({})\n",
                        text(&error["path"]),
                        text(&error["message"]),
                        text(&error["code"])
                    ));
                }
            }
        }
        "preview" => {
            message.push_str(&format!(
                "Application: {}\nAffects: {}\nSave target: {}",
                text(&result["application"]),
                lines(&result["affected"]),
                text(&result["edit_target"])
            ));
            let waiting = result["waiting_runs"].as_array().map_or(0, Vec::len);
            if waiting > 0 {
                message.push_str(&format!(
                    "\nWaits for {waiting} running operation(s) to finish."
                ));
            }
        }
        _ => {
            message.push_str(match text(&result["status"]) {
                "applied" => "Applied to this Session.",
                "restored" => "Apply failed; previous configuration restored.",
                "recovery_failed" => "Apply and recovery failed.",
                "cleanup_failed" => "Configuration cleanup failed.",
                _ => "Configuration operation settled.",
            });
            for key in ["error", "recovery_error"] {
                if let Some(error) = result[key]["message"].as_str() {
                    message.push_str(&format!("\n{error}"));
                }
            }
        }
    }
    message
}

fn copy_page(path: String, kind: String, target: Option<u64>) -> (Flow, Value) {
    let page = panel(
        "Session copy",
        format!("{kind}: {path}; target {target:?}. Review the plan before creating a copy."),
        vec![
            entry(
                "destination",
                "New copy path",
                "text",
                json!(format!("{path}.{kind}.jsonl")),
            ),
            entry(
                "cwd",
                "Clone cwd (blank keeps saved binding)",
                "text",
                json!(""),
            ),
        ],
        vec![action("preview", "Preview copy")],
    );
    (Flow::Copy { path, kind, target }, page)
}

fn removal_page(plan: &crate::RemovalPlan) -> Value {
    if !plan.reviewed_stop && plan.owner.is_some() {
        return panel(
            "Stop older session before removal?",
            format!(
                "{}\n\nThis host uses an older session protocol. Stop all work in this host \
                 first. Its history will remain saved, then you can review moving it to \
                 Trash.\n\n{}",
                plan.title,
                plan.history.path.display()
            ),
            vec![],
            vec![action("stop_legacy", "Stop all work and review removal")],
        );
    }
    let busy =
        plan.active_run.is_some() || !plan.shell_runs.is_empty() || !plan.command_runs.is_empty();
    let consequence = if busy {
        "The current task will be stopped. Its tools will finish cancellation before the history \
         moves to Trash. Other attached views will disconnect."
    } else if plan.owner.is_some() {
        "The idle session will close and its history will move to Trash. Other attached views will \
         disconnect."
    } else {
        "The history will move to Trash. You can restore it later, including after restarting Eden."
    };
    panel(
        "Move session to Trash?",
        format!(
            "{}\n\n{consequence}\n\n{}",
            plan.title,
            plan.history.path.display()
        ),
        vec![],
        vec![action(
            "remove",
            if busy {
                "Stop task and move to Trash"
            } else if plan.owner.is_some() {
                "Close session and move to Trash"
            } else {
                "Move to Trash"
            },
        )],
    )
}
