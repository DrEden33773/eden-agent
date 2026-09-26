//! Validated immutable routing graph; instance lifecycle is owned separately.
use eden_protocol::{
    self as p, Composition, Fault,
    runtime::{Binding, InstanceSpec, ServiceScope},
};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct Graph {
    pub instances: BTreeMap<String, InstanceSpec>,
    pub scopes: BTreeMap<String, ServiceScope>,
    pub order: Vec<String>,
    pub dependencies: BTreeMap<String, BTreeSet<String>>,
}
fn invalid(message: impl Into<String>) -> Fault {
    Fault::new("InvalidInput", "runtime-graph", message)
}
impl Graph {
    pub fn build(composition: &Composition) -> Result<Self, Fault> {
        let packages: BTreeMap<_, _> = composition
            .packages
            .iter()
            .map(|m| (m.descriptor.package.as_str(), m))
            .collect();
        let mut instances = BTreeMap::new();
        for spec in &composition.runtime.instances {
            if spec.id.is_empty()
                || !packages.contains_key(spec.package.as_str())
                || instances.insert(spec.id.clone(), spec.clone()).is_some()
            {
                return Err(invalid("duplicate/empty instance id or missing package"));
            }
        }
        for package in &composition.packages {
            let name = &package.descriptor.package;
            if !composition
                .runtime
                .instances
                .iter()
                .any(|i| &i.package == name)
                && instances
                    .insert(
                        name.clone(),
                        InstanceSpec {
                            id: name.clone(),
                            package: name.clone(),
                            scope: String::new(),
                            owner: None,
                            dependencies: vec![],
                            config: None,
                        },
                    )
                    .is_some()
            {
                return Err(invalid(
                    "implicit package instance conflicts with explicit instance",
                ));
            }
        }
        let mut scopes = composition.runtime.scopes.clone();
        let root = scopes.entry(String::new()).or_default();
        if root.parent.is_some() {
            return Err(invalid("session scope cannot have a parent"));
        }
        for (role, target) in &composition.roles {
            root.bindings
                .entry(role.clone())
                .or_insert_with(|| Binding {
                    tail: target.clone(),
                    wrappers: vec![],
                });
        }
        for (id, scope) in &scopes {
            if !id.is_empty() && scope.parent.is_none() {
                return Err(invalid("child scope needs a parent"));
            }
            let mut seen = BTreeSet::new();
            let mut cursor = Some(id.as_str());
            while let Some(id) = cursor {
                if !seen.insert(id) {
                    return Err(invalid("scope cycle"));
                }
                cursor = scopes
                    .get(id)
                    .ok_or_else(|| invalid("missing parent scope"))?
                    .parent
                    .as_deref();
            }
            for (role, binding) in &scope.bindings {
                if role.is_empty()
                    || matches!(
                        role.as_str(),
                        p::INSTANCE_STOP | p::runtime::HOST | p::runtime::READY
                    )
                {
                    return Err(invalid("reserved/empty service binding"));
                }
                let mut seen = BTreeSet::new();
                for id in binding
                    .wrappers
                    .iter()
                    .chain(std::iter::once(&binding.tail))
                {
                    if !seen.insert(id) {
                        return Err(invalid("duplicate wrapper/tail instance"));
                    }
                    let instance = instances
                        .get(id)
                        .ok_or_else(|| invalid(format!("missing instance {id}")))?;
                    if !packages[instance.package.as_str()]
                        .descriptor
                        .provides
                        .contains(role)
                    {
                        return Err(invalid(format!("{id} does not provide {role}")));
                    }
                }
            }
        }
        let mut graph = Self {
            instances,
            scopes,
            order: vec![],
            dependencies: BTreeMap::new(),
        };
        for spec in graph.instances.values() {
            if !graph.scopes.contains_key(&spec.scope) {
                return Err(invalid("missing instance scope"));
            }
            for role in &packages[spec.package.as_str()].requires {
                graph.binding(&spec.scope, role)?;
            }
        }
        for (id, spec) in &graph.instances {
            let mut dependencies: BTreeSet<_> = spec
                .dependencies
                .iter()
                .chain(spec.owner.iter())
                .cloned()
                .collect();
            for role in &packages[spec.package.as_str()].requires {
                let binding = graph.binding(&spec.scope, role)?;
                dependencies.extend(
                    binding
                        .wrappers
                        .iter()
                        .chain(std::iter::once(&binding.tail))
                        .filter(|target| *target != id)
                        .cloned(),
                );
            }
            graph.dependencies.insert(id.clone(), dependencies);
        }
        // A retained wrapper continuation depends on every downstream publication in its chain.
        for scope in graph.scopes.values() {
            for binding in scope.bindings.values() {
                let chain: Vec<_> = binding
                    .wrappers
                    .iter()
                    .chain(std::iter::once(&binding.tail))
                    .collect();
                for pair in chain.windows(2) {
                    graph
                        .dependencies
                        .entry(pair[0].clone())
                        .or_default()
                        .insert(pair[1].clone());
                }
            }
        }
        let mut visiting = BTreeSet::new();
        let mut done = BTreeSet::new();
        let mut order = vec![];
        done.clear();
        for id in graph.instances.keys() {
            graph.visit(id, &mut visiting, &mut done, &mut order)?;
        }
        graph.order = order;
        Ok(graph)
    }
    fn visit(
        &self,
        id: &str,
        visiting: &mut BTreeSet<String>,
        done: &mut BTreeSet<String>,
        order: &mut Vec<String>,
    ) -> Result<(), Fault> {
        if done.contains(id) {
            return Ok(());
        }
        if !visiting.insert(id.into()) {
            return Err(invalid("instance ownership/dependency cycle"));
        }
        let dependencies = self
            .dependencies
            .get(id)
            .ok_or_else(|| invalid(format!("missing owner/dependency {id}")))?;
        for dependency in dependencies {
            self.visit(dependency, visiting, done, order)?;
        }
        visiting.remove(id);
        done.insert(id.into());
        order.push(id.into());
        Ok(())
    }
    pub fn binding(&self, scope: &str, contract: &str) -> Result<&Binding, Fault> {
        let mut cursor = Some(scope);
        while let Some(id) = cursor {
            let scope = self
                .scopes
                .get(id)
                .ok_or_else(|| invalid("unknown scope"))?;
            if let Some(binding) = scope.bindings.get(contract) {
                return Ok(binding);
            }
            cursor = scope.parent.as_deref();
        }
        Err(Fault::new("MissingDependency", "router", contract))
    }
    pub fn descendant(&self, child: &str, parent: &str) -> bool {
        let mut cursor = Some(child);
        while let Some(id) = cursor {
            if id == parent {
                return true;
            }
            cursor = self.scopes.get(id).and_then(|s| s.parent.as_deref());
        }
        false
    }
}
