//! Local replacements keep unrelated publications and incarnation-bound tokens alive.
use super::*;
use std::collections::BTreeSet;

impl Router {
    pub(crate) fn admit(&self, ids: &[String]) -> Result<(), Fault> {
        let gated = self.reconfiguring.lock().unwrap_or_else(|e| e.into_inner());
        if ids.iter().any(|id| gated.contains(id)) {
            return Err(Fault::new(
                "Reconfiguring",
                "runtime",
                "affected instance is being reconfigured",
            ));
        }
        Ok(())
    }
}

impl Kernel {
    /// Trusted host transactions can commit storage and configuration while client admission is
    /// gated. This bypass must never be selected by a serialized client request.
    pub async fn invoke_management(&self, request: Request, cancel: Cancellation) -> Terminal {
        if request.session_id != self.router.session_id || !self.router.open.load(Ordering::Acquire)
        {
            return Terminal::failed(Fault::new(
                "Unavailable",
                "router",
                "session closed or foreign session",
            ));
        }
        let binding = match self.router.graph.binding("", &request.contract) {
            Ok(binding) => binding,
            Err(error) => return Terminal::failed(error),
        };
        self.router
            .dispatch(
                request,
                String::new(),
                binding
                    .wrappers
                    .iter()
                    .chain(std::iter::once(&binding.tail))
                    .cloned()
                    .collect(),
                0,
                cancel,
                vec![],
                None,
            )
            .await
    }

    /// Current receipts let management preview which affected owners still have unsettled work.
    pub fn jobs(&self) -> Vec<p::runtime::JobStatus> {
        self.router
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(id, job)| p::runtime::JobStatus {
                id: *id,
                owner: job.owner.clone(),
                terminal: job.status.borrow().clone(),
            })
            .collect()
    }

    /// Normalized declarations include implicit package-named instances for legacy compositions.
    pub fn instance_specs(&self) -> Vec<p::runtime::InstanceSpec> {
        graph::Graph::build(&self.composition())
            .map(|graph| graph.instances.into_values().collect())
            .unwrap_or_default()
    }

    fn candidate_graph(&self, candidate: &Composition) -> Result<graph::Graph, Fault> {
        preflight(candidate)?;
        let current = self.composition();
        let old = graph::Graph::build(&current)?;
        let next = graph::Graph::build(candidate)?;
        let topology = |graph: &graph::Graph| {
            let mut instances = graph.instances.clone();
            for instance in instances.values_mut() {
                instance.config = None;
            }
            serde_json::json!([instances, graph.scopes, graph.dependencies])
        };
        if topology(&old) != topology(&next)
            || serde_json::to_value(&current.host_environment).ok()
                != serde_json::to_value(&candidate.host_environment).ok()
            || current.roles != candidate.roles
        {
            return Err(Fault::new(
                "UnsupportedChange",
                "replacement",
                "routing and host environment changes require a new session",
            ));
        }
        for manifest in &candidate.packages {
            let previous = current
                .packages
                .iter()
                .find(|old| old.descriptor.package == manifest.descriptor.package)
                .ok_or_else(|| {
                    Fault::new("UnsupportedChange", "replacement", "package set changed")
                })?;
            if previous.descriptor.provides != manifest.descriptor.provides
                || previous.requires != manifest.requires
            {
                return Err(Fault::new(
                    "UnsupportedChange",
                    "replacement",
                    "service declarations changed",
                ));
            }
        }
        Ok(next)
    }

    /// Compute the restart closure without executing plugin code. Children and declared consumers
    /// follow their providers; scope overrides keep isolated consumers outside that closure.
    pub fn affected_instances(&self, candidate: &Composition) -> Result<Vec<String>, Fault> {
        let next = self.candidate_graph(candidate)?;
        let current = self.composition();
        let old = graph::Graph::build(&current)?;
        let mut affected = BTreeSet::new();
        for (id, spec) in &next.instances {
            let before = current
                .packages
                .iter()
                .find(|m| m.descriptor.package == spec.package);
            let after = candidate
                .packages
                .iter()
                .find(|m| m.descriptor.package == spec.package);
            if serde_json::to_value(before).ok() != serde_json::to_value(after).ok()
                || old.instances[id].config != spec.config
            {
                affected.insert(id.clone());
            }
        }
        loop {
            let previous = affected.len();
            for (id, dependencies) in &next.dependencies {
                if dependencies
                    .iter()
                    .any(|dependency| affected.contains(dependency))
                {
                    affected.insert(id.clone());
                }
            }
            if affected.len() == previous {
                break;
            }
        }
        Ok(next
            .order
            .into_iter()
            .filter(|id| affected.contains(id))
            .collect())
    }

    /// Restart unavailable seeds even when their desired declaration has not changed. Recovery
    /// must recreate their consumers and owned children with the same dependency ordering.
    pub fn restart_closure(&self, seeds: &[String]) -> Result<Vec<String>, Fault> {
        if seeds
            .iter()
            .any(|id| !self.router.graph.instances.contains_key(id))
        {
            return Err(Fault::new(
                "InvalidInput",
                "replacement",
                "unknown restart seed",
            ));
        }
        let mut affected: BTreeSet<_> = seeds.iter().cloned().collect();
        loop {
            let before = affected.len();
            for (id, dependencies) in &self.router.graph.dependencies {
                if dependencies.iter().any(|id| affected.contains(id)) {
                    affected.insert(id.clone());
                }
            }
            if affected.len() == before {
                break;
            }
        }
        Ok(self
            .router
            .graph
            .order
            .iter()
            .filter(|id| affected.contains(*id))
            .cloned()
            .collect())
    }

    /// A foreground entry point is affected when its selected chain or declared transitive
    /// dependencies intersect the restart set, even before those dependencies have been called.
    pub fn affected_by_contract(&self, contract: &str, affected: &[String]) -> Result<bool, Fault> {
        let binding = match self.router.graph.binding("", contract) {
            Ok(binding) => binding,
            Err(error) if error.code == "MissingDependency" => return Ok(false),
            Err(error) => return Err(error),
        };
        let mut pending: Vec<_> = binding
            .wrappers
            .iter()
            .chain(std::iter::once(&binding.tail))
            .collect();
        let mut visited = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if !visited.insert(id) {
                continue;
            }
            if affected.contains(id) {
                return Ok(true);
            }
            pending.extend(&self.router.graph.dependencies[id]);
        }
        Ok(false)
    }

    /// Session owns the durable transaction and releases this foreground gate only after commit
    /// or recovery. Native initialization and finalizers can still call their live dependencies.
    pub fn set_reconfiguring(&self, ids: &[String], enabled: bool) -> Result<(), Fault> {
        if ids
            .iter()
            .any(|id| !self.router.graph.instances.contains_key(id))
        {
            return Err(Fault::new(
                "InvalidInput",
                "replacement",
                "unknown instance",
            ));
        }
        let mut gated = self
            .router
            .reconfiguring
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for id in ids {
            if enabled {
                gated.insert(id.clone());
            } else {
                gated.remove(id);
            }
        }
        Ok(())
    }

    /// Record a configuration already applied through its public live-update service. This does
    /// not call plugins or persist history; the transaction owner must do both before committing.
    pub fn update_configuration_snapshot(&self, candidate: Composition) -> Result<(), Fault> {
        self.candidate_graph(&candidate)?;
        *self.composition.lock().unwrap_or_else(|e| e.into_inner()) = candidate;
        Ok(())
    }

    /// Availability differs from identity: a failed cleanup retains its old identity for
    /// diagnostics while admission remains permanently closed.
    pub fn instance_running(&self, id: &str) -> bool {
        self.router
            .unit(id)
            .is_ok_and(|unit| unit.open.load(Ordering::Acquire))
    }

    /// Reject unusable replacement inputs before the transaction closes any admission. This checks
    /// metadata, complete restart closure, recreation support and installed paths without loading code.
    pub fn validate_replacement(
        &self,
        candidate: &Composition,
        affected: &[String],
    ) -> Result<(), Fault> {
        let graph = self.candidate_graph(candidate)?;
        let required = self.affected_instances(candidate)?;
        let selected: BTreeSet<_> = affected.iter().cloned().collect();
        if selected.len() != affected.len()
            || selected.iter().any(|id| !graph.instances.contains_key(id))
            || required.iter().any(|id| !selected.contains(id))
            || graph.dependencies.iter().any(|(id, dependencies)| {
                !selected.contains(id)
                    && dependencies
                        .iter()
                        .any(|dependency| selected.contains(dependency))
            })
        {
            return Err(Fault::new(
                "InvalidInput",
                "replacement",
                "restart set is not the complete dependency closure",
            ));
        }
        for id in affected {
            if self
                .router
                .unit(id)
                .is_ok_and(|unit| matches!(unit.instance.as_ref(), Instance::Local(_)))
            {
                return Err(Fault::new(
                    "UnsupportedChange",
                    "replacement",
                    "embedded contribution has no recreation factory",
                ));
            }
            let spec = &graph.instances[id];
            let manifest = candidate
                .packages
                .iter()
                .find(|manifest| manifest.descriptor.package == spec.package)
                .ok_or_else(|| Fault::new("InvalidInput", "replacement", "missing package"))?;
            std::fs::canonicalize(self.base.join(&manifest.library)).map_err(|error| {
                Fault::new("MissingDependency", "replacement", error.to_string())
            })?;
        }
        Ok(())
    }

    /// Stop the supplied dependency closure and recreate only its native publications. Metadata
    /// and native paths are checked before shutdown. `CleanupFailure` forbids recovery startup;
    /// `InitializationFailure` means candidate cleanup completed and one restoration may be tried.
    /// The caller owns persistence, recovery policy, and the foreground reconfiguration gate.
    pub async fn replace_instances(
        &self,
        candidate: Composition,
        affected: &[String],
    ) -> Result<(), Fault> {
        let kernel = self.clone();
        let affected = affected.to_vec();
        tokio::spawn(async move {
            let _replace = kernel.replacement.lock().await;
            kernel.validate_replacement(&candidate, &affected)?;
            kernel
                .replace_owned(candidate, &affected, BTreeMap::new())
                .await
        })
        .await
        .map_err(|error| Fault::new("CleanupFailure", "replacement", error.to_string()))?
    }

    async fn replace_owned(
        &self,
        candidate: Composition,
        affected: &[String],
        mut local: BTreeMap<String, eden_plugin_sdk::Package>,
    ) -> Result<(), Fault> {
        if !self.router.open.load(Ordering::Acquire) {
            return Err(Fault::new(
                "Unavailable",
                "replacement",
                "session is closed",
            ));
        }
        let graph = self.candidate_graph(&candidate)?;
        let required = self.affected_instances(&candidate)?;
        let selected: BTreeSet<_> = affected.iter().cloned().collect();
        if selected.len() != affected.len()
            || selected.iter().any(|id| !graph.instances.contains_key(id))
            || required.iter().any(|id| !selected.contains(id))
            || graph.dependencies.iter().any(|(id, dependencies)| {
                !selected.contains(id)
                    && dependencies
                        .iter()
                        .any(|dependency| selected.contains(dependency))
            })
        {
            return Err(Fault::new(
                "InvalidInput",
                "replacement",
                "restart set is not the complete dependency closure",
            ));
        }
        let order: Vec<_> = graph
            .order
            .iter()
            .filter(|id| selected.contains(*id))
            .cloned()
            .collect();
        let mut manifests = BTreeMap::new();
        for id in &order {
            let spec = &graph.instances[id];
            if self
                .router
                .unit(id)
                .is_ok_and(|unit| matches!(unit.instance.as_ref(), Instance::Local(_)))
                && !local.contains_key(&spec.package)
            {
                return Err(Fault::new(
                    "UnsupportedChange",
                    "replacement",
                    "embedded contribution has no recreation factory",
                ));
            }
            let manifest = candidate
                .packages
                .iter()
                .find(|manifest| manifest.descriptor.package == spec.package)
                .ok_or_else(|| Fault::new("InvalidInput", "replacement", "missing package"))?;
            let path = if local.contains_key(&spec.package) {
                std::path::PathBuf::new()
            } else {
                std::fs::canonicalize(self.base.join(&manifest.library)).map_err(|error| {
                    Fault::new("MissingDependency", "replacement", error.to_string())
                })?
            };
            manifests.insert(spec.package.clone(), (manifest.clone(), path));
        }
        // Do not close providers before consumers' finalizers have used them.
        for id in order.iter().rev() {
            if self.router.unit(id).is_ok() {
                stop_unit(self.router.clone(), id.clone())
                    .await
                    .map_err(|error| {
                        Fault::new("CleanupFailure", "replacement", error.to_string())
                    })?;
            }
        }
        {
            let mut units = self
                .router
                .instances
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for id in &order {
                units.remove(id);
            }
        }
        if let Err(error) = self
            .initialize(&manifests, &mut local, &graph, &order)
            .await
        {
            let mut cleanup_failure = None;
            for id in order.iter().rev() {
                if self.router.unit(id).is_ok()
                    && let Err(cleanup) = stop_unit(self.router.clone(), id.clone()).await
                {
                    cleanup_failure = Some(cleanup);
                }
            }
            if let Some(cleanup) = cleanup_failure {
                return Err(Fault::new(
                    "CleanupFailure",
                    "replacement",
                    format!("{error}; candidate cleanup: {cleanup}"),
                ));
            }
            let mut units = self
                .router
                .instances
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for id in &order {
                units.remove(id);
            }
            let code = if error.code == "CleanupFailure" {
                "CleanupFailure"
            } else {
                "InitializationFailure"
            };
            return Err(Fault::new(code, "replacement", error.to_string()));
        }
        *self.composition.lock().unwrap_or_else(|e| e.into_inner()) = candidate;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eden_plugin_sdk::Package;
    use serde_json::json;

    fn package(
        name: &str,
        events: Arc<Mutex<Vec<String>>>,
        fail_stop: bool,
        fail_ready: bool,
    ) -> Package {
        let stopped = name.to_owned();
        Package::new(name)
            .service(name, |value: serde_json::Value, _| async move { Ok(value) })
            .service(
                p::runtime::READY,
                move |_: serde_json::Value, _| async move {
                    if fail_ready {
                        Err(Fault::new("TestFailure", "ready", "prepare rejected"))
                    } else {
                        Ok(())
                    }
                },
            )
            .service(p::INSTANCE_STOP, move |_: serde_json::Value, _| {
                let events = events.clone();
                let stopped = stopped.clone();
                async move {
                    events.lock().unwrap().push(stopped);
                    if fail_stop {
                        Err(Fault::new("TestFailure", "stop", "cleanup rejected"))
                    } else {
                        Ok(())
                    }
                }
            })
    }

    async fn mount(events: Arc<Mutex<Vec<String>>>, fail_stop: bool) -> Kernel {
        let mut tail = Package::new("tail");
        for role in [p::AGENT_LOOP, p::CONTEXT, p::PROVIDER, p::TOOL] {
            tail = tail.service(role, |value: serde_json::Value, _| async move { Ok(value) });
        }
        let packages = vec![
            tail,
            package("provider", events.clone(), fail_stop, false),
            package("caller", events.clone(), false, false),
            package("child", events, false, false),
        ];
        let manifests: Vec<_> = packages
            .iter()
            .map(|package| {
                json!({
                    "descriptor": package.descriptor(),
                    "host": p::CONTRACT,
                    "sdk": p::CONTRACT,
                    "target": eden_plugin_sdk::abi::TARGET,
                    "library": "embedded",
                    "config": {},
                    "requires": if package.descriptor().package == "caller" {
                            vec!["provider"]
                        } else {
                            vec![]
                        },
                })
            })
            .collect();
        let roles: BTreeMap<_, _> = [p::AGENT_LOOP, p::CONTEXT, p::PROVIDER, p::TOOL]
            .into_iter()
            .map(|role| (role, "tail"))
            .collect();
        let composition = serde_json::from_value(json!({
            "packages": manifests,
            "roles": roles,
            "runtime": {
                "instances": [{ "id": "child", "package": "child", "owner": "caller" }],
                "scopes": { "": { "bindings": { "provider": { "tail": "provider" } } } },
            },
        }))
        .unwrap();
        Kernel::load_embedded(
            composition,
            Path::new("."),
            1,
            Events::new(1),
            packages
                .into_iter()
                .map(|package| (package.descriptor().package.clone(), package))
                .collect(),
        )
        .await
        .unwrap()
    }

    fn candidate(kernel: &Kernel) -> Composition {
        let mut candidate = kernel.composition();
        candidate
            .packages
            .iter_mut()
            .find(|manifest| manifest.descriptor.package == "provider")
            .unwrap()
            .config = json!({ "new": true });
        candidate
    }

    fn replacements(
        events: Arc<Mutex<Vec<String>>>,
        fail_ready: bool,
    ) -> BTreeMap<String, Package> {
        ["provider", "caller", "child"]
            .into_iter()
            .map(|name| {
                (
                    name.into(),
                    package(name, events.clone(), false, fail_ready && name == "caller"),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn replacement_stops_consumers_first_and_preserves_unrelated_generation() {
        let events = Arc::new(Mutex::new(vec![]));
        let kernel = mount(events.clone(), false).await;
        let tail = kernel.instance_identity("tail").unwrap();
        let provider = kernel.instance_identity("provider").unwrap();
        let old_handle = kernel.role("provider").unwrap();
        let old_action = kernel.instance_service("provider", "provider").unwrap();
        let candidate = candidate(&kernel);
        let affected = kernel.affected_instances(&candidate).unwrap();
        assert_eq!(affected, ["provider", "caller", "child"]);
        kernel
            .replace_owned(candidate, &affected, replacements(events.clone(), false))
            .await
            .unwrap();
        assert_eq!(*events.lock().unwrap(), ["child", "caller", "provider"]);
        assert_eq!(tail, kernel.instance_identity("tail").unwrap());
        assert_ne!(provider, kernel.instance_identity("provider").unwrap());
        let request = Request {
            execution: None,
            session_id: 1,
            run_id: 1,
            contract: "provider".into(),
            payload: json!("ok"),
        };
        assert_eq!(
            old_handle
                .call(request.clone(), Cancellation::default())
                .await
                .into_result()
                .unwrap_err()
                .code,
            "Unavailable"
        );
        assert_eq!(
            old_action
                .call(request.clone(), Cancellation::default())
                .await
                .into_result()
                .unwrap_err()
                .code,
            "Unavailable"
        );
        assert_eq!(
            kernel
                .invoke(request, Cancellation::default())
                .await
                .into_result()
                .unwrap(),
            json!("ok")
        );
        kernel.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_cleanup_prevents_replacement_and_remains_a_recovery_barrier() {
        let events = Arc::new(Mutex::new(vec![]));
        let kernel = mount(events.clone(), true).await;
        let provider = kernel.instance_identity("provider").unwrap();
        let candidate = candidate(&kernel);
        let affected = kernel.affected_instances(&candidate).unwrap();
        for _ in 0..2 {
            let error = kernel
                .replace_owned(
                    candidate.clone(),
                    &affected,
                    replacements(events.clone(), false),
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, "CleanupFailure");
            assert_eq!(provider, kernel.instance_identity("provider").unwrap());
        }
        assert_eq!(*events.lock().unwrap(), ["child", "caller", "provider"]);
        assert!(kernel.shutdown().await.is_err());
    }

    #[tokio::test]
    async fn failed_activation_cleans_candidate_before_explicit_restore() {
        let events = Arc::new(Mutex::new(vec![]));
        let kernel = mount(events.clone(), false).await;
        let original = kernel.composition();
        let tail = kernel.instance_identity("tail").unwrap();
        let candidate = candidate(&kernel);
        let affected = kernel.affected_instances(&candidate).unwrap();
        let error = kernel
            .replace_owned(candidate, &affected, replacements(events.clone(), true))
            .await
            .unwrap_err();
        assert_eq!(error.code, "InitializationFailure");
        assert_eq!(
            *events.lock().unwrap(),
            ["child", "caller", "provider", "child", "caller", "provider"]
        );
        assert!(kernel.instance_identity("provider").is_err());
        kernel
            .replace_owned(original, &affected, replacements(events, false))
            .await
            .unwrap();
        assert_eq!(tail, kernel.instance_identity("tail").unwrap());
        kernel.shutdown().await.unwrap();
    }
}
