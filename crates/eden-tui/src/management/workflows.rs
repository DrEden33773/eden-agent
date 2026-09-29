//! Shared management workflows use the same fields as plugin forms, with explicit final actions.
use super::*;
fn json_field(key: &str, label: &str, value: Value) -> Field {
    let mut f = text(key, label, &value.to_string());
    f.kind = "json".into();
    f
}
pub(super) fn actions(page: &str) -> Option<&'static [(&'static str, &'static str)]> {
    Some(match page {
        "auth-challenge" => &[
            ("details", "Authorization URL, code and expiry"),
            ("copy-url", "Copy authorization URL"),
            ("browser", "Open authorization URL in browser"),
            ("input", "Enter private key / authorization code"),
            ("status", "Check original authentication status"),
            ("cancel-auth", "Cancel authentication"),
        ],
        "plugins" => &[
            ("install", "Install / update a package"),
            ("remove", "Remove a package"),
            ("resolve", "Resolve an installed package composition"),
            (
                "replace",
                "Replace a selected instance from a resolved composition",
            ),
        ],
        "delivery" => &[
            ("preview", "Prepare filtered reading JSONL"),
            ("read", "Open a JSONL file read only"),
        ],
        "export-preview" => &[
            ("inspect", "Read the exact filtered preview"),
            ("save", "Save these bytes to a new file"),
            ("publish", "Share this exact preview as a secret gist"),
            ("discard", "Discard preview"),
        ],
        "updates" => &[
            ("discover", "Installed update channels"),
            ("check", "Check a host / package channel"),
            ("prepare", "Prepare the checked candidate"),
            ("activate", "Activate the prepared update"),
        ],
        "background" => &[
            ("notes", "Notes configuration"),
            ("warmer", "Cache warmer configuration"),
            ("status", "Cache warmer status and auxiliary usage"),
            ("cancel-warmer", "Cancel warming and wait for cleanup"),
            (
                "compact",
                "Compact with the selected summary / notes policy",
            ),
            ("rebuild", "Rebuild context on a new branch"),
        ],
        "trust" => &[
            ("inspect", "Inspect effective trust and startup override"),
            ("save", "Save project trust"),
        ],
        _ => return None,
    })
}
impl App {
    pub(super) fn workflow_choice(&mut self, page: &str, id: &str) -> bool {
        let id = id.strip_prefix("action:").unwrap_or(id);
        match (page, id) {
            ("auth-challenge", "details") => self.management_detail(
                "Authentication challenge",
                self.management.auth.clone().unwrap_or(Value::Null),
            ),
            ("auth-challenge", "copy-url") => {
                self.copied = self
                    .management
                    .auth
                    .as_ref()
                    .and_then(|v| v["interaction"]["url"].as_str())
                    .map(str::to_owned);
            }
            ("auth-challenge", "browser") => {
                if let Some(url) = self
                    .management
                    .auth
                    .as_ref()
                    .and_then(|v| v["interaction"]["url"].as_str())
                    .filter(|s| s.starts_with("https://") || s.starts_with("http://"))
                    .map(str::to_owned)
                {
                    let tx = self.tx.clone();
                    let generation = self.management.generation;
                    self.runtime.spawn_blocking(move || {
                        let mut command = if cfg!(target_os = "windows") {
                            let mut c = std::process::Command::new("rundll32");
                            c.arg("url.dll,FileProtocolHandler");
                            c
                        } else {
                            std::process::Command::new(if cfg!(target_os = "macos") {
                                "open"
                            } else {
                                "xdg-open"
                            })
                        };
                        let result = command
                            .arg(url)
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status()
                            .map_err(|_| {
                                Fault::new(
                                    "BrowserUnavailable",
                                    "tui",
                                    "browser could not open; copy the authorization URL",
                                )
                            })
                            .and_then(|status| {
                                if status.success() {
                                    Ok(json!({ "opened": true }))
                                } else {
                                    Err(Fault::new(
                                        "BrowserUnavailable",
                                        "tui",
                                        "browser could not open; copy the authorization URL",
                                    ))
                                }
                            });
                        let _ = tx.send(Update::Reply {
                            route: "/auth/browser".into(),
                            body: json!({ "management": generation, "detail": true }),
                            result,
                        });
                    });
                }
            }
            ("auth-challenge", "status") => {
                if let Some(auth) = &self.management.auth {
                    self.dispatch(
                        "/auth/status",
                        json!({
                            "operation_id": auth["operation_id"],
                            "management": self.management.generation,
                            "detail": true,
                        }),
                    );
                }
            }
            ("auth-challenge", "cancel-auth") => {
                if let Some(run) = self.management.run {
                    self.dispatch("/cancel", json!({ "run_id": run }));
                } else if let Some(auth) = &self.management.auth {
                    self.management_request(
                        "/auth/start",
                        json!({
                            "request": { "action": "cancel", "operation_id": auth["operation_id"] },
                        }),
                    );
                }
            }
            ("auth-challenge", "input") => {
                if let Some(auth) = self.management.auth.clone() {
                    let api_key = !auth["interaction"].is_object();
                    let mut field = text(
                        "/input",
                        if api_key {
                            "API key"
                        } else {
                            "Authorization code / redirect URL"
                        },
                        "",
                    );
                    field.private = true;
                    self.management_form(
                        "Private authentication input",
                        "/auth/input",
                        json!({
                            "operation_id": auth["operation_id"],
                            "input": "",
                            "api_key": api_key,
                        }),
                        vec![field],
                        true,
                    );
                }
            }

            ("selected-session", "rename") => self.management_form(
                "Rename stopped session",
                "/manage/rename",
                json!({
                    "path": self.management.data["path"],
                    "name": self.management.data["name"],
                    "tags": self.management.data["tags"],
                }),
                vec![
                    text(
                        "/name",
                        "Name",
                        self.management.data["name"].as_str().unwrap_or(""),
                    ),
                    json_field("/tags", "Tags", self.management.data["tags"].clone()),
                ],
                false,
            ),
            ("plugins", "install") => self.management_form(
                "Install / update package",
                "/command",
                json!({
                    "name": "package.install",
                    "arguments": { "source": { "kind": "local", "path": "" }, "build": false },
                }),
                vec![
                    json_field(
                        "/arguments/source",
                        "Source (local, git or release)",
                        json!({ "kind": "local", "path": "" }),
                    ),
                    boolean("/arguments/build", "Allow package build"),
                ],
                false,
            ),
            ("plugins", "remove") => self.management_form(
                "Remove installed package",
                "/command",
                json!({
                    "name": "package.remove",
                    "arguments": { "name": "", "version": "", "force": false },
                }),
                vec![
                    text("/arguments/name", "Package", ""),
                    text("/arguments/version", "Version", ""),
                    boolean("/arguments/force", "Remove despite recorded references"),
                ],
                false,
            ),
            ("plugins", "resolve") => self.management_form(
                "Resolve installed package",
                "/command",
                json!({
                    "name": "package.resolve",
                    "arguments": { "base": "", "packages": [], "roles": {} },
                }),
                vec![
                    text("/arguments/base", "Base composition path", ""),
                    json_field(
                        "/arguments/packages",
                        "Package selections",
                        json!([{ "name": "", "version": "" }]),
                    ),
                    json_field("/arguments/roles", "Selected roles", json!({})),
                ],
                false,
            ),
            ("plugins", "replace") => self.management_form(
                "Replace installed instance",
                "/configuration/apply",
                json!({
                    "change": {
                        "instance": "",
                        "revision": self.management.data["revision"],
                        "patch": {},
                        "replacement": "",
                    },
                    "mode": "wait",
                }),
                vec![
                    text("/change/instance", "Instance", ""),
                    text("/change/replacement", "Resolved composition path", ""),
                    choice("/mode", "Safe boundary", "wait", &["wait", "cancel"]),
                ],
                false,
            ),
            ("delivery", "preview") => {
                let selection = eden_protocol::delivery::Selection::default();
                let mut fields = vec![];
                for (key, label, on) in [
                    ("messages", "Messages", true),
                    ("tools", "Tools", true),
                    ("thinking", "Thinking", false),
                    ("attachments", "Attachments", false),
                    ("full_outputs", "Full tool outputs", false),
                    ("extensions", "Static plugin views", true),
                ] {
                    let mut f = boolean(&format!("/selection/{key}"), label);
                    f.value = on.to_string();
                    fields.push(f);
                }
                fields.push(json_field(
                    "/selection/runs",
                    "Run IDs (empty = all)",
                    json!([]),
                ));
                self.management_form(
                    "Prepare reading JSONL",
                    "/delivery/preview",
                    json!({ "selection": selection }),
                    fields,
                    false,
                );
            }
            ("delivery", "read") => self.management_form(
                "Read JSONL",
                "/manage/open",
                json!({ "path": "", "read_only": true }),
                vec![text("/path", "JSONL file", "")],
                false,
            ),
            ("export-preview", "inspect") => {
                let content = self.management.data["artifact"]["content"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned();
                self.management.form = None;
                self.dialog_scroll = 0;
                self.dialog = Some(Dialog::Details {
                    title: "Filtered JSONL preview".into(),
                    text: content,
                });
            }
            ("export-preview", "save") => self.management_form(
                "Save fixed preview",
                "/delivery/save",
                json!({ "preview_id": self.management.data["preview_id"], "path": "" }),
                vec![text(
                    "/path",
                    "New file",
                    self.management.data["artifact"]["filename"]
                        .as_str()
                        .unwrap_or("session.jsonl"),
                )],
                false,
            ),
            ("export-preview", "publish") => self.management_form(
                "Share fixed preview",
                "/delivery/publish",
                json!({ "preview_id": self.management.data["preview_id"], "confirmed": false }),
                vec![boolean(
                    "/confirmed",
                    "Publish this preview · anyone with its URL can read it",
                )],
                false,
            ),
            ("export-preview", "discard") => self.management_request(
                "/delivery/discard",
                json!({ "preview_id": self.management.data["preview_id"] }),
            ),
            ("updates", "discover") => self.management_request(
                "/updates",
                json!({ "request": { "operation": "discover" } }),
            ),
            ("updates", "check") => self.management_form(
                "Check updates",
                "/updates",
                json!({
                    "request": {
                        "operation": "check",
                        "target": { "kind": "host" },
                        "channel": { "kind": "stable" },
                    },
                }),
                vec![
                    json_field("/request/target", "Target", json!({ "kind": "host" })),
                    json_field("/request/channel", "Channel", json!({ "kind": "stable" })),
                ],
                false,
            ),
            ("updates", "prepare") => {
                if self.management.data["status"]["candidate"].is_object() {
                    self.management_request(
                        "/updates",
                        json!({
                            "request": {
                                "operation": "prepare",
                                "candidate":
                                    self.management.data["status"]["candidate"],
                            },
                        }),
                    );
                } else {
                    self.notice = "Check a channel and inspect its candidate first".into();
                }
            }
            ("updates", "activate") => {
                if self.management.data["prepared"].is_object() {
                    self.management_form(
                        "Activate prepared update for future launches",
                        "/updates",
                        json!({
                            "request": {
                                "operation": "activate",
                                "prepared": self.management.data["prepared"],
                            },
                            "confirmed": false,
                        }),
                        vec![boolean("/confirmed", "Activate the prepared version")],
                        false,
                    );
                } else {
                    self.notice = "Prepare the checked candidate first".into();
                }
            }
            ("background", "notes" | "warmer") => {
                let instance = if id == "notes" {
                    "note-style-context-management"
                } else {
                    "cache-warmer"
                };
                self.management.form = None;
                self.dispatch("/configuration/open", json!({ "instance": instance }));
                self.open("live");
            }
            ("background", "status" | "cancel-warmer") => self.management_request(
                "/background/warmer",
                json!({ "cancel": id == "cancel-warmer" }),
            ),
            ("background", "compact") => self.open_context_management(false),
            ("background", "rebuild") => self.open_context_management(true),
            ("trust", "inspect") => self.dispatch(
                "/trust/inspect",
                json!({ "management": self.management.generation, "detail": true }),
            ),
            ("trust", "save") => self.management_form(
                "Project trust",
                "/trust/save",
                json!({ "path": self.snapshot.state.cwd, "trusted": false }),
                vec![
                    text("/path", "Project directory", &self.snapshot.state.cwd),
                    boolean("/trusted", "Allow project code and configuration"),
                ],
                false,
            ),
            ("resources", "trust") => self.open_management("trust"),
            _ => return false,
        }
        true
    }
}
