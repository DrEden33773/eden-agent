//! Composition binding records: what a session's packages were when it was written.
use super::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{io::Read, path::PathBuf};
fn failure(message: impl Into<String>) -> Fault {
    Fault::new("Unavailable", "composition", message)
}
pub(crate) fn binding(composition: &eden_protocol::Composition, cwd: &str) -> Result<Value, Fault> {
    let mut packages = BTreeMap::new();
    for package in &composition.packages {
        let mut file = std::fs::File::open(&package.library).map_err(|e| failure(e.to_string()))?;
        let mut hash = Sha256::new();
        let mut bytes = [0; 65536];
        loop {
            let n = file.read(&mut bytes).map_err(|e| failure(e.to_string()))?;
            if n == 0 {
                break;
            }
            hash.update(&bytes[..n]);
        }
        packages.insert(
            package.descriptor.package.clone(),
            json!({
                "descriptor": package.descriptor,
                "target": package.target,
                "requires": package.requires,
                "sha256": format!("{:x}", hash.finalize()),
            }),
        );
    }
    let mut binding = json!({
        "cwd": cwd,
        "roles": composition.roles,
        "packages": packages,
        "library_locations": composition
            .packages
            .iter()
            .map(|p| &p.library)
            .chain(composition.resource_packages.iter().map(|p| &p.root))
            .collect::<Vec<_>>(),
    });
    if !composition.resource_packages.is_empty() {
        binding["resource_packages"] = json!(
            composition
                .resource_packages
                .iter()
                .map(|package| json!({ "manifest": package.manifest, "digest": package.digest }))
                .collect::<Vec<_>>()
        );
    }
    Ok(binding)
}
/// Locations support data-only copy reference tracking, but are not package identity.
pub(crate) fn equivalent(left: &Value, right: &Value) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    for value in [&mut left, &mut right] {
        if let Some(object) = value.as_object_mut() {
            object.remove("library_locations");
        }
    }
    left == right
}
impl Session {
    /// Explicit generation replacement; static failure leaves the current generation live.
    pub fn switch_composition(&self, path: PathBuf) -> Result<u64, Fault> {
        self.start(true, move |session, run_id, cancel| async move {
            management::as_terminal(session.switch_owned(path, run_id, cancel).await)
        })
    }
    async fn switch_owned(
        &self,
        path: PathBuf,
        run_id: u64,
        cancel: Cancellation,
    ) -> Result<Value, Fault> {
        let mut selected = workspace_setup::prepare(
            &path,
            self.cwd(),
            &self.0.workspace_options,
            &self.0.events,
            self.0.history_path.as_deref(),
        )?;
        eden_kernel::preflight(&selected)?;
        if selected.roles.contains_key(c::LOOP) != self.0.coding {
            return Err(failure("switch cannot change the session protocol family"));
        }
        let base = path.parent().unwrap_or(Path::new("."));
        for package in &mut selected.packages {
            package.library = std::fs::canonicalize(base.join(&package.library))
                .map_err(|e| failure(e.to_string()))?
                .to_string_lossy()
                .into_owned();
        }
        let locked = binding(&selected, self.cwd())?;
        let records = if self.0.coding {
            self.history().await?
        } else {
            vec![]
        };
        *self
            .0
            .offline_records
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = records.clone();
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Fault::new(
                    "Cancelled",
                    "composition",
                    "switch cancelled before stopping the current generation"
                )),
            _ = std::future::ready(()) => {}
        }
        if self.0.coding && self.0.kernel.available() {
            self.service::<_, c::StoreReply>(run_id, c::STORE, &c::StoreRequest::Close)
                .await?;
        }
        self.0.kernel.shutdown().await?;
        self.0.events.push(
            run_id,
            "composition_unavailable",
            json!({ "reason": "explicit switch stopped the previous generation" }),
        );
        // Once the old generation stops, finish initialization/cleanup even if
        // the requester cancels; no abandoned initializer can escape ownership.
        let kernel =
            Kernel::load_resolved(selected, Path::new("."), self.id(), self.0.events.clone())
                .await?;
        let setup = async {
            if self.0.coding {
                let request = match &self.0.history_path {
                    Some(path) => c::StoreRequest::Open {
                        path: Some(path.to_string_lossy().into_owned()),
                        session_id: self.id(),
                    },
                    None => c::StoreRequest::RestoreMemory {
                        session_id: self.id(),
                        records,
                    },
                };
                kernel
                    .invoke(
                        Request {
                            session_id: self.id(),
                            run_id,
                            contract: c::STORE.into(),
                            payload: json!(request),
                        },
                        Cancellation::default(),
                    )
                    .await
                    .into_result()?;
                kernel
                    .invoke(
                        Request {
                            session_id: self.id(),
                            run_id,
                            contract: c::QUEUE.into(),
                            payload: json!(c::QueueRequest::Restore),
                        },
                        Cancellation::default(),
                    )
                    .await
                    .into_result()?;
                workspace_setup::register(self, kernel.composition(), true)?;
                kernel
                    .invoke(
                        Request {
                            session_id: self.id(),
                            run_id,
                            contract: c::STORE.into(),
                            payload: json!(c::StoreRequest::Append {
                                run_id,
                                kind: "composition_lock".into(),
                                payload: locked.clone()
                            }),
                        },
                        Cancellation::default(),
                    )
                    .await
                    .into_result()?;
                workspace_setup::register(self, kernel.composition(), false)?;
            }
            Ok::<_, Fault>(())
        }
        .await;
        if let Err(error) = setup {
            let stopped = kernel.shutdown().await;
            if let Err(cleanup) = stopped {
                return Err(failure(format!("{error}; cleanup: {cleanup}")));
            }
            return Err(error);
        }
        self.0.kernel.install(kernel);
        self.0.events.push(run_id, "composition_switched", locked);
        Ok(json!({ "available": true, "composition": path }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_bindings_require_same_content_but_allow_package_relocation() {
        let mut composition: eden_protocol::Composition = serde_json::from_value(json!({
            "packages": [],
            "roles": {},
            "resource_packages": [{
                "manifest": {
                    "name": "text",
                    "version": "1.0.0",
                    "skills": ["skills"],
                    "templates": [],
                },
                "root": "/old/package",
                "digest": "locked-content",
            }],
        }))
        .unwrap();
        let original = binding(&composition, "/project").unwrap();
        composition.resource_packages[0].root = "/new/package".into();
        let moved = binding(&composition, "/project").unwrap();
        assert!(equivalent(&original, &moved));
        assert_eq!(moved["library_locations"], json!(["/new/package"]));
        composition.resource_packages[0].digest = "different-content".into();
        assert!(!equivalent(
            &original,
            &binding(&composition, "/project").unwrap()
        ));
    }

    #[test]
    fn legacy_empty_resource_binding_keeps_its_serialized_shape() {
        let composition = serde_json::from_value(json!({ "packages": [], "roles": {} })).unwrap();
        let current = binding(&composition, "/project").unwrap();
        assert!(equivalent(
            &current,
            &json!({ "cwd": "/project", "roles": {}, "packages": {}, "library_locations": [] })
        ));
    }
}
