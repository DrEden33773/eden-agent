//! Product management shares the normal dialog controls and host-owned operation receipts.
use crate::{
    app::{App, Update},
    model::{Dialog, Field},
};
mod workflows;
use eden_protocol::Fault;
use serde_json::{Value, json};

#[derive(Default)]
pub(crate) struct Management {
    pub generation: u64,
    pub page: String,
    pub items: Vec<(String, String)>,
    pub data: Value,
    pub form: Option<Operation>,
    pub run: Option<u64>,
    pub run_route: String,
    pub auth: Option<Value>,
    pub last_error: Option<Fault>,
}
#[derive(Clone)]
pub(crate) struct Operation {
    route: String,
    body: Value,
    private: bool,
}
fn text(key: &str, label: &str, value: &str) -> Field {
    let mut field = Field::text(label, value);
    field.key = key.into();
    field
}
fn choice(key: &str, label: &str, value: &str, choices: &[&str]) -> Field {
    let mut field = text(key, label, value);
    field.kind = "choice".into();
    field.options = choices.iter().map(|s| (*s).into()).collect();
    field
}
fn boolean(key: &str, label: &str) -> Field {
    let mut field = text(key, label, "false");
    field.kind = "boolean".into();
    field
}
impl App {
    pub(crate) fn open_management(&mut self, page: &str) {
        self.management.generation += 1;
        self.management.page = page.into();
        self.management.form = None;
        self.management.items.clear();
        self.management.data = Value::Null;
        self.open(&format!("manage:{page}"));
        let route = match page {
            "models" | "auth" => "/models/list",
            "router" => "/router/list",
            "settings" | "plugins" => "/configuration/inspect",
            "resources" => "/resources",
            "sessions" => "/manage/sessions",
            "tree" => {
                self.management.data = json!(self.snapshot.history);
                self.management.items = self
                    .snapshot
                    .history
                    .iter()
                    .filter(|r| r.kind != "branch_selected")
                    .map(|r| {
                        (
                            r.sequence.to_string(),
                            format!(
                                "{} ← {} · {} · {}",
                                r.sequence,
                                r.parent_id
                                    .map_or_else(|| "root".into(), |id| id.to_string()),
                                r.branch,
                                r.kind
                            ),
                        )
                    })
                    .collect();
                return;
            }
            _ => return,
        };
        self.notice = "Loading…".into();
        self.dispatch(
            route,
            json!({ "management": self.management.generation, "query": true }),
        );
    }
    pub(crate) fn management_choices(&self, page: &str) -> Vec<(String, String)> {
        let mut items = vec![];
        let actions: &[(&str, &str)] = match page {
            page if workflows::actions(page).is_some() => {
                workflows::actions(page).unwrap_or_default()
            }
            "models" => &[
                ("refresh", "Refresh catalog"),
                ("cycle", "Cycle configured models"),
                ("source", "Catalog source"),
                (
                    "current",
                    "Current model / requested and effective thinking",
                ),
            ],
            "router" => &[
                ("search", "Search repositories"),
                ("download", "Download model"),
                ("reconnect", "Reconnect and observe remote state"),
            ],
            "auth" => &[],
            "selected-session" => &[
                ("resume", "Resume stopped history (no automatic execution)"),
                ("read", "Open read only"),
                ("fork", "Fork selected ancestry"),
                ("clone", "Clone the complete tree"),
                ("recover", "Recover validated prefix into a new file"),
                ("upgrade", "Upgrade legacy history into a new file"),
                ("rename", "Rename / tag this stopped session"),
                ("delete", "Delete selected session"),
            ],
            "copy-preview" => &[
                ("inspect", "Inspect frozen copy plan"),
                ("apply", "Apply this reviewed copy plan"),
                ("discard", "Discard copy preview"),
            ],
            "resources" => &[
                ("reload", "Reload resources at a safe boundary"),
                ("trust", "Project trust"),
            ],
            "sessions" => &[
                ("metadata", "Rename / tag the current session"),
                ("tree", "Current session tree"),
                ("directory", "Browse another directory"),
            ],
            "tree" => &[("metadata", "Rename / tag the current session")],
            _ => &[],
        };
        items.extend(actions.iter().map(|(id, label)| {
            (
                if matches!(page, "selected-session" | "copy-preview") {
                    (*id).into()
                } else {
                    format!("action:{id}")
                },
                (*label).into(),
            )
        }));
        if let Some(run) = self.management.run {
            items.push((
                "action:wait".into(),
                format!("Observe operation {run} completion"),
            ));
            items.push((
                "action:cancel".into(),
                format!("Cancel operation {run} and wait for cleanup"),
            ));
        }
        if self.management.last_error.is_some() {
            items.push((
                "action:error".into(),
                "Last operation error and plugin source".into(),
            ));
            items.push((
                "action:copy-error".into(),
                "Copy last operation diagnostic".into(),
            ));
        }
        if self.management.auth.is_some() && page == "auth" {
            items.push((
                "action:active-auth".into(),
                "Return to the active authentication".into(),
            ));
        }
        items.extend(self.management.items.clone());
        items
    }
    fn management_form(
        &mut self,
        title: &str,
        route: &str,
        body: Value,
        fields: Vec<Field>,
        private: bool,
    ) {
        self.management.generation += 1;
        self.form_target = None;
        self.management.form = Some(Operation {
            route: route.into(),
            body,
            private,
        });
        self.dialog = Some(Dialog::Form {
            title: title.into(),
            fields,
            selected: 0,
            status: "Ctrl+S Apply · Esc Back".into(),
        });
    }
    pub(crate) fn choose_management(&mut self, page: &str, id: &str) {
        if self.workflow_choice(page, id) {
            return;
        }
        if id == "action:error" {
            self.management_detail("Operation diagnostic", json!(self.management.last_error));
            return;
        }
        if id == "action:copy-error" {
            self.copied = self
                .management
                .last_error
                .as_ref()
                .map(|error| json!(error).to_string());
            return;
        }
        if id == "action:active-auth" {
            self.management.page = "auth-challenge".into();
            self.management.items.clear();
            self.open("manage:auth-challenge");
            return;
        }
        if id == "action:wait" {
            if let Some(run) = self.management.run {
                self.dispatch(
                    "/terminal",
                    json!({
                        "run_id": run,
                        "management": self.management.generation,
                        "operation": self.management.run_route,
                    }),
                );
            }
            return;
        }
        if id == "action:cancel" {
            if let Some(run) = self.management.run {
                self.dispatch("/cancel", json!({ "run_id": run }));
                self.notice = "Cancellation requested; waiting for cleanup".into();
            }
            return;
        }
        match (page, id) {
            ("sessions", "action:directory") => self.management_form(
                "Saved session directory",
                "/manage/sessions",
                json!({ "directory": "" }),
                vec![text(
                    "/directory",
                    "Directory",
                    &format!("{}/.eden/sessions", self.snapshot.state.cwd),
                )],
                false,
            ),
            ("selected-session", "resume" | "read") => self.management_request(
                "/manage/open",
                json!({ "path": self.management.data["path"], "read_only": id == "read" }),
            ),
            ("selected-session", "delete") => self.management_form(
                "Delete selected saved session",
                "/manage/delete",
                json!({
                    "path": self.management.data["path"],
                    "expected_session": self.management.data["session_id"],
                    "confirmed": false,
                }),
                vec![
                    {
                        let mut f = text(
                            "",
                            "Path",
                            self.management.data["path"].as_str().unwrap_or(""),
                        );
                        f.readonly = true;
                        f
                    },
                    boolean("/confirmed", "Delete this session permanently"),
                ],
                false,
            ),
            ("selected-session", "fork" | "clone" | "recover" | "upgrade") => self.management_form(
                "Preview session copy",
                "/manage/copy/preview",
                json!({
                    "source": self.management.data["path"],
                    "destination": "",
                    "kind": id,
                    "target": null,
                }),
                vec![text("/destination", "New history path", "")],
                false,
            ),
            ("copy-preview", "apply") => self.management_request(
                "/manage/copy/apply",
                json!({ "preview_id": self.management.data["preview_id"], "confirmed": true }),
            ),
            ("copy-preview", "inspect") => {
                self.management_detail("Copy plan", self.management.data.clone())
            }
            ("copy-preview", "discard") => self.management_request(
                "/manage/copy/discard",
                json!({ "preview_id": self.management.data["preview_id"] }),
            ),
            (_, "action:metadata") => self.management_form(
                "Session name and tags",
                "/manage/metadata",
                json!({ "name": "", "tags": [] }),
                vec![text("/name", "Name", ""), {
                    let mut f = text("/tags", "Tags (JSON array)", "[]");
                    f.kind = "json".into();
                    f
                }],
                false,
            ),
            (_, "action:tree") => self.open_management("tree"),
            ("models", "action:cycle") => self.management_request("/models/cycle", json!({})),
            ("models", "action:refresh") => self.management_request(
                "/models/catalog",
                json!({ "request": { "action": "refresh" } }),
            ),
            ("models", "action:source") => self.management_form(
                "Catalog source",
                "/models/catalog",
                json!({ "request": { "action": "set_source", "url": "" } }),
                vec![text("/request/url", "URL", "")],
                false,
            ),
            ("models", "action:current") => self.dispatch(
                "/models/current",
                json!({ "management": self.management.generation, "detail": true }),
            ),
            ("models", _) => {
                let Some(entry) = self.management.data["models"]
                    .as_array()
                    .and_then(|models| models.iter().find(|m| model_id(m) == id))
                    .cloned()
                else {
                    return;
                };
                let target = &entry["target"];
                let body = json!({
                    "selection": {
                        "provider": target["provider"],
                        "model": target["model"],
                        "thinking": target["thinking"]["requested"],
                    },
                    "save_default": false,
                });
                self.management_form(
                    &format!(
                        "{} · {}",
                        entry["name"].as_str().unwrap_or(id),
                        entry["status"].as_str().unwrap_or("unknown")
                    ),
                    "/models/select",
                    body,
                    vec![
                        choice(
                            "/selection/thinking",
                            "Requested thinking",
                            target["thinking"]["requested"].as_str().unwrap_or("off"),
                            &["off", "minimal", "low", "medium", "high", "xhigh", "max"],
                        ),
                        boolean("/save_default", "Save as global default"),
                    ],
                    false,
                );
            }
            ("router", "action:search") => self.management_form(
                "Search router models",
                "/router/manage",
                json!({ "request": { "action": "search", "query": "" } }),
                vec![text("/request/query", "Query", "")],
                false,
            ),
            ("router", "action:download") => self.management_form(
                "Download model",
                "/router/manage",
                json!({ "request": { "action": "download", "model": "" } }),
                vec![text("/request/model", "Repository:quantization", "")],
                false,
            ),
            ("router", "action:reconnect") => self.management_request(
                "/router/manage",
                json!({ "request": { "action": "reconnect" } }),
            ),
            ("router", _) => self.management_form(
                id,
                "/router/manage",
                json!({ "request": { "action": "load", "model": id, "unload_others": false } }),
                vec![
                    choice(
                        "/request/action",
                        "Action",
                        "load",
                        &["load", "unload", "cancel", "download"],
                    ),
                    boolean("/request/unload_others", "Unload other models"),
                ],
                false,
            ),
            ("auth", _) => self.management_form(
                &format!("Authentication · {id}"),
                "/auth/start",
                json!({ "request": { "action": "login", "provider": id, "method": null } }),
                vec![
                    choice(
                        "/request/action",
                        "Method",
                        "login",
                        &["login", "start", "refresh", "logout"],
                    ),
                    choice(
                        "/request/method",
                        "Browser / device (login only)",
                        "browser",
                        &["browser", "device"],
                    ),
                ],
                false,
            ),
            ("settings" | "plugins", _) => {
                self.management.form = None;
                self.dispatch("/configuration/open", json!({ "instance": id }));
                self.open("live");
            }
            ("resources", "action:reload") => {
                self.management_request("/resources/reload", json!({}))
            }
            ("resources", _) => self.management_detail("Resource", self.management.data.clone()),
            ("sessions", _) => {
                if let Some(entry) = self
                    .management
                    .data
                    .as_array()
                    .and_then(|items| items.iter().find(|entry| entry["path"] == id))
                    .cloned()
                {
                    self.management.page = "selected-session".into();
                    self.management.data = entry;
                    self.management.items.clear();
                    self.open("manage:selected-session");
                }
            }
            ("tree", _) => {
                if let Some(record) = self
                    .snapshot
                    .history
                    .iter()
                    .find(|r| r.sequence.to_string() == id)
                {
                    let body = json!({
                        "target": record.sequence,
                        "branch": record.branch,
                        "summarize": false,
                    });
                    self.management_form(
                        "Navigate without replaying tools",
                        "/manage/navigate",
                        body,
                        vec![
                            text("/branch", "Branch label", "main"),
                            boolean("/summarize", "Summarize before navigating"),
                        ],
                        false,
                    );
                }
            }
            _ => {}
        }
    }
    fn management_detail(&mut self, title: &str, value: Value) {
        self.management.form = None;
        self.form_target = None;
        self.dialog_scroll = 0;
        self.dialog = Some(Dialog::Details {
            title: title.into(),
            text: serde_json::to_string_pretty(&value).unwrap_or_default(),
        });
    }
    pub(crate) fn submit_management(&mut self) {
        let Some(operation) = self.management.form.clone() else {
            return;
        };
        let Some(Dialog::Form { fields, .. }) = &mut self.dialog else {
            return;
        };
        let mut body = operation.body;
        for field in fields.iter() {
            let value = match field.parsed_value() {
                Ok(value) => value,
                Err(error) => {
                    self.notice = error;
                    return;
                }
            };
            let Some(slot) = body.pointer_mut(&field.key) else {
                continue;
            };
            *slot = value;
        }
        if body.get("confirmed").is_some() && body["confirmed"] != true {
            self.notice = "Confirm the selected action before applying".into();
            return;
        }
        if body["save_default"] == true {
            body =
                json!({ "request": { "action": "set_default", "selection": body["selection"] } });
        }
        let route = if operation.route == "/models/select" && body.get("request").is_some() {
            "/models/catalog"
        } else {
            &operation.route
        };
        if route == "/manage/sessions" {
            self.management.page = "sessions".into();
            self.management.form = None;
            self.open("manage:sessions");
            body["management"] = json!(self.management.generation);
            body["query"] = json!(true);
            self.dispatch(route, body);
            return;
        }
        if operation.private {
            for field in fields.iter_mut().filter(|f| f.private) {
                field.value.clear();
                field.initial.clear();
            }
            self.dispatch_private_auth(route, body);
        } else {
            self.management_request(route, body);
        }
    }
    fn management_request(&mut self, route: &str, mut body: Value) {
        if self.management.run.is_some() {
            self.notice =
                "An operation is running · observe or cancel it before another change".into();
            return;
        }
        body["management"] = json!(self.management.generation);
        self.request(route, body, false);
    }
    fn dispatch_private_auth(&mut self, route: &str, mut body: Value) {
        body["session_id"] = json!(self.snapshot.presentation.session_id);
        let endpoint = self.endpoint.clone();
        let route = route.to_owned();
        let generation = self.management.generation;
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            let result = eden_tui_client::call(&endpoint, "POST", &route, Some(&body)).await;
            drop(body);
            let _ = tx.send(Update::Reply {
                route,
                body: json!({ "management": generation, "private": true }),
                result,
            });
        });
    }
    pub(crate) fn management_reply(
        &mut self,
        route: &str,
        body: &Value,
        result: Result<Value, Fault>,
    ) {
        let belongs = self
            .pending
            .as_ref()
            .is_some_and(|(r, b, _)| r == route && b["request_id"] == body["request_id"]);
        let pending = if belongs {
            self.saved = false;
            self.pending.take()
        } else {
            None
        };
        let current = body["management"].as_u64() == Some(self.management.generation);
        let mut value = match result {
            Ok(value) => value,
            Err(error) => {
                self.management.last_error = Some(error.clone());
                if matches!(error.code.as_str(), "InputFailure" | "OutputFailure")
                    && let Some(pending) = pending
                {
                    self.uncertain = Some(pending);
                    self.notice = "Admission unknown · Ctrl+R checks the original request".into();
                } else {
                    self.notice = error.to_string();
                }
                return;
            }
        };
        if route == "/configuration/apply"
            && let Some(operation) = value["operation"].as_u64()
        {
            self.notice = "Configuration accepted · waiting for safe boundary and recovery".into();
            self.dispatch(
                "/configuration/wait",
                json!({ "operation": operation, "management": body["management"], "detail": true }),
            );
            return;
        }
        if let Some(run) = value["run_id"].as_u64() {
            self.management.run = Some(run);
            self.management.run_route = route.into();
            self.notice = format!("Operation {run} accepted · waiting for completion");
            self.dispatch(
                "/terminal",
                json!({ "run_id": run, "management": body["management"], "operation": route }),
            );
            return;
        }
        if route == "/terminal" {
            if self.management.run == body["run_id"].as_u64() {
                self.management.run = None;
            }
            value = match serde_json::from_value::<eden_protocol::Terminal>(value) {
                Ok(terminal) => match terminal.into_result() {
                    Ok(v) => v,
                    Err(e) => {
                        self.management.last_error = Some(e.clone());
                        self.notice = e.to_string();
                        return;
                    }
                },
                Err(error) => {
                    self.notice = error.to_string();
                    return;
                }
            };
            self.notice = "Operation completed".into();
        }
        if !current {
            return;
        }
        if route == "/manage/open"
            && let Some(endpoint) = value["endpoint"].as_str()
        {
            self.switch_endpoint = Some(endpoint.into());
            self.quit = true;
            return;
        }
        if route == "/manage/copy/preview" {
            self.management.page = "copy-preview".into();
            self.management.data = value;
            self.management.items.clear();
            self.management.form = None;
            self.open("manage:copy-preview");
            return;
        }
        if body["operation"] == "/auth/start" || body["operation"] == "/auth/input" {
            if let Some(operation) = value["operation_id"].as_str()
                && (value["status"] == "awaiting_input"
                    || value["challenge"] == "api_key"
                    || value["interaction"].is_object())
            {
                let api_key = !value["interaction"].is_object();
                self.management.auth = Some(value.clone());
                self.management.page = "auth-challenge".into();
                self.management.items.clear();
                self.management.form = None;
                self.open("manage:auth-challenge");
                if !api_key {
                    self.management_request(
                        "/auth/start",
                        json!({ "request": { "action": "wait", "operation_id": operation } }),
                    );
                }
                return;
            }
            self.management.auth = None;
        }
        if route == "/delivery/preview" {
            self.management.page = "export-preview".into();
            self.management.data = value;
            self.management.items.clear();
            self.management.form = None;
            self.open("manage:export-preview");
            return;
        }
        if body["operation"] == "/updates" {
            self.management.data = value.clone();
        }
        if route == "/auth/input" && self.management.run.is_some() {
            self.notice = "Authorization input delivered · waiting for provider".into();
            return;
        }
        if body["detail"] == true
            || route == "/terminal"
            || route == "/auth/input"
            || route == "/background/warmer"
        {
            self.management_detail("Operation result", value);
            return;
        }
        self.management.items = match self.management.page.as_str() {
            "models" => value["models"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|m| {
                    (
                        model_id(m),
                        format!(
                            "{} · {} · {}",
                            m["target"]["provider"].as_str().unwrap_or(""),
                            m["name"].as_str().unwrap_or(""),
                            m["status"].as_str().unwrap_or("unknown")
                        ),
                    )
                })
                .collect(),
            "auth" => value["providers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|p| (p.into(), p.into()))
                .collect(),
            "router" => value["models"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|m| {
                    (
                        m["id"].as_str().unwrap_or("").into(),
                        format!(
                            "{} · {} · {}",
                            m["id"].as_str().unwrap_or(""),
                            m["state"].as_str().unwrap_or("unknown"),
                            m["progress"]
                                .as_f64()
                                .map_or_else(|| "progress unknown".into(), |p| format!("{p}%"))
                        ),
                    )
                })
                .collect(),
            "settings" | "plugins" => value["instances"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|i| {
                    (
                        i["id"].as_str().unwrap_or("").into(),
                        format!(
                            "{} · {}",
                            i["id"].as_str().unwrap_or(""),
                            i["package"].as_str().unwrap_or("")
                        ),
                    )
                })
                .collect(),
            "resources" => ["skills", "templates", "context_files", "diagnostics"]
                .into_iter()
                .filter(|k| value[*k].is_array())
                .map(|k| {
                    (
                        k.into(),
                        format!("{k} · {}", value[k].as_array().map_or(0, Vec::len)),
                    )
                })
                .collect(),
            "sessions" => value
                .as_array()
                .into_iter()
                .flatten()
                .map(|e| {
                    (
                        e["path"].as_str().unwrap_or("").into(),
                        format!(
                            "{} · {} · {}",
                            e["name"].as_str().unwrap_or(""),
                            e["path"].as_str().unwrap_or(""),
                            e["diagnostic"].as_str().unwrap_or("")
                        ),
                    )
                })
                .collect(),
            _ => vec![],
        };
        self.management.data = value;
        self.notice = "Loaded".into();
    }
}
fn model_id(model: &Value) -> String {
    format!(
        "{}/{}",
        model["target"]["provider"].as_str().unwrap_or(""),
        model["target"]["model"].as_str().unwrap_or("")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::app;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[tokio::test]
    async fn closed_management_does_not_reopen_for_late_completion() {
        let mut app = app();
        app.open_management("models");
        let generation = app.management.generation;
        app.dialog_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.management_reply(
            "/terminal",
            &json!({ "management": generation, "run_id": 9 }),
            Ok(json!(eden_protocol::Terminal {
                outcome: eden_protocol::Outcome::Completed(json!({ "changed": true })),
                cleanup_errors: vec![],
                partial_result: None
            })),
        );
        assert!(app.dialog.is_none());
    }

    #[tokio::test]
    async fn switching_management_rejects_old_catalog_and_preserves_composer() {
        let mut app = app();
        app.editor.restore("用户草稿", 12).unwrap();
        app.open_management("models");
        let generation = app.management.generation;
        app.open_management("router");
        app.management_reply(
            "/models/list",
            &json!({ "management": generation, "query": true }),
            Ok(json!({ "models": [{ "name": "wrong" }] })),
        );
        assert!(app.management.items.is_empty());
        assert_eq!(app.editor.text(), "用户草稿");
    }
}

#[cfg(test)]
mod acceptance_tests {
    use super::*;
    use crate::app::tests::app;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[tokio::test]
    async fn export_confirmation_and_back_preserve_the_exact_preview() {
        let mut app = app();
        app.open_management("delivery");
        let preview = json!({
            "preview_id": "fixed-1",
            "artifact": { "content": "FILTERED_ONLY", "filename": "reading.jsonl", "warnings": [] },
        });
        app.management_reply(
            "/delivery/preview",
            &json!({ "management": app.management.generation }),
            Ok(preview.clone()),
        );
        app.choose_management("export-preview", "action:inspect");
        assert!(matches!(&app.dialog,Some(Dialog::Details{text,..}) if text=="FILTERED_ONLY"));
        app.dialog_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.management.data, preview);
        app.choose_management("export-preview", "action:publish");
        app.submit_management();
        assert!(app.pending.is_none());
        assert!(app.notice.contains("Confirm"));
        if let Some(Dialog::Form { fields, .. }) = &mut app.dialog {
            fields[0].value = "true".into();
        }
        app.submit_management();
        assert_eq!(app.pending.as_ref().unwrap().1["preview_id"], "fixed-1");
        assert!(app.pending.as_ref().unwrap().1.get("artifact").is_none());
    }

    #[tokio::test]
    async fn management_auth_input_never_enters_pending_draft_receipts() {
        let mut app = app();
        app.management.page = "auth-challenge".into();
        app.management.auth = Some(json!({ "operation_id": "private-op", "challenge": "api_key" }));
        app.choose_management("auth-challenge", "action:input");
        if let Some(Dialog::Form { fields, .. }) = &mut app.dialog {
            fields[0].value = "PRIVATE_AUTH_CANARY".into();
        }
        app.submit_management();
        assert!(app.pending.is_none());
        assert!(
            matches!(&app.dialog,Some(Dialog::Form{fields,..}) if fields[0].value.is_empty() && fields[0].initial.is_empty())
        );
        assert!(
            !serde_json::to_string(&app.management.auth)
                .unwrap()
                .contains("PRIVATE_AUTH_CANARY")
        );
    }

    #[tokio::test]
    async fn management_remains_usable_at_small_and_transient_sizes() {
        let mut app = app();
        for page in [
            "models",
            "sessions",
            "settings",
            "plugins",
            "router",
            "delivery",
            "updates",
            "background",
        ] {
            app.open_management(page);
            for (width, height) in [(0, 0), (1, 1), (48, 16), (80, 24), (120, 36), (160, 48)] {
                let area = ratatui::layout::Rect::new(0, 0, width, height);
                let mut buffer = ratatui::buffer::Buffer::empty(area);
                crate::view::render(&mut app, &mut buffer, area, true);
            }
        }
    }

    #[tokio::test]
    #[ignore = "explicit release-profile performance measurement"]
    async fn measure_large_history_render_and_input() {
        use crate::model::{Message, Role};
        use ratatui::{buffer::Buffer, layout::Rect};
        use std::time::Instant;
        for count in [1_000, 10_000, 100_000] {
            let mut app = app();
            app.messages = (0..count)
                .map(|i| {
                    Message::new(
                        i,
                        Role::Assistant,
                        "Assistant",
                        format!(
                            "Record {i}: 中文 é 🧭 plain response with enough text to wrap at \
                             ordinary terminal width."
                        ),
                    )
                })
                .collect();
            let area = Rect::new(0, 0, 120, 36);
            let start = Instant::now();
            let mut buffer = Buffer::empty(area);
            crate::view::render(&mut app, &mut buffer, area, false);
            let first = start.elapsed();
            let mut samples = vec![];
            for _ in 0..20 {
                let start = Instant::now();
                app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
                crate::view::render(&mut app, &mut buffer, area, false);
                samples.push(start.elapsed().as_micros());
            }
            samples.sort();
            let memory = std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find(|line| line.starts_with("VmHWM:"))
                        .map(str::to_owned)
                });
            println!("S4_MEMORY records={count} peak={memory:?}");
            println!(
                "S4_PERF records={count} first_us={} input_render_p50_us={} p95_us={} max_us={} \
                 rows={}",
                first.as_micros(),
                samples[10],
                samples[18],
                samples[19],
                app.rows.len()
            );
        }
    }
}
