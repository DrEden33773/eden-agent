//! Explicit source-preserving session copies and migration previews.
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

/// Which of the six copy families an operation performs. Each answers a
/// different question about a source, and each has its own preservation rule.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyKind {
    /// Copy the selected ancestry into an independent session.
    Fork,
    /// Keep the whole tree and the active selection.
    Clone,
    /// Take a session from an outside history file, as this installation's format.
    Import,
    /// Convert a version 1 linear history to the current schema.
    Upgrade,
    /// Salvage only the validated prefix of a damaged history.
    Recover,
    /// Relocate a session and convert its extension state for a new layout.
    Migrate,
}
/// What one copy or migration operation was asked to do. `target` selects the
/// node to copy from and `cwd` relocates the session, each only where the
/// selected [`CopyKind`] supports it.
#[derive(Clone, Debug)]
pub struct CopyOptions {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub kind: CopyKind,
    pub target: Option<u64>,
    pub cwd: Option<PathBuf>,
    pub public_only: bool,
}
/// Preview is bound to the exact source bytes. Applying a stale preview fails.
#[derive(Clone, Debug, Serialize)]
pub struct CopyPlan {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub kind: CopyKind,
    pub source_session: u64,
    pub new_session: u64,
    pub source_sequence: u64,
    pub selected_node: Option<u64>,
    pub cwd: String,
    pub preserved: Vec<String>,
    pub losses: Vec<String>,
    #[serde(skip)]
    pub(crate) bytes: Vec<u8>,
    #[serde(skip)]
    pub(crate) records: Vec<c::Record>,
    #[serde(skip)]
    pub(crate) composition: PathBuf,
    #[serde(skip)]
    pub(crate) migration: Option<c::MigrateReply>,
}

fn invalid(message: impl Into<String>) -> Fault {
    Fault::new("InvalidInput", "session-copy", message)
}
fn consumed_ids(records: &[c::Record]) -> BTreeSet<u64> {
    records
        .iter()
        .filter(|r| r.kind == "queue_consumed")
        .flat_map(|r| {
            r.payload
                .get("ids")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_u64)
                .chain(r.payload.get("id").and_then(Value::as_u64))
        })
        .collect()
}
fn copy_records(
    records: &[c::Record],
    kind: &CopyKind,
    target: Option<u64>,
    id: u64,
    public_only: bool,
) -> Result<Vec<c::Record>, Fault> {
    eden_protocol::history::validate_records(records)?;
    let mut selected = BTreeSet::new();
    if matches!(kind, CopyKind::Fork) {
        let mut head = target.or(eden_protocol::history::branch_state(records)?.0);
        while let Some(n) = head {
            let record = records
                .iter()
                .find(|r| r.sequence == n && r.kind != "branch_selected")
                .ok_or_else(|| invalid("fork target is not a tree node"))?;
            selected.insert(n);
            head = record.parent_id;
        }
    } else {
        selected.extend(records.iter().map(|r| r.sequence));
    }
    let mut deliveries = BTreeMap::new();
    let mut consumed_deliveries = BTreeSet::new();
    for record in records.iter().filter(|r| selected.contains(&r.sequence)) {
        if record.kind == "queue_delivered"
            && let Some(id) = record.payload["id"].as_u64()
        {
            deliveries.insert(id, record.sequence);
        }
        if record.kind == "queue_returned"
            && let Some(id) = record.payload["id"].as_u64()
        {
            deliveries.remove(&id);
        }
        if record.kind == "queue_consumed" {
            for id in consumed_ids(std::slice::from_ref(record)) {
                if let Some(sequence) = deliveries.remove(&id) {
                    consumed_deliveries.insert(sequence);
                }
            }
        }
    }
    let mut mapping = BTreeMap::new();
    let mut output = vec![];
    for record in records.iter().filter(|r| selected.contains(&r.sequence)) {
        let mut copy = record.clone();
        if copy.kind.starts_with("queue_") && copy.kind != "queue_config" {
            if copy.kind == "queue_delivered"
                && (copy.schema_version == 1 || consumed_deliveries.contains(&copy.sequence))
            {
                copy.kind = "message".into();
                copy.payload = json!({
                    "type": "message",
                    "role": "user",
                    "content": copy.payload["content"],
                });
            } else {
                continue;
            }
        }
        if public_only && matches!(copy.kind.as_str(), "extension_state" | "provider_state") {
            continue;
        }
        let mut parent = copy.parent_id;
        while let Some(n) = parent {
            if let Some(mapped) = mapping.get(&n) {
                parent = Some(*mapped);
                break;
            }
            parent = records[(n - 1) as usize].parent_id;
        }
        copy.parent_id = parent;
        copy.schema_version = 2;
        copy.session_id = id;
        copy.sequence = output.len() as u64 + 1;
        if copy.kind == "branch_selected" {
            copy.payload["target"] = json!(parent);
        }
        if copy.kind == "compaction" {
            if let Some(n) = copy.payload["first_kept"].as_u64().filter(|n| *n != 0) {
                // Find the first surviving node after the boundary on this ancestry;
                // global sequence order can include unrelated branches.
                let mut ancestor = record.parent_id;
                let mut mapped = None;
                while let Some(node) = ancestor {
                    if node < n {
                        break;
                    }
                    if let Some(value) = mapping.get(&node) {
                        mapped = Some(*value);
                    }
                    if node == n {
                        break;
                    }
                    ancestor = records[(node - 1) as usize].parent_id;
                }
                copy.payload["first_kept"] = json!(mapped.unwrap_or(0));
            }
            if let Some(ids) = copy.payload["source_ids"].as_array() {
                copy.payload["source_ids"] = json!(
                    ids.iter()
                        .filter_map(|v| v.as_u64().and_then(|id| mapping.get(&id).copied()))
                        .collect::<Vec<_>>()
                );
            }
        }
        mapping.insert(record.sequence, copy.sequence);
        output.push(copy);
    }
    eden_protocol::history::validate_records(&output)?;
    Ok(output)
}

impl Session {
    /// Read a source and explain an independent copy before any destination writes.
    pub async fn plan_copy(
        composition: impl AsRef<Path>,
        options: CopyOptions,
    ) -> Result<CopyPlan, Fault> {
        let bytes = std::fs::read(&options.source).map_err(|e| invalid(e.to_string()))?;
        let scan = eden_protocol::history::scan_records(&bytes);
        if scan.diagnostic.is_some() && !matches!(options.kind, CopyKind::Recover) {
            return Err(invalid(concat!(
                "source has damaged/uncommitted data; use explicit recover to copy the validated ",
                "prefix",
            )));
        }
        let first = scan
            .records
            .first()
            .filter(|r| r.kind == "session")
            .ok_or_else(|| invalid("source has no committed session header"))?;
        if options.destination.exists() {
            return Err(invalid("destination already exists"));
        }
        if matches!(options.kind, CopyKind::Upgrade) && first.schema_version != 1 {
            return Err(invalid("upgrade requires a v1 source"));
        }
        let cwd = match options.cwd {
            Some(path) => std::fs::canonicalize(path)
                .map_err(|e| invalid(e.to_string()))?
                .to_string_lossy()
                .into_owned(),
            None => first.payload["cwd"]
                .as_str()
                .ok_or_else(|| invalid("source cwd missing"))?
                .to_owned(),
        };
        if options.public_only && !matches!(options.kind, CopyKind::Migrate) {
            return Err(invalid("public-only conversion requires migrate"));
        }
        let id = new_session_id();
        let mut records = copy_records(
            &scan.records,
            &options.kind,
            options.target,
            id,
            options.public_only,
        )?;
        let mut losses =
            vec!["Pending queue entries are not copied; they remain in the source session.".into()];
        if let Some(diagnostic) = scan.diagnostic {
            losses.push(format!(
                "Only the validated committed prefix is copied: {diagnostic}"
            ));
        }
        if options.public_only {
            losses.push(
                concat!(
                    "Plugin private state and provider reasoning state are omitted; this is not a ",
                    "full state restoration.",
                )
                .into(),
            );
        }
        if cwd != first.payload["cwd"].as_str().unwrap_or("") {
            losses.push(
                concat!(
                    "Working directory binding changes; external project files are not copied or ",
                    "rolled back.",
                )
                .into(),
            );
        }
        let composition = std::fs::canonicalize(composition).map_err(|e| invalid(e.to_string()))?;
        let mut selected: eden_protocol::Composition = serde_json::from_slice(
            &std::fs::read(&composition).map_err(|e| invalid(e.to_string()))?,
        )
        .map_err(|e| invalid(e.to_string()))?;
        for package in &mut selected.packages {
            package.library = composition
                .parent()
                .unwrap_or(Path::new("."))
                .join(&package.library)
                .to_string_lossy()
                .into_owned();
        }
        for record in &mut records {
            if record.kind == "composition_lock" {
                if matches!(options.kind, CopyKind::Migrate | CopyKind::Upgrade) {
                    record.payload = crate::composition::binding(&selected, &cwd)?;
                } else {
                    record.payload["cwd"] = json!(cwd);
                }
            }
        }
        let mut migration = None;
        if matches!(options.kind, CopyKind::Migrate) && !options.public_only {
            let states: Vec<c::ExtensionState> = records
                .iter()
                .filter(|r| r.kind == "extension_state")
                .map(|r| {
                    serde_json::from_value(r.payload.clone()).map_err(|e| invalid(e.to_string()))
                })
                .collect::<Result<_, _>>()?;
            if !states.is_empty() {
                let reply: c::MigrateReply = isolated_service(
                    &composition,
                    id,
                    c::MIGRATOR,
                    &c::MigrateRequest {
                        states,
                        apply: false,
                    },
                )
                .await?;
                if reply.states.len()
                    != records
                        .iter()
                        .filter(|r| r.kind == "extension_state")
                        .count()
                {
                    return Err(invalid(
                        "migrator must preserve one state result per source record",
                    ));
                }
                if reply.states.iter().any(|state| state.required) {
                    let _: c::InterpretReply = isolated_service(
                        &composition,
                        id,
                        c::INTERPRETER,
                        &c::InterpretRequest {
                            states: reply.states.clone(),
                        },
                    )
                    .await?;
                }
                losses.extend(reply.losses.clone());
                migration = Some(reply);
            }
        }
        let selected_node = if matches!(options.kind, CopyKind::Fork) {
            options
                .target
                .or(eden_protocol::history::branch_state(&scan.records)?.0)
        } else {
            eden_protocol::history::branch_state(&scan.records)?.0
        };
        let header = &mut records[0];
        header.payload["cwd"] = json!(cwd);
        header.payload["origin"] = json!({
            "session_id": first.session_id,
            "sequence": selected_node,
            "source_tip": scan.records.last().unwrap().sequence,
            "operation": options.kind,
            "source": options.source,
        });
        if matches!(options.kind, CopyKind::Migrate | CopyKind::Upgrade) {
            header.payload["roles"] = json!(selected.roles);
            header.payload["packages"] = json!(
                selected
                    .packages
                    .iter()
                    .map(|p| &p.descriptor)
                    .collect::<Vec<_>>()
            );
        }
        let mut preserved = vec![
            concat!(
                "Public selected history, attachments, branch links and source provenance; source ",
                "bytes remain unchanged.",
            )
            .into(),
        ];
        if let Some(reply) = &migration {
            preserved.extend(reply.preserved.clone());
        }
        Ok(CopyPlan {
            source: options.source,
            destination: options.destination,
            kind: options.kind,
            source_session: first.session_id,
            new_session: id,
            source_sequence: scan.records.last().unwrap().sequence,
            selected_node,
            cwd,
            preserved,
            losses,
            bytes,
            records,
            composition,
            migration,
        })
    }
    /// Execute an already reviewed plan; source changes invalidate it.
    pub async fn apply_copy(plan: CopyPlan) -> Result<PathBuf, Fault> {
        // The owned task retains destination creation and cleanup if its receiver is dropped.
        tokio::spawn(async move {
            if std::fs::read(&plan.source).map_err(|e| invalid(e.to_string()))? != plan.bytes {
                return Err(invalid("source changed after preview; generate a new plan"));
            }
            let mut records = plan.records;
            if let Some(preview) = plan.migration {
                let states = records
                    .iter()
                    .filter(|r| r.kind == "extension_state")
                    .map(|r| {
                        serde_json::from_value(r.payload.clone())
                            .map_err(|e| invalid(e.to_string()))
                    })
                    .collect::<Result<_, _>>()?;
                let reply: c::MigrateReply = isolated_service(
                    &plan.composition,
                    plan.new_session,
                    c::MIGRATOR,
                    &c::MigrateRequest {
                        states,
                        apply: true,
                    },
                )
                .await?;
                if serde_json::to_value(&reply).ok() != serde_json::to_value(&preview).ok() {
                    return Err(invalid(
                        "migrator result differs from preview; destination not created",
                    ));
                }
                let count = records
                    .iter()
                    .filter(|r| r.kind == "extension_state")
                    .count();
                if reply.states.len() != count {
                    return Err(invalid(concat!(
                        "migrator must preserve one state result per source record; preview a ",
                        "public-only copy to discard state",
                    )));
                }
                let mut states = reply.states.into_iter();
                for record in &mut records {
                    if record.kind == "extension_state" {
                        record.payload = json!(states.next().unwrap());
                    }
                }
            }
            let _: c::StoreReply = isolated_service(
                &plan.composition,
                plan.new_session,
                c::STORE,
                &c::StoreRequest::Create {
                    path: plan.destination.to_string_lossy().into_owned(),
                    session_id: plan.new_session,
                    records,
                },
            )
            .await?;
            Ok(plan.destination)
        })
        .await
        .map_err(|e| invalid(e.to_string()))?
    }
}
async fn isolated_service<I: Serialize, O: serde::de::DeserializeOwned + Send + 'static>(
    composition: &Path,
    id: u64,
    role: &str,
    input: &I,
) -> Result<O, Fault> {
    let composition = composition.to_owned();
    let role = role.to_owned();
    let payload = serde_json::to_value(input).map_err(|e| invalid(e.to_string()))?;
    tokio::spawn(async move {
        let mut selected: eden_protocol::Composition = serde_json::from_slice(
            &std::fs::read(&composition).map_err(|e| invalid(e.to_string()))?,
        )
        .map_err(|e| invalid(e.to_string()))?;

        // Copy and migration do not consume prompt resources. Explicitly disable
        // discovery instead of loading project configuration as a side effect.
        for package in &mut selected.packages {
            if package.descriptor.package == "distribution" {
                package.config = json!({
                    "root":
                        WorkspaceOptions::default().global_dir.join("distribution"),
                });
            }
            if package
                .descriptor
                .provides
                .iter()
                .any(|role| role == eden_protocol::resources::SOURCE)
            {
                package.config = json!({
                    "cwd":
                        std::env::current_dir().map_err(|e| invalid(e.to_string()))?,
                    "global_dir": WorkspaceOptions::default().global_dir,
                    "trusted": false,
                    "settings": {
                        "discover_context": false,
                        "discover_skills": false,
                        "discover_templates": false,
                    },
                });
            }
        }
        eden_workspace::packages::resolve_paths(
            &mut selected,
            composition.parent().unwrap_or(Path::new(".")),
            &WorkspaceOptions::default().global_dir.join("distribution"),
        )?;
        let created = if role == c::STORE && payload["operation"] == "create" {
            payload["path"].as_str().map(PathBuf::from)
        } else {
            None
        };
        let copied_libraries: Option<Vec<String>> = payload["records"]
            .as_array()
            .and_then(|records| {
                records
                    .iter()
                    .rev()
                    .find(|r| r["kind"] == "composition_lock")
            })
            .and_then(|record| record["payload"]["library_locations"].as_array())
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(|p| p.as_str().map(str::to_owned))
                    .collect()
            });
        let kernel = Kernel::load_resolved(
            selected,
            composition.parent().unwrap_or(Path::new(".")),
            id,
            Events::new(id),
        )
        .await?;
        let result = kernel
            .invoke(
                Request {
                    session_id: id,
                    run_id: 0,
                    contract: role,
                    payload,
                },
                Cancellation::default(),
            )
            .await
            .into_result();
        let result = result.and_then(|value| {
            if let Some(path) = created {
                // The creator's Store composition need not match the copied binding.
                if let Some(libraries) = copied_libraries {
                    eden_workspace::packages::register_libraries(
                        &WorkspaceOptions::default().global_dir.join("distribution"),
                        &path,
                        libraries,
                        false,
                    )?;
                }
            }
            Ok(value)
        });
        let stopped = kernel.shutdown().await;
        let value = match (result, stopped) {
            (Ok(value), Ok(())) => value,
            (Err(error), Ok(())) | (Ok(_), Err(error)) => return Err(error),
            (Err(error), Err(cleanup)) => {
                return Err(Fault::new(
                    &error.code,
                    &error.source,
                    format!("{}; cleanup failed: {cleanup}", error.message),
                ));
            }
        };
        serde_json::from_value(value).map_err(|e| invalid(e.to_string()))
    })
    .await
    .map_err(|e| invalid(e.to_string()))?
}

impl Session {
    /// Stored cwd is authoritative when a caller supplies no override.
    pub async fn open_saved(
        composition: impl AsRef<Path>,
        history: PathBuf,
        cwd: Option<PathBuf>,
    ) -> Result<Self, Fault> {
        let records = eden_kernel::history::read(&history)?;
        let cwd = match cwd {
            Some(cwd) => cwd,
            None => PathBuf::from(
                records
                    .first()
                    .and_then(|r| r.payload["cwd"].as_str())
                    .ok_or_else(|| invalid("missing recorded cwd"))?,
            ),
        };
        Self::open_with(
            composition,
            SessionOptions {
                cwd,
                history: Some(history),
            },
        )
        .await
    }
    /// The directory this session runs in.
    pub fn cwd(&self) -> &str {
        &self.0.cwd
    }
    /// Change the active path without modifying previous nodes or project files.
    pub fn navigate(&self, target: u64, branch: String, summarize: bool) -> Result<u64, Fault> {
        self.start(true, move |session, run_id, cancel| async move {
            let result = async {
                let records = session.history().await?;
                let (head, old_branch) = eden_protocol::history::branch_state(&records)?;
                let old_path = eden_protocol::history::active_path(&records)?;
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        return Err(Fault::new("Cancelled", "navigation", "cancelled"));
                    },
                    _ = std::future::ready(()) => {}
                }
                let _: c::StoreReply = session
                    .service(
                        run_id,
                        c::STORE,
                        &c::StoreRequest::Navigate { target, branch },
                    )
                    .await?;
                if summarize {
                    let new_path = eden_protocol::history::active_path(&session.history().await?)?;
                    let ids: BTreeSet<_> = new_path.iter().map(|r| r.sequence).collect();
                    let departed: Vec<_> = old_path
                        .into_iter()
                        .filter(|r| !ids.contains(&r.sequence))
                        .collect();
                    if !departed.is_empty() {
                        let reply = session
                            .0
                            .kernel
                            .invoke(
                                Request {
                                    session_id: session.id(),
                                    run_id,
                                    contract: c::CONTEXT.into(),
                                    payload: json!(c::ContextInput {
                                        resources: session.context_resources().await?,
                                        tools: session.context_tools().await?,
                                        action: "branch_summary".into(),
                                        records: departed,
                                        instructions: String::new(),
                                        limits: c::ModelLimits::default(),
                                        cwd: session.cwd().into(),
                                        items: vec![]
                                    }),
                                },
                                cancel,
                            )
                            .await
                            .into_result();
                        if let Err(error) = reply {
                            if let Some(target) = head {
                                session
                                    .service::<_, c::StoreReply>(
                                        run_id,
                                        c::STORE,
                                        &c::StoreRequest::Navigate {
                                            target,
                                            branch: old_branch,
                                        },
                                    )
                                    .await
                                    .map_err(|restore| {
                                        Fault::new(
                                            "PersistenceFailure",
                                            "navigation",
                                            format!(
                                                concat!(
                                                    "summary failed ({error}); could not restore ",
                                                    "selection: {restore}",
                                                ),
                                                error = error,
                                                restore = restore,
                                            ),
                                        )
                                    })?;
                            }
                            return Err(error);
                        }
                    }
                }
                Ok(json!("Branch selected; project files unchanged."))
            }
            .await;
            as_terminal(result)
        })
    }

    /// Manual compaction participates in the same cancellation and settled barrier as a run.
    pub fn compact(&self, instructions: String) -> Result<u64, Fault> {
        self.start(true, move |session, run_id, cancel| async move {
            let records = match session.history().await {
                Ok(records) => records,
                Err(error) => return Terminal::failed(error),
            };
            let resources = match session.context_resources().await {
                Ok(value) => value,
                Err(error) => return Terminal::failed(error),
            };
            let tools = match session.context_tools().await {
                Ok(value) => value,
                Err(error) => return Terminal::failed(error),
            };
            session
                .0
                .kernel
                .invoke(
                    Request {
                        session_id: session.id(),
                        run_id,
                        contract: c::CONTEXT.into(),
                        payload: json!(c::ContextInput {
                            resources,
                            tools,
                            action: "compact".into(),
                            records,
                            instructions,
                            limits: c::ModelLimits::default(),
                            cwd: session.cwd().into(),
                            items: vec![]
                        }),
                    },
                    cancel,
                )
                .await
        })
    }
    /// Atomically reload text resources while the session is idle.
    pub fn reload_resources(&self) -> Result<u64, Fault> {
        self.start(true, move |session, run_id, _| async move {
            as_terminal(
                session
                    .service::<_, eden_protocol::resources::ResourceReply>(
                        run_id,
                        eden_protocol::resources::SOURCE,
                        &eden_protocol::resources::ResourceRequest::Reload,
                    )
                    .await
                    .and_then(|reply| {
                        serde_json::to_value(reply)
                            .map_err(|e| Fault::new("InvalidInput", "resources", e.to_string()))
                    }),
            )
        })
    }
    async fn context_resources(&self) -> Result<Option<eden_protocol::resources::Snapshot>, Fault> {
        if self.role(eden_protocol::resources::SOURCE).is_err() {
            return Ok(None);
        }
        self.resources().await.map(Some)
    }
    async fn context_tools(&self) -> Result<Option<Vec<c::ToolDefinition>>, Fault> {
        use eden_protocol::resources as r;
        if self.role(r::TOOL_CATALOG).is_err() {
            return Ok(None);
        }
        let catalog: r::Catalog = self
            .service(
                0,
                r::TOOL_CATALOG,
                &r::CatalogRequest {
                    cwd: self.cwd().into(),
                },
            )
            .await?;
        Ok(Some(catalog.tools))
    }
    /// Every command the installed packages contribute. This reads a catalog
    /// and starts no run.
    pub async fn commands(&self) -> Result<eden_protocol::resources::CommandCatalog, Fault> {
        self.service(
            0,
            "eden.command-catalog.v1",
            &eden_protocol::resources::CatalogRequest {
                cwd: self.cwd().into(),
            },
        )
        .await
    }
    /// Run one contributed command as a run of its own, so its result settles
    /// like any other and reaches history with the same ordering.
    pub fn command(&self, name: String, arguments: Value) -> Result<u64, Fault> {
        self.start(true, move |session, run_id, cancel| async move {
            session
                .0
                .kernel
                .invoke(
                    Request {
                        session_id: session.id(),
                        run_id,
                        contract: eden_protocol::resources::COMMAND.into(),
                        payload: json!(eden_protocol::resources::CommandRequest {
                            cwd: session.cwd().into(),
                            name,
                            arguments
                        }),
                    },
                    cancel,
                )
                .await
        })
    }
    /// Take the resource source's current snapshot, which is what the session
    /// would load for a new request.
    pub async fn resources(&self) -> Result<eden_protocol::resources::Snapshot, Fault> {
        let reply: eden_protocol::resources::ResourceReply = self
            .service(
                0,
                eden_protocol::resources::SOURCE,
                &eden_protocol::resources::ResourceRequest::Snapshot,
            )
            .await?;
        Ok(reply.snapshot)
    }
    /// Save the session's name and tags as a committed record.
    pub fn set_metadata(&self, name: String, tags: Vec<String>) -> Result<u64, Fault> {
        self.start(true, move |session, run_id, _| async move {
            as_terminal(
                session
                    .commit(
                        run_id,
                        "session_metadata",
                        json!({ "name": name, "tags": tags }),
                    )
                    .await
                    .map(|_| json!("Session metadata saved.")),
            )
        })
    }
    /// Choose how the queue delivers steering and follow-up submissions.
    pub fn configure_queue(&self, steering: String, follow_up: String) -> Result<u64, Fault> {
        self.start(true, move |session, run_id, _| async move {
            as_terminal(
                session
                    .service::<_, Vec<c::QueueEntry>>(
                        run_id,
                        c::QUEUE,
                        &c::QueueRequest::Configure {
                            steering,
                            follow_up,
                        },
                    )
                    .await
                    .map(|_| json!("Queue delivery settings saved.")),
            )
        })
    }
    /// Add a versioned extension state record through the only history writer.
    pub fn record_state(&self, state: c::ExtensionState) -> Result<u64, Fault> {
        self.start(true, move |session, run_id, _| async move {
            as_terminal(
                session
                    .commit(run_id, "extension_state", json!(state))
                    .await
                    .map(|_| json!("Extension state saved.")),
            )
        })
    }
    /// Reintroduce stored attachment content without depending on its former source path.
    pub fn include_attachment(&self, record_id: u64) -> Result<u64, Fault> {
        self.start(true, move |session, run_id, _| async move {
            let result = async {
                let records = session.history().await?;
                let record = records
                    .iter()
                    .find(|r| r.sequence == record_id)
                    .ok_or_else(|| invalid("unknown attachment record"))?;
                let item: c::Item = serde_json::from_value(record.payload.clone())
                    .map_err(|_| invalid("record is not a message"))?;
                let c::Item::Message { content, .. } = item else {
                    return Err(invalid("record is not a message"));
                };
                let attachments: Vec<_> = content
                    .into_iter()
                    .filter(|b| matches!(b, c::Block::Image { .. } | c::Block::File { .. }))
                    .collect();
                if attachments.is_empty() {
                    return Err(invalid("record has no stored binary attachment"));
                }
                session
                    .commit(
                        run_id,
                        "message",
                        json!(c::Item::Message {
                            role: "user".into(),
                            content: attachments
                        }),
                    )
                    .await?;
                Ok(json!({ "included_from": record_id }))
            }
            .await;
            as_terminal(result)
        })
    }
}
pub(crate) fn as_terminal(result: Result<Value, Fault>) -> Terminal {
    Terminal {
        outcome: match result {
            Ok(value) => Outcome::Completed(value),
            Err(error) if error.code == "Cancelled" => Outcome::Cancelled,
            Err(error) => Outcome::Failed(error),
        },
        cleanup_errors: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn r(n: u64, parent: Option<u64>, kind: &str, payload: Value) -> c::Record {
        c::Record {
            schema_version: 2,
            session_id: 10,
            sequence: n,
            run_id: 1,
            parent_id: parent,
            branch: "main".into(),
            kind: kind.into(),
            payload,
        }
    }
    #[test]
    fn fork_keeps_ancestors_and_rebases_identity() {
        let records = vec![
            r(1, None, "session", json!({ "cwd": "/p" })),
            r(
                2,
                Some(1),
                "message",
                json!({ "type": "message", "role": "user", "content": [] }),
            ),
            r(
                3,
                Some(2),
                "message",
                json!({ "type": "message", "role": "assistant", "content": [] }),
            ),
            r(
                4,
                Some(2),
                "message",
                json!({ "type": "message", "role": "assistant", "content": [] }),
            ),
        ];
        let copy = copy_records(&records, &CopyKind::Fork, Some(3), 99, false).unwrap();
        assert_eq!(copy.len(), 3);
        assert!(copy.iter().all(|r| r.session_id == 99));
        assert_eq!(copy[2].parent_id, Some(2));
        assert_eq!(records.len(), 4);
    }
    #[test]
    fn copied_queue_keeps_consumed_conversation_but_never_pending_work() {
        let q = json!({
            "id": 2,
            "kind": "steering",
            "branch": "main",
            "content": [{ "type": "text", "text": "change requested" }],
        });
        let records = vec![
            r(1, None, "session", json!({})),
            r(2, Some(1), "queue_accepted", q.clone()),
            r(3, Some(2), "queue_delivered", q),
            r(4, Some(3), "queue_consumed", json!({ "ids": [2] })),
            r(5, Some(4), "queue_accepted", json!({ "id": 5 })),
        ];
        let copy = copy_records(&records, &CopyKind::Clone, None, 99, false).unwrap();
        assert_eq!(copy.len(), 2);
        assert_eq!(copy[1].kind, "message");
        assert_eq!(copy[1].payload["content"][0]["text"], "change requested");
        assert!(copy.iter().all(|r| !r.kind.starts_with("queue_")));
    }
    #[test]
    fn returned_delivery_is_not_copied_twice_or_before_consumption() {
        let q = json!({
            "id": 2,
            "kind": "steering",
            "branch": "main",
            "content": [{ "type": "text", "text": "one user input" }],
        });
        let records = vec![
            r(1, None, "session", json!({})),
            r(2, Some(1), "queue_accepted", q.clone()),
            r(3, Some(2), "queue_delivered", q.clone()),
            r(4, Some(3), "queue_returned", q.clone()),
            r(5, Some(4), "queue_delivered", q.clone()),
            r(6, Some(5), "queue_consumed", q),
            r(
                7,
                Some(6),
                "queue_config",
                json!({ "steering": "all", "follow_up": "one" }),
            ),
        ];
        let copy = copy_records(&records, &CopyKind::Clone, None, 99, false).unwrap();
        assert_eq!(copy.iter().filter(|r| r.kind == "message").count(), 1);
        assert_eq!(copy.last().unwrap().kind, "queue_config");
        let fork = copy_records(&records, &CopyKind::Fork, Some(5), 99, false).unwrap();
        assert_eq!(
            fork.len(),
            1,
            "unconsumed delivery cannot become copied work"
        );
    }
    #[test]
    fn v1_upgrade_preserves_delivered_context_without_pending_queue() {
        let q = json!({
            "id": 2,
            "kind": "steering",
            "content": [{ "type": "text", "text": "legacy instruction" }],
        });
        let mut records = vec![
            r(1, None, "session", json!({})),
            r(2, Some(1), "queue_accepted", q.clone()),
            r(3, Some(2), "queue_delivered", q),
            r(
                4,
                Some(3),
                "message",
                json!({ "type": "message", "role": "assistant", "content": [] }),
            ),
            r(5, Some(4), "queue_accepted", json!({ "id": 5 })),
        ];
        for record in &mut records {
            record.schema_version = 1;
        }
        let copy = copy_records(&records, &CopyKind::Upgrade, None, 99, false).unwrap();
        assert_eq!(copy.len(), 3);
        assert_eq!(copy[1].kind, "message");
        assert_eq!(copy[1].payload["content"][0]["text"], "legacy instruction");
        assert!(copy.iter().all(|r| !r.kind.starts_with("queue_")));
    }
    #[tokio::test]
    async fn fork_preview_records_selected_node_and_separate_snapshot_tip() {
        let directory = std::env::temp_dir().join(format!("eden-fork-origin-{}", new_session_id()));
        std::fs::create_dir(&directory).unwrap();
        let source = directory.join("source.jsonl");
        let composition = directory.join("composition.json");
        let records = vec![
            r(1, None, "session", json!({ "cwd": "/saved" })),
            r(2, Some(1), "message", json!({})),
            r(3, Some(2), "message", json!({})),
        ];
        std::fs::write(
            &source,
            eden_protocol::history::encode_transaction(&records).unwrap(),
        )
        .unwrap();
        std::fs::write(&composition, r#"{"packages":[],"roles":{}}"#).unwrap();
        let plan = Session::plan_copy(
            &composition,
            CopyOptions {
                source,
                destination: directory.join("fork.jsonl"),
                kind: CopyKind::Fork,
                target: Some(2),
                cwd: None,
                public_only: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(plan.records[0].payload["origin"]["sequence"], 2);
        assert_eq!(plan.records[0].payload["origin"]["source_tip"], 3);
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn copied_compaction_boundary_stays_on_its_branch() {
        let records = vec![
            r(1, None, "session", json!({})),
            r(2, Some(1), "message", json!({})),
            r(3, Some(2), "queue_accepted", json!({ "id": 3 })),
            r(4, Some(2), "message", json!({ "other_branch": true })),
            r(5, Some(3), "message", json!({ "retained": true })),
            r(
                6,
                Some(5),
                "compaction",
                json!({ "first_kept": 3, "source_ids": [2] }),
            ),
        ];
        let copy = copy_records(&records, &CopyKind::Clone, None, 99, false).unwrap();
        let compaction = copy.last().unwrap();
        let boundary = compaction.payload["first_kept"].as_u64().unwrap();
        assert_eq!(copy[(boundary - 1) as usize].payload["retained"], true);
    }
    #[test]
    fn binding_ignores_unused_packages_and_compatible_inventory_changes() {
        let original = json!({
            "cwd": "/p",
            "roles": { "context": "author" },
            "packages": [
                { "package": "author", "version": "1", "provides": ["context"] },
                { "package": "old-display", "provides": ["display"] }
            ],
        });
        let updated = json!({
            "cwd": "/p",
            "roles": { "context": "author" },
            "packages": [{ "package": "author", "version": "2", "provides": ["context", "extra"] }],
        });
        assert!(compatible_binding(&original, &updated));
        let mut relocated = updated;
        relocated["cwd"] = json!("/other");
        assert!(!compatible_binding(&original, &relocated));
    }
}
