use eden_kernel::preflight;
use eden_plugin_sdk::abi::TARGET;
use eden_protocol::{
    AGENT_LOOP, CONTEXT, CONTRACT, Composition, Descriptor, PROVIDER, PackageManifest, TOOL,
};
fn manifest() -> Composition {
    let roles = [AGENT_LOOP, CONTEXT, PROVIDER, TOOL];
    Composition {
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
