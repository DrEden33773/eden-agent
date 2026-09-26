//! Session-owned configuration transactions shared by SDK, CLI and RPC callers.
use super::*;
pub use crate::configuration_metadata::InstalledPackage;
use eden_protocol::{Composition, configuration as p, runtime::InstanceSpec};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::PathBuf};

/// Explicit session override; patches retain the established recursive-object/replace-array merge.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Change {
    pub instance: String,
    pub revision: u64,
    pub patch: Value,
    /// A resolved composition containing the installed replacement package. No build or install runs.
    pub replacement: Option<PathBuf>,
}
/// Default waiting preserves the entire affected foreground run, including cleanup and persistence.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum ApplyMode {
    #[default]
    Wait,
    Cancel,
}
/// Live updates require an author's explicit field declaration and atomic Update implementation.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum Application {
    Live,
    Restart,
    Unchanged,
}
/// Management operation status is independent of chat run IDs.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum Status {
    Waiting,
    Applying,
    Applied,
    Restored,
    RecoveryFailed,
    CleanupFailed,
}
impl Status {
    fn finished(&self) -> bool {
        !matches!(self, Self::Waiting | Self::Applying)
    }
}
/// A receipt never contains configuration values; operation errors distinguish apply and recovery.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Receipt {
    pub operation: u64,
    pub revision: u64,
    pub instance: String,
    pub affected: Vec<String>,
    pub status: Status,
    pub error: Option<Fault>,
    pub recovery_error: Option<Fault>,
}
/// Preview freezes neither runtime nor native initialization; Apply rechecks revision and validation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Preview {
    pub revision: u64,
    pub instance: String,
    pub affected: Vec<String>,
    pub application: Application,
    pub waiting_runs: Vec<u64>,
    pub jobs: Vec<eden_protocol::runtime::JobStatus>,
    pub edit_target: String,
}
/// Secret values and host authority are excluded; sources describe the configured merge layers.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct InstanceConfiguration {
    pub id: String,
    pub package: String,
    pub generation: Option<u64>,
    pub state: String,
    pub selected: bool,
    pub effective: Value,
    pub description: p::Description,
    pub sources: Vec<String>,
    pub field_sources: BTreeMap<String, String>,
    pub secrets_configured: BTreeMap<String, bool>,
}
/// One management model consumed by all headless frontends.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Inspection {
    pub revision: u64,
    pub instances: Vec<InstanceConfiguration>,
    pub installed: Vec<InstalledPackage>,
    pub operations: Vec<Receipt>,
}
#[derive(Default)]
pub(crate) struct Manager {
    pub revision: u64,
    pub next: u64,
    pub active: bool,
    pub blocked: BTreeSet<String>,
    pub receipts: BTreeMap<u64, Receipt>,
    pub descriptions: BTreeMap<String, p::Description>,
    pub sources: BTreeMap<String, BTreeMap<String, String>>,
}
fn fault(code: &str, message: impl Into<String>) -> Fault {
    Fault::new(code, "configuration", message)
}
pub(crate) fn specs(composition: &Composition) -> Vec<InstanceSpec> {
    let mut result = composition.runtime.instances.clone();
    for package in &composition.packages {
        if !result
            .iter()
            .any(|s| s.package == package.descriptor.package)
        {
            result.push(InstanceSpec {
                id: package.descriptor.package.clone(),
                package: package.descriptor.package.clone(),
                scope: String::new(),
                owner: None,
                dependencies: vec![],
                config: None,
            });
        }
    }
    result
}
pub(crate) fn config(composition: &Composition, id: &str) -> Result<(InstanceSpec, Value), Fault> {
    let spec = specs(composition)
        .into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| fault("InvalidInput", "unknown instance"))?;
    let manifest = composition
        .packages
        .iter()
        .find(|p| p.descriptor.package == spec.package)
        .ok_or_else(|| fault("MissingDependency", "missing package"))?;
    let mut value = spec
        .config
        .clone()
        .unwrap_or_else(|| manifest.config.clone());
    if let Some(object) = value.as_object_mut() {
        object.remove(eden_protocol::environment::CONFIG_KEY);
    }
    Ok((spec, value))
}
fn set_config(composition: &mut Composition, mut spec: InstanceSpec, value: Value) {
    spec.config = Some(value);
    if let Some(current) = composition
        .runtime
        .instances
        .iter_mut()
        .find(|s| s.id == spec.id)
    {
        *current = spec;
    } else {
        composition.runtime.instances.push(spec);
    }
}
impl Session {
    async fn configuration_service<T: serde::de::DeserializeOwned>(
        &self,
        id: &str,
        request: p::PluginRequest,
    ) -> Result<T, Fault> {
        let value = self
            .0
            .kernel
            .get()?
            .invoke_package_management(
                id,
                Request {
                    execution: None,
                    session_id: self.id(),
                    run_id: 0,
                    contract: p::CONFIGURATION.into(),
                    payload: json!(request),
                },
                Cancellation::default(),
            )
            .await
            .into_result()?;
        serde_json::from_value(value).map_err(|_| {
            fault(
                "IncompatibleContract",
                "invalid configuration service reply",
            )
        })
    }
    async fn description(
        &self,
        composition: &Composition,
        spec: &InstanceSpec,
    ) -> Result<p::Description, Fault> {
        if composition.packages.iter().any(|m| {
            m.descriptor.package == spec.package
                && m.descriptor.provides.iter().any(|s| s == p::CONFIGURATION)
        }) {
            if !self.0.kernel.get()?.instance_running(&spec.id) {
                return self
                    .0
                    .configuration
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .descriptions
                    .get(&spec.id)
                    .cloned()
                    .ok_or_else(|| fault("Unavailable", "configuration description unavailable"));
            }
            let description: p::Description = self
                .configuration_service(&spec.id, p::PluginRequest::Describe)
                .await?;
            self.0
                .configuration
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .descriptions
                .insert(spec.id.clone(), description.clone());
            Ok(description)
        } else {
            Ok(p::Description::default())
        }
    }
    /// Inspect live instance values with secret redaction; no chat run is started.
    pub async fn inspect_configuration(&self) -> Result<Inspection, Fault> {
        let kernel = self.0.kernel.get()?;
        let composition = kernel.composition();
        let (revision, operations) = {
            let manager = self
                .0
                .configuration
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            (
                manager.revision,
                manager.receipts.values().cloned().collect(),
            )
        };
        let mut instances = vec![];
        for spec in specs(&composition) {
            if composition::internal_host_package(&spec.package) {
                continue;
            }
            let (_, effective) = config(&composition, &spec.id)?;
            let mut description = self.description(&composition, &spec).await?;
            description.defaults = p::redact(&description.defaults, &description.secret_paths);
            let generation = kernel
                .instance_identity(&spec.id)
                .ok()
                .map(|i| i.generation);
            let selected = composition.roles.values().any(|id| id == &spec.id)
                || composition.runtime.scopes.values().any(|s| {
                    s.bindings
                        .values()
                        .any(|b| b.tail == spec.id || b.wrappers.contains(&spec.id))
                });
            let field_sources = self
                .0
                .configuration
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .sources
                .get(&spec.id)
                .cloned()
                .unwrap_or_default();
            let sources = field_sources
                .values()
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            instances.push(InstanceConfiguration {
                id: spec.id.clone(),
                package: spec.package,
                generation,
                state: if kernel.instance_running(&spec.id) {
                    "running"
                } else {
                    "unavailable"
                }
                .into(),
                selected,
                secrets_configured: description
                    .secret_paths
                    .iter()
                    .map(|path| {
                        (
                            path.clone(),
                            effective.pointer(path).is_some_and(|v| !v.is_null()),
                        )
                    })
                    .collect(),
                effective: p::redact(&effective, &description.secret_paths),
                description,
                sources,
                field_sources,
            });
        }
        Ok(Inspection {
            revision,
            instances,
            installed: configuration_metadata::installed(&composition)?,
            operations,
        })
    }
    async fn prepare_configuration(
        &self,
        change: &Change,
    ) -> Result<(Composition, p::Description, p::Validation, Application), Fault> {
        {
            let manager = self
                .0
                .configuration
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if manager.revision != change.revision {
                return Err(fault("Conflict", "configuration revision changed"));
            }
            if manager.active {
                return Err(fault("Conflict", "configuration transaction in progress"));
            }
        }
        let mut candidate = self.0.kernel.composition();
        let (spec, old) = config(&candidate, &change.instance)?;
        if composition::internal_host_package(&spec.package) {
            return Err(fault(
                "InvalidInput",
                "host-owned instances are not editable",
            ));
        }
        if change
            .patch
            .get(eden_protocol::environment::CONFIG_KEY)
            .is_some()
        {
            return Err(fault("InvalidInput", "host environment is not editable"));
        }
        let description = self.description(&candidate, &spec).await?;
        if !description.editable_layers.is_empty()
            && !description.editable_layers.contains(&p::Layer::Explicit)
        {
            return Err(fault(
                "InvalidInput",
                "instance does not permit explicit configuration overrides",
            ));
        }
        if description
            .secret_paths
            .iter()
            .any(|path| change.patch.pointer(path).is_some())
        {
            return Err(fault(
                "PrivateInputRequired",
                "secret fields cannot be supplied through public configuration edits",
            ));
        }
        let mut value = old.clone();
        eden_workspace::merge(&mut value, change.patch.clone());
        let mut validation = p::validate(&description, &value)?;
        validation
            .errors
            .extend(p::validate_public_edit(&description, &old, &value).errors);
        if validation.errors.is_empty()
            && self.0.kernel.get()?.instance_running(&spec.id)
            && candidate.packages.iter().any(|m| {
                m.descriptor.package == spec.package
                    && m.descriptor.provides.iter().any(|s| s == p::CONFIGURATION)
            })
        {
            let business: p::Validation = self
                .configuration_service(
                    &spec.id,
                    p::PluginRequest::Validate {
                        config: value.clone(),
                    },
                )
                .await?;
            validation.errors.extend(business.errors);
        }
        let application = if change.replacement.is_none()
            && old == value
            && self.0.kernel.get()?.instance_running(&spec.id)
        {
            Application::Unchanged
        } else if change.replacement.is_none()
            && self.0.kernel.get()?.instance_running(&spec.id)
            && candidate.packages.iter().any(|m| {
                m.descriptor.package == spec.package
                    && m.descriptor.provides.iter().any(|s| s == p::CONFIGURATION)
            })
            && p::is_live_change(&description, &old, &value)
        {
            Application::Live
        } else {
            Application::Restart
        };
        if let Some(path) = &change.replacement {
            let selected = workspace_setup::prepare(
                path,
                self.cwd(),
                &self.0.workspace_options,
                &self.0.events,
                self.0.history_path.as_deref(),
            )?;
            let replacement = selected
                .packages
                .iter()
                .find(|p| p.descriptor.package == spec.package)
                .ok_or_else(|| {
                    fault(
                        "MissingDependency",
                        "replacement composition lacks selected package",
                    )
                })?
                .clone();
            let manifest = candidate
                .packages
                .iter_mut()
                .find(|p| p.descriptor.package == spec.package)
                .ok_or_else(|| fault("MissingDependency", "missing package"))?;
            *manifest = replacement;
        }
        if application != Application::Unchanged {
            set_config(&mut candidate, spec, value);
        }
        eden_kernel::preflight(&candidate)?;
        Ok((candidate, description, validation, application))
    }
    /// Static and business validation run without stopping the effective instance.
    pub async fn validate_configuration(&self, change: Change) -> Result<p::Validation, Fault> {
        Ok(self.prepare_configuration(&change).await?.2)
    }
    /// Show the actual dependency/ownership closure and foreground runs whose settlement is required.
    pub async fn preview_configuration(&self, change: Change) -> Result<Preview, Fault> {
        let (candidate, _, validation, application) = self.prepare_configuration(&change).await?;
        if !validation.errors.is_empty() {
            return Err(fault(
                "ValidationFailed",
                serde_json::to_string(&validation).unwrap_or_default(),
            ));
        }
        let kernel = self.0.kernel.get()?;
        let affected = match application {
            Application::Unchanged => vec![],
            Application::Live => vec![change.instance.clone()],
            Application::Restart => {
                let mut ids = kernel.affected_instances(&candidate)?;
                if ids.is_empty() && !kernel.instance_running(&change.instance) {
                    ids = kernel.restart_closure(std::slice::from_ref(&change.instance))?;
                }
                ids
            }
        };
        if application == Application::Restart {
            kernel.validate_replacement(&candidate, &affected)?;
        }
        let waiting_runs = self.configuration_runs(&affected)?;
        Ok(Preview {
            revision: change.revision,
            instance: change.instance,
            affected: affected.clone(),
            application,
            waiting_runs,
            jobs: kernel
                .jobs()
                .into_iter()
                .filter(|j| affected.contains(&j.owner.id))
                .collect(),
            edit_target: "explicit session override (saved with persistent session)".into(),
        })
    }
    fn foreground_affected(&self, affected: &[String]) -> Result<bool, Fault> {
        let roles: &[&str] = if self.0.coding {
            &[
                c::LOOP,
                c::CONTEXT,
                c::PROVIDER,
                c::TOOL,
                c::STORE,
                c::QUEUE,
                eden_protocol::resources::SOURCE,
                eden_protocol::resources::TOOL_CATALOG,
            ]
        } else {
            &[
                AGENT_LOOP,
                eden_protocol::CONTEXT,
                eden_protocol::PROVIDER,
                eden_protocol::TOOL,
            ]
        };
        let kernel = self.0.kernel.get()?;
        for role in roles {
            if kernel.affected_by_contract(role, affected)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn configuration_runs(&self, affected: &[String]) -> Result<Vec<u64>, Fault> {
        let kernel = self.0.kernel.get()?;
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut runs = vec![];

        let store_affected = self.0.coding && kernel.affected_by_contract(c::STORE, affected)?;
        if let Some((id, _)) = &state.active
            && (state.management || store_affected || self.foreground_affected(affected)?)
        {
            runs.push(*id);
        }
        for id in state.commands.keys() {
            let contract = state
                .command_contracts
                .get(id)
                .map(String::as_str)
                .unwrap_or(eden_protocol::resources::COMMAND);
            if store_affected || kernel.affected_by_contract(contract, affected)? {
                runs.push(*id);
            }
        }
        if !state.shells.is_empty()
            && (store_affected
                || kernel.affected_by_contract(eden_protocol::shell::USER_SHELL, affected)?)
        {
            runs.extend(state.shells.keys().copied());
        }
        Ok(runs)
    }
    /// Accept a revision-bound management operation. The session retains execution after callers detach.
    pub async fn apply_configuration(&self, change: Change, mode: ApplyMode) -> Result<u64, Fault> {
        let (candidate, _, validation, application) = self.prepare_configuration(&change).await?;
        if !validation.errors.is_empty() {
            return Err(fault(
                "ValidationFailed",
                serde_json::to_string(&validation).unwrap_or_default(),
            ));
        }
        let kernel = self.0.kernel.get()?;
        let affected = match application {
            Application::Unchanged => vec![],
            Application::Live => vec![change.instance.clone()],
            Application::Restart => {
                let mut ids = kernel.affected_instances(&candidate)?;
                if ids.is_empty() && !kernel.instance_running(&change.instance) {
                    ids = kernel.restart_closure(std::slice::from_ref(&change.instance))?;
                }
                ids
            }
        };
        if application == Application::Restart {
            kernel.validate_replacement(&candidate, &affected)?;
        }
        for spec in specs(&kernel.composition())
            .iter()
            .filter(|s| affected.contains(&s.id))
        {
            self.description(&kernel.composition(), spec).await?;
        }
        let operation;
        {
            let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.closed || state.management {
                return Err(fault(
                    "Unavailable",
                    "session closed or switching composition",
                ));
            }
            let mut manager = self
                .0
                .configuration
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if manager.active || manager.revision != change.revision {
                return Err(fault(
                    "Conflict",
                    "configuration revision changed or transaction active",
                ));
            }
            manager.next += 1;
            operation = manager.next;
            manager.active = true;
            manager.blocked = affected.iter().cloned().collect();
            let revision = manager.revision;
            manager.receipts.insert(
                operation,
                Receipt {
                    operation,
                    revision,
                    instance: change.instance.clone(),
                    affected: affected.clone(),
                    status: Status::Waiting,
                    error: None,
                    recovery_error: None,
                },
            );
        }
        let session = self.clone();
        tokio::spawn(async move {
            session
                .apply_configuration_owned(
                    operation,
                    change,
                    candidate,
                    affected,
                    application,
                    mode,
                )
                .await;
        });
        Ok(operation)
    }
    /// Inspect acceptance, waiting, application and recovery using the same identity after reconnect.
    pub fn configuration_operation(&self, operation: u64) -> Result<Receipt, Fault> {
        self.0
            .configuration
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .receipts
            .get(&operation)
            .cloned()
            .ok_or_else(|| fault("InvalidInput", "unknown management operation"))
    }
    /// Completion includes cleanup, one recovery attempt if allowed, and the binding commit.
    pub async fn wait_configuration(&self, operation: u64) -> Result<Receipt, Fault> {
        loop {
            let changed = self.0.settled.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let receipt = self.configuration_operation(operation)?;
            if receipt.status.finished() {
                return Ok(receipt);
            }
            changed.await;
        }
    }
    pub(crate) fn configuration_admission(
        &self,
        contract: &str,
        global: bool,
    ) -> Result<(), Fault> {
        let manager = self
            .0
            .configuration
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !manager.active {
            return Ok(());
        }
        let affected: Vec<_> = manager.blocked.iter().cloned().collect();
        if global
            || ((contract == c::LOOP || contract == AGENT_LOOP)
                && self.foreground_affected(&affected)?)
            || self
                .0
                .kernel
                .get()?
                .affected_by_contract(contract, &affected)?
            || (self.0.coding
                && self
                    .0
                    .kernel
                    .get()?
                    .affected_by_contract(c::STORE, &affected)?)
        {
            Err(fault(
                "Unavailable",
                "affected instances are waiting for configuration application",
            ))
        } else {
            Ok(())
        }
    }
    async fn apply_configuration_owned(
        &self,
        operation: u64,
        change: Change,
        candidate: Composition,
        affected: Vec<String>,
        application: Application,
        mode: ApplyMode,
    ) {
        let mut receipt = self
            .configuration_operation(operation)
            .expect("accepted operation");
        let result = self
            .execute_configuration(operation, &change, candidate, &affected, application, mode)
            .await;
        receipt.revision += 1;
        match result {
            Ok(()) => {
                receipt.status = Status::Applied;
            }
            Err(failure) => {
                let (status, error, recovery) = *failure;
                receipt.status = status;
                receipt.error = Some(error);
                receipt.recovery_error = recovery;
            }
        }
        if receipt.status != Status::Applied
            && self.0.coding
            && let Err(error) = self
                .configuration_host_service::<_, c::StoreReply>(
                    c::STORE,
                    &c::StoreRequest::Append {
                        run_id: 0,
                        kind: "configuration_result".into(),
                        payload: json!(receipt),
                    },
                )
                .await
        {
            receipt.recovery_error.get_or_insert(error);
        }
        let mut manager = self
            .0
            .configuration
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        manager.revision = receipt.revision;
        if receipt.status == Status::Applied {
            configuration_metadata::overlay(
                manager.sources.entry(change.instance.clone()).or_default(),
                &change.patch,
                "explicit_session",
            );
        }
        manager.active = false;
        manager.blocked.clear();
        manager.receipts.insert(operation, receipt.clone());
        self.0
            .events
            .push(0, "configuration_settled", json!(receipt));
        self.0.settled.notify_waiters();
    }
    async fn execute_configuration(
        &self,
        operation: u64,
        change: &Change,
        candidate: Composition,
        affected: &[String],
        application: Application,
        mode: ApplyMode,
    ) -> Result<(), Box<(Status, Fault, Option<Fault>)>> {
        let ordinary = |e| Box::new((Status::Restored, e, None));
        let kernel = self.0.kernel.get().map_err(ordinary)?;
        let affected_inputs = [c::QUEUE, c::CODING_CONTROL, c::STORE]
            .into_iter()
            .try_fold(false, |found, contract| {
                kernel
                    .affected_by_contract(contract, affected)
                    .map(|next| found || next)
            })
            .map_err(ordinary)?;
        loop {
            let changed = self.0.settled.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let runs = self.configuration_runs(affected).map_err(ordinary)?;
            if runs.is_empty()
                && (!affected_inputs
                    || self
                        .0
                        .state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .pending_inputs
                        == 0)
            {
                break;
            }
            if mode == ApplyMode::Cancel {
                for id in runs {
                    let _ = self.cancel(id);
                }
            }
            changed.await;
        }
        let old = kernel.composition();
        let records = if self.0.coding {
            self.history().await.map_err(ordinary)?
        } else {
            vec![]
        };
        *self
            .0
            .offline_records
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = records.clone();
        self.0
            .configuration
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .receipts
            .get_mut(&operation)
            .expect("accepted operation")
            .status = Status::Applying;
        workspace_setup::register(self, &candidate, true).map_err(ordinary)?;
        kernel.set_reconfiguring(affected, true).map_err(ordinary)?;
        self.0.presentation.invalidate_instances(affected);
        let apply = async {
            if application == Application::Live {
                let (_, value) = config(&candidate, &change.instance)?;
                let validation: p::Validation = self
                    .configuration_service(
                        &change.instance,
                        p::PluginRequest::Update { config: value },
                    )
                    .await?;
                if !validation.errors.is_empty() {
                    return Err(fault("ValidationFailed", "plugin rejected live update"));
                }
            } else if application == Application::Restart {
                kernel
                    .replace_instances(candidate.clone(), affected)
                    .await?;
                self.restore_configuration_store(&kernel, affected, &records)
                    .await?;
            }
            let locked = composition::binding(&candidate, self.cwd())?;
            if self.0.coding {
                let receipt = Receipt {
                    operation,
                    revision: change.revision + 1,
                    instance: change.instance.clone(),
                    affected: affected.to_vec(),
                    status: Status::Applied,
                    error: None,
                    recovery_error: None,
                };
                self.configuration_host_service::<_, c::StoreReply>(
                    c::STORE,
                    &c::StoreRequest::AppendBatch {
                        run_id: 0,
                        entries: vec![
                            c::RecordDraft {
                                kind: "configuration_commit".into(),
                                payload: json!({
                                    "change": change,
                                    "application": application,
                                    "receipt": receipt,
                                    "replacement_package": saved_replacement(&candidate, change),
                                }),
                            },
                            c::RecordDraft {
                                kind: "composition_lock".into(),
                                payload: locked,
                            },
                        ],
                    },
                )
                .await?;
            }
            if application == Application::Live {
                kernel.update_configuration_snapshot(candidate.clone())?;
            }
            Ok::<_, Fault>(())
        }
        .await;
        if let Err(error) = apply {
            if error.code == "CleanupFailure" {
                return Err(Box::new((Status::CleanupFailed, error, None)));
            }
            let restored = async {
                if application == Application::Live {
                    let (_, value) = config(&old, &change.instance)?;
                    let validation: p::Validation = self
                        .configuration_service(
                            &change.instance,
                            p::PluginRequest::Update { config: value },
                        )
                        .await?;
                    if !validation.errors.is_empty() {
                        return Err(fault("ValidationFailed", "plugin rejected live update"));
                    }
                    kernel.update_configuration_snapshot(old)?;
                } else if application == Application::Restart {
                    kernel.replace_instances(old, affected).await?;
                    self.restore_configuration_store(&kernel, affected, &records)
                        .await?;
                }
                Ok::<_, Fault>(())
            }
            .await;
            return Err(Box::new(match restored {
                Ok(()) => {
                    let _ = kernel.set_reconfiguring(affected, false);
                    (Status::Restored, error, None)
                }
                Err(recovery) => (Status::RecoveryFailed, error, Some(recovery)),
            }));
        }
        // The pending union retains both code versions; old libraries remain usable for recovery.
        kernel
            .set_reconfiguring(affected, false)
            .map_err(ordinary)?;
        Ok(())
    }
    async fn configuration_host_service<I: serde::Serialize, O: serde::de::DeserializeOwned>(
        &self,
        contract: &str,
        input: &I,
    ) -> Result<O, Fault> {
        let value = self
            .0
            .kernel
            .get()?
            .invoke_management(
                Request {
                    execution: None,
                    session_id: self.id(),
                    run_id: 0,
                    contract: contract.into(),
                    payload: json!(input),
                },
                Cancellation::default(),
            )
            .await
            .into_result()?;
        serde_json::from_value(value)
            .map_err(|_| fault("IncompatibleContract", "invalid management service reply"))
    }
    async fn restore_configuration_store(
        &self,
        kernel: &Kernel,
        affected: &[String],
        records: &[c::Record],
    ) -> Result<(), Fault> {
        if !self.0.coding {
            return Ok(());
        }
        if kernel.affected_by_contract(c::STORE, affected)? {
            let request = match &self.0.history_path {
                Some(path) => c::StoreRequest::Open {
                    path: Some(path.to_string_lossy().into_owned()),
                    session_id: self.id(),
                },
                None => c::StoreRequest::RestoreMemory {
                    session_id: self.id(),
                    records: records.to_vec(),
                },
            };
            self.configuration_host_service::<_, c::StoreReply>(c::STORE, &request)
                .await?;
        }
        if kernel.affected_by_contract(c::QUEUE, affected)? {
            self.configuration_host_service::<_, Vec<c::QueueEntry>>(
                c::QUEUE,
                &c::QueueRequest::Restore,
            )
            .await?;
        }
        Ok(())
    }
}
/// Reapply only committed explicit edits before checking the persisted composition identity.
pub(crate) fn replay(selected: &mut Composition, records: &[c::Record]) -> Result<Manager, Fault> {
    let mut manager = Manager::default();
    let start = records
        .iter()
        .rposition(|r| r.kind == "configuration_reset")
        .map(|index| {
            manager.revision = records[index].payload["revision"]
                .as_u64()
                .unwrap_or_default();
            index + 1
        })
        .unwrap_or_default();
    for record in records[start..]
        .iter()
        .filter(|r| r.kind == "configuration_commit")
    {
        let change: Change = serde_json::from_value(record.payload["change"].clone())
            .map_err(|_| fault("InvalidInput", "invalid saved configuration change"))?;
        let receipt: Receipt = serde_json::from_value(record.payload["receipt"].clone())
            .map_err(|_| fault("InvalidInput", "invalid saved configuration receipt"))?;
        if change.replacement.is_some() {
            let mut replacement: eden_protocol::PackageManifest =
                serde_json::from_value(record.payload["replacement_package"].clone())
                    .map_err(|_| fault("InvalidInput", "missing committed replacement manifest"))?;
            let current = selected
                .packages
                .iter_mut()
                .find(|m| m.descriptor.package == replacement.descriptor.package)
                .ok_or_else(|| fault("MissingDependency", "saved replacement package missing"))?;
            replacement.config = current.config.clone();
            *current = replacement;
        }
        if record.payload["application"] != "unchanged" {
            let (spec, mut value) = config(selected, &change.instance)?;
            eden_workspace::merge(&mut value, change.patch);
            set_config(selected, spec, value);
        }
        manager.revision = receipt.revision;
        manager.next = manager.next.max(receipt.operation);
        manager.receipts.insert(receipt.operation, receipt);
    }
    for record in records[start..]
        .iter()
        .filter(|r| r.kind == "configuration_result")
    {
        let receipt: Receipt = serde_json::from_value(record.payload.clone())
            .map_err(|_| fault("InvalidInput", "invalid saved configuration result"))?;
        manager.next = manager.next.max(receipt.operation);
        manager.revision = manager.revision.max(receipt.revision);
        manager.receipts.insert(receipt.operation, receipt);
    }
    Ok(manager)
}

fn saved_replacement(
    candidate: &Composition,
    change: &Change,
) -> Option<eden_protocol::PackageManifest> {
    change.replacement.as_ref()?;
    let (spec, _) = config(candidate, &change.instance).ok()?;
    let mut package = candidate
        .packages
        .iter()
        .find(|m| m.descriptor.package == spec.package)?
        .clone();
    package.config = Value::Null;
    Some(package)
}

pub(crate) fn replay_sources(manager: &mut Manager, records: &[c::Record]) {
    let start = records
        .iter()
        .rposition(|r| r.kind == "configuration_reset")
        .map_or(0, |i| i + 1);
    for record in records[start..]
        .iter()
        .filter(|r| r.kind == "configuration_commit")
    {
        if let Ok(change) = serde_json::from_value::<Change>(record.payload["change"].clone()) {
            configuration_metadata::overlay(
                manager.sources.entry(change.instance).or_default(),
                &change.patch,
                "explicit_session",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(kind: &str, payload: Value) -> c::Record {
        c::Record {
            schema_version: 2,
            session_id: 1,
            sequence: 1,
            run_id: 0,
            parent_id: None,
            branch: "main".into(),
            kind: kind.into(),
            payload,
        }
    }
    #[test]
    fn replay_discards_edits_before_explicit_switch_and_preserves_failed_revision() {
        let mut composition: Composition = serde_json::from_value(json!({
            "packages": [],
            "roles": {},
        }))
        .unwrap();
        let receipt = Receipt {
            operation: 4,
            revision: 6,
            instance: "gone".into(),
            affected: vec!["gone".into()],
            status: Status::Restored,
            error: None,
            recovery_error: None,
        };
        let records = vec![
            record(
                "configuration_commit",
                json!({
                    "change": {
                        "instance": "gone",
                        "revision": 0,
                        "patch": { "n": 1 },
                        "replacement": null,
                    },
                    "receipt": receipt,
                }),
            ),
            record("configuration_reset", json!({ "revision": 5 })),
            record("configuration_result", json!(receipt)),
        ];
        let manager = replay(&mut composition, &records).unwrap();
        assert_eq!(manager.revision, 6);
        assert_eq!(manager.receipts[&4].status, Status::Restored);
    }
    #[test]
    fn unchanged_commit_does_not_add_an_explicit_instance_on_reopen() {
        let mut composition: Composition = serde_json::from_value(json!({
            "packages": [],
            "roles": {},
        }))
        .unwrap();
        let receipt = Receipt {
            operation: 1,
            revision: 1,
            instance: "legacy".into(),
            affected: vec![],
            status: Status::Applied,
            error: None,
            recovery_error: None,
        };
        let record = record(
            "configuration_commit",
            json!({
                "application": "unchanged",
                "change": { "instance": "legacy", "revision": 0, "patch": {}, "replacement": null },
                "receipt": receipt,
            }),
        );
        let manager = replay(&mut composition, &[record]).unwrap();
        assert!(composition.runtime.instances.is_empty());
        assert_eq!(manager.revision, 1);
    }
    #[test]
    fn replacement_replay_uses_committed_manifest_and_preserves_private_configuration() {
        let mut composition: Composition = serde_json::from_value(json!({
            "packages": [{
                "descriptor": { "package": "plugin", "version": "old", "provides": [] },
                "host": eden_protocol::CONTRACT,
                "sdk": eden_protocol::CONTRACT,
                "target": "test",
                "library": "old.so",
                "config": { "secret": "kept", "n": 1 },
            }],
            "roles": {},
        }))
        .unwrap();
        let mut replacement = composition.packages[0].clone();
        replacement.library = "new.so".into();
        replacement.descriptor.version = "new".into();
        replacement.config = Value::Null;
        let receipt = Receipt {
            operation: 1,
            revision: 1,
            instance: "plugin".into(),
            affected: vec!["plugin".into()],
            status: Status::Applied,
            error: None,
            recovery_error: None,
        };
        let record = record(
            "configuration_commit",
            json!({
                "change": {
                    "instance": "plugin",
                    "revision": 0,
                    "patch": { "n": 2 },
                    "replacement": "deleted-composition.json",
                },
                "receipt": receipt,
                "replacement_package": replacement,
            }),
        );
        replay(&mut composition, &[record]).unwrap();
        assert_eq!(composition.packages[0].library, "new.so");
        let (_, effective) = config(&composition, "plugin").unwrap();
        assert_eq!(effective, json!({ "secret": "kept", "n": 2 }));
    }
}
