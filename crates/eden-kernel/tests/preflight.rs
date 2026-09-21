//! Tests are outside the doc comment standard; see docs/development-checks.md#doc-comments.
#![allow(missing_docs)]

use eden_kernel::preflight;
use eden_plugin_sdk::abi::TARGET;
use eden_protocol::{
    AGENT_LOOP, CONTEXT, CONTRACT, Composition, Descriptor, PROVIDER, PackageManifest, TOOL,
};
fn manifest() -> Composition {
    let roles = [AGENT_LOOP, CONTEXT, PROVIDER, TOOL];
    Composition {
        resource_packages: vec![],
        packages: vec![PackageManifest {
            descriptor: Descriptor {
                package: "standard".into(),
                version: "0.1.0".into(),
                provides: roles.iter().map(|s| (*s).into()).collect(),
            },
            host: CONTRACT.into(),
            sdk: CONTRACT.into(),
            target: TARGET.into(),
            library: "missing-library".into(),
            config: serde_json::Value::Null,
            requires: vec![],
        }],
        roles: roles
            .iter()
            .map(|s| ((*s).into(), "standard".into()))
            .collect(),
    }
}
#[test]
fn incompatible_package_is_rejected_before_library_loading() {
    let mut composition = manifest();
    composition.packages[0].sdk = "old-sdk".into();
    assert_eq!(
        preflight(&composition).unwrap_err().code,
        "IncompatibleContract"
    );
}

#[test]
fn unknown_domain_requirements_are_checked_without_host_domain_types() {
    let mut composition = manifest();
    composition.packages[0].requires = vec!["example.weather.v2".into()];
    assert_eq!(
        preflight(&composition).unwrap_err().code,
        "MissingDependency"
    );
    composition.packages[0]
        .descriptor
        .provides
        .push("example.weather.v1".into());
    composition
        .roles
        .insert("example.weather.v1".into(), "standard".into());
    assert_eq!(
        preflight(&composition).unwrap_err().code,
        "MissingDependency"
    );
    composition.packages[0]
        .descriptor
        .provides
        .push("example.weather.v2".into());
    composition
        .roles
        .insert("example.weather.v2".into(), "standard".into());
    preflight(&composition).unwrap();
}
