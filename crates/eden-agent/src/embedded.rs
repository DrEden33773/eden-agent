//! Explicit in-process composition; packages retain caller-runtime Rust values while native calls
//! continue to cross the versioned byte bridge.
use super::*;
use eden_plugin_sdk::Package;
use eden_protocol::{Composition, PackageManifest};
use std::path::PathBuf;

/// An explicit composition with caller-owned role implementations. Reopen requires the same
/// contribution identities; closures themselves are never persisted.
pub struct Embedded {
    pub(crate) composition: Composition,
    pub(crate) base: PathBuf,
    pub(crate) packages: BTreeMap<String, Package>,
}
impl Embedded {
    /// Library and resource paths resolve from this explicit base, independently of session cwd.
    pub fn new(composition: Composition, base: PathBuf) -> Self {
        Self {
            composition,
            base,
            packages: BTreeMap::new(),
        }
    }
    /// Add a package and select its public roles. Identity must change whenever a new
    /// implementation is incompatible with saved state. Duplicate package identities are refused.
    pub fn package(mut self, package: Package, identity: &str) -> Result<Self, Fault> {
        let descriptor = package.descriptor().clone();
        if identity.is_empty()
            || self
                .composition
                .packages
                .iter()
                .any(|p| p.descriptor.package == descriptor.package)
        {
            return Err(Fault::new(
                "InvalidInput",
                "embedded",
                "empty identity or duplicate package",
            ));
        }
        for role in &descriptor.provides {
            if role != eden_protocol::INSTANCE_STOP {
                self.composition
                    .roles
                    .insert(role.clone(), descriptor.package.clone());
            }
        }
        self.composition.packages.push(PackageManifest {
            descriptor: descriptor.clone(),
            host: eden_protocol::CONTRACT.into(),
            sdk: eden_protocol::CONTRACT.into(),
            target: eden_plugin_sdk::abi::TARGET.into(),
            library: format!("embedded:{identity}"),
            config: serde_json::Value::Null,
            requires: vec![],
        });
        self.packages.insert(descriptor.package, package);
        Ok(self)
    }
    /// Open without writing a temporary composition file. Dropping this future cannot orphan
    /// initialized packages; the same session delivery guard owns rollback.
    pub async fn open(
        self,
        options: SessionOptions,
        workspace: WorkspaceOptions,
    ) -> Result<Session, Fault> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let base = self.base.clone();
        tokio::spawn(async move {
            let result = Session::open_owned(
                base.join("embedded.json"),
                options,
                workspace,
                false,
                Some(self),
            )
            .await;
            let _ = sender.send(SessionDelivery(Some(result)));
        });
        receiver
            .await
            .map_err(|e| Fault::new("Unavailable", "embedded", e.to_string()))?
            .0
            .take()
            .ok_or_else(|| Fault::new("Unavailable", "embedded", "missing delivery"))?
    }
}

/// Model-visible guidance and read-only eligibility. `ToolDefinition.execution` controls the
/// default loop's sequential barriers and parallel batches; children remain scope-owned.
#[derive(Clone, Default)]
pub struct ToolOptions {
    /// Model-visible usage direction included in the advertised description.
    pub system_prompt: String,
    /// Model-visible constraints included alongside the tool's description.
    pub guidelines: Vec<String>,
    /// Whether this contribution remains available under read-only selection.
    pub read_only: bool,
}
impl Embedded {
    /// Register a schema-checked tool through the selected catalog/execution registry. The
    /// preprocessing hook runs before validation; execution receives only the validated value.
    /// Explicit workspace tool selection still has to include this tool's name.
    pub fn tool<P, F, Fut>(
        self,
        mut definition: c::ToolDefinition,
        identity: &str,
        options: ToolOptions,
        preprocess: P,
        execute: F,
    ) -> Result<Self, Fault>
    where
        P: Fn(serde_json::Value) -> Result<serde_json::Value, Fault> + Send + Sync + 'static,
        F: Fn(c::ToolRequest, eden_plugin_sdk::CallContext) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<c::ToolResult, Fault>> + Send + 'static,
    {
        use eden_protocol::resources as r;
        let validator = jsonschema::validator_for(&definition.parameters)
            .map_err(|e| Fault::new("InvalidInput", "tool-schema", e.to_string()))?;
        if !options.system_prompt.is_empty() {
            definition
                .description
                .push_str(&format!("\n{}", options.system_prompt));
        }
        for guideline in options.guidelines {
            definition.description.push_str(&format!("\n{guideline}"));
        }
        let name = definition.name.clone();
        if name.is_empty() {
            return Err(Fault::new("InvalidInput", "embedded", "tool name is empty"));
        }
        let catalog = format!("eden.embedded-tool.{name}.catalog.v1");
        let route = format!("eden.embedded-tool.{name}.execute.v1");
        let tool_name = name.clone();
        let execute = Arc::new(execute);
        let preprocess = Arc::new(preprocess);
        let validator = Arc::new(validator);
        let package = Package::new(&format!("embedded-tool-{name}"))
            .service(&catalog, move |_: r::CatalogRequest, _| {
                let definition = definition.clone();
                async move {
                    Ok(r::Catalog {
                        tools: vec![definition],
                    })
                }
            })
            .service(&route, move |mut request: c::ToolRequest, cx| {
                let execute = execute.clone();
                let preprocess = preprocess.clone();
                let validator = validator.clone();
                let tool_name = tool_name.clone();
                async move {
                    if request.name != tool_name {
                        return Err(Fault::new("UnknownTool", "embedded", "foreign tool name"));
                    }
                    request.arguments = preprocess(request.arguments)?;
                    validator
                        .validate(&request.arguments)
                        .map_err(|e| Fault::new("InvalidInput", "tool-arguments", e.to_string()))?;
                    execute(request, cx).await
                }
            });
        let mut result = self.package(package, identity)?;
        let selected = result
            .composition
            .roles
            .get(c::TOOL)
            .ok_or_else(|| {
                Fault::new(
                    "MissingDependency",
                    "embedded",
                    "tool registry is not selected",
                )
            })?
            .clone();
        let registry = result
            .composition
            .packages
            .iter_mut()
            .find(|p| p.descriptor.package == selected)
            .ok_or_else(|| {
                Fault::new(
                    "MissingDependency",
                    "embedded",
                    "tool registry package is missing",
                )
            })?;
        if !registry.config.is_object() {
            registry.config = serde_json::json!({});
        }
        let contributions = registry
            .config
            .as_object_mut()
            .ok_or_else(|| {
                Fault::new(
                    "InvalidInput",
                    "embedded",
                    "registry config is not an object",
                )
            })?
            .entry("contributions")
            .or_insert_with(|| serde_json::json!([]));
        contributions
            .as_array_mut()
            .ok_or_else(|| {
                Fault::new("InvalidInput", "embedded", "contributions must be an array")
            })?
            .push(serde_json::json!({
                "catalog": catalog,
                "execute": route,
                "read_only": options.read_only,
            }));
        let tools = registry
            .config
            .as_object_mut()
            .ok_or_else(|| {
                Fault::new(
                    "InvalidInput",
                    "embedded",
                    "registry config is not an object",
                )
            })?
            .entry("tools")
            .or_insert_with(|| serde_json::json!(["read", "write", "edit", "bash"]));
        tools
            .as_array_mut()
            .ok_or_else(|| Fault::new("InvalidInput", "embedded", "tools must be an array"))?
            .push(serde_json::json!(name));
        Ok(result)
    }
    /// Register an in-memory resource source. Reload publishes a complete replacement only
    /// after the provider succeeds; failures leave the old snapshot available. Expansion replies
    /// must carry the frozen revision. Relative metadata paths resolve from `base` without reads.
    pub fn resources<F, Fut>(
        self,
        identity: &str,
        base: PathBuf,
        provider: F,
    ) -> Result<Self, Fault>
    where
        F: Fn(eden_protocol::resources::ResourceRequest, eden_plugin_sdk::CallContext) -> Fut
            + Send
            + Sync
            + 'static,
        Fut: std::future::Future<Output = Result<eden_protocol::resources::ResourceReply, Fault>>
            + Send
            + 'static,
    {
        use eden_protocol::resources as r;
        if !base.is_absolute() {
            return Err(Fault::new(
                "InvalidInput",
                "embedded-resources",
                "resource base must be absolute",
            ));
        }
        let provider = Arc::new(provider);
        let current = Arc::new(tokio::sync::Mutex::new(None::<r::Snapshot>));
        let package = Package::new("embedded-resources").service(
            r::SOURCE,
            move |request: r::ResourceRequest, cx| {
                let provider = provider.clone();
                let current = current.clone();
                let base = base.clone();
                async move {
                    let mut current = current.lock().await;
                    if current.is_none() || matches!(request, r::ResourceRequest::Reload) {
                        let mut reply = provider(r::ResourceRequest::Reload, cx.clone()).await?;
                        reply.snapshot.revision = current.as_ref().map_or(1, |s| s.revision + 1);
                        for resource in reply
                            .snapshot
                            .skills
                            .iter_mut()
                            .chain(reply.snapshot.templates.iter_mut())
                        {
                            resource.path =
                                base.join(&resource.path).to_string_lossy().into_owned();
                        }
                        for source in &mut reply.snapshot.sources {
                            *source = base.join(&*source).to_string_lossy().into_owned();
                        }
                        *current = Some(reply.snapshot);
                    }
                    let snapshot = current.clone().ok_or_else(|| {
                        Fault::new("Unavailable", "embedded-resources", "no snapshot")
                    })?;
                    let text = if matches!(
                        request,
                        r::ResourceRequest::Snapshot | r::ResourceRequest::Reload
                    ) {
                        None
                    } else {
                        let reply = provider(request, cx).await?;
                        if reply.snapshot.revision != snapshot.revision {
                            return Err(Fault::new(
                                "InvalidInput",
                                "embedded-resources",
                                "expansion changed the frozen resource revision",
                            ));
                        }
                        reply.text
                    };
                    Ok(r::ResourceReply { snapshot, text })
                }
            },
        );
        self.package(package, identity)
    }
}
