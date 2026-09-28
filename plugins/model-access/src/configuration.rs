//! Configuration metadata protects credential subtrees while retaining restart semantics.
use eden_plugin_sdk::Package;
use eden_protocol::{Fault, configuration as c};
use serde_json::{Value, json};
use std::collections::BTreeSet;

fn pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn description(config: &Value) -> c::Description {
    let configured = config
        .pointer("/credentials/providers")
        .and_then(Value::as_object);
    let mut providers = BTreeSet::from(["openai".to_owned(), "deepseek".to_owned()]);
    if let Some(configured) = configured {
        providers.extend(configured.keys().cloned());
    }
    let mut secret_paths = vec!["/api_key".into(), "/credentials/oauth".into()];
    let mut properties = serde_json::Map::new();
    for provider in providers {
        let base = format!("/credentials/providers/{}", pointer(&provider));
        let mut private = BTreeSet::from([
            "literal".to_owned(),
            "headers".to_owned(),
            "command".to_owned(),
            "cloud".to_owned(),
        ]);
        if let Some(fields) = configured
            .and_then(|all| all.get(&provider))
            .and_then(Value::as_object)
        {
            private.extend(fields.keys().filter(|key| key.as_str() != "env").cloned());
        }
        secret_paths.extend(
            private
                .into_iter()
                .map(|key| format!("{base}/{}", pointer(&key))),
        );
        properties.insert(
            provider,
            json!({
                "type": "object",
                "properties": {
                    "literal": { "type": "string", "title": "Provider API key", "minLength": 1 },
                    "env": {
                        "type": "string",
                        "title": "API key environment variable",
                        "minLength": 1,
                    },
                },
            }),
        );
    }
    if let Some(credentials) = config.get("credentials").and_then(Value::as_object) {
        for key in credentials
            .keys()
            .filter(|key| !["providers", "path", "oauth"].contains(&key.as_str()))
        {
            secret_paths.push(format!("/credentials/{}", pointer(key)));
        }
    }
    c::Description {
        profile: Some(1),
        schema: Some(json!({
            "type": "object",
            "properties": {
                "model": { "type": "string", "title": "Legacy Responses model", "minLength": 1 },
                "endpoint": {
                    "type": "string",
                    "title": "Legacy Responses endpoint",
                    "minLength": 1,
                },
                "api_key_env": {
                    "type": "string",
                    "title": "Legacy API key environment variable",
                    "minLength": 1,
                },
                "api_key": {
                    "type": "string",
                    "title": "Legacy Responses API key",
                    "minLength": 1,
                },
                "catalog": {
                    "type": "object",
                    "properties": {
                        "source": { "type": "string" },
                        "offline": { "type": "boolean", "default": false },
                        "auto_refresh": { "type": "boolean", "default": true },
                    },
                },
                "credentials": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "providers": { "type": "object", "properties": properties },
                    },
                },
            },
        })),
        defaults: json!({}),
        description: Some(
            "Changes restart this model-access instance. Provider API keys are used by \
             catalog-selected models; the legacy Responses key takes precedence over its \
             environment variable. Other credential subtrees remain private."
                .into(),
        ),
        secret_paths,
        ..Default::default()
    }
}

fn validation(config: &Value) -> Result<c::Validation, Fault> {
    let mut result = c::validate(&description(config), config)?;
    let mut invalid = |path: &str| {
        result.errors.push(c::FieldError {
            path: path.into(),
            code: "type".into(),
            message: "Invalid model-access configuration type".into(),
        })
    };
    if serde_json::from_value::<crate::Config>(config.clone()).is_err() {
        invalid("");
    }
    if let Some(oauth) = config.pointer("/credentials/oauth")
        && serde_json::from_value::<std::collections::BTreeMap<String, crate::oauth::Config>>(
            oauth.clone(),
        )
        .is_err()
    {
        invalid("/credentials/oauth");
    }
    if let Some(providers) = config
        .pointer("/credentials/providers")
        .and_then(Value::as_object)
    {
        for (provider, fields) in providers {
            let base = format!("/credentials/providers/{}", pointer(provider));
            if let Some(command) = fields.get("command")
                && !command.is_null()
                && !command.is_string()
            {
                invalid(&format!("{base}/command"));
            }
            if let Some(headers) = fields.get("headers")
                && serde_json::from_value::<std::collections::BTreeMap<String, String>>(
                    headers.clone(),
                )
                .is_err()
            {
                invalid(&format!("{base}/headers"));
            }
            if let Some(cloud) = fields.get("cloud")
                && !cloud.is_null()
                && !cloud.is_object()
            {
                invalid(&format!("{base}/cloud"));
            }
        }
    }
    Ok(result)
}

pub(crate) fn register(package: Package, config: &Value) -> Package {
    let description = description(config);
    package.service(c::CONFIGURATION, move |request: c::PluginRequest, _| {
        let description = description.clone();
        async move {
            match request {
                c::PluginRequest::Describe => Ok(json!(description)),
                c::PluginRequest::Validate { config } => Ok(json!(validation(&config)?)),
                c::PluginRequest::Update { .. } => Err(Fault::new(
                    "Unsupported",
                    "model-access",
                    "model-access configuration requires instance restart",
                )),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_subtree_string_replacements_fail_without_echoing_values() {
        for path in ["oauth", "providers"] {
            let value = json!({ "credentials": { path: "PRIVATE-invalid" } });
            let result = validation(&value).unwrap();
            assert!(!result.errors.is_empty());
            assert!(!serde_json::to_string(&result).unwrap().contains("PRIVATE"));
        }
        for field in ["headers", "cloud"] {
            let value = json!({
                "credentials": { "providers": { "openai": { field: "PRIVATE-invalid" } } },
            });
            assert!(!validation(&value).unwrap().errors.is_empty());
        }
        assert!(
            !validation(&json!({ "max_output_tokens": "PRIVATE-invalid" }))
                .unwrap()
                .errors
                .is_empty()
        );
    }

    #[test]
    fn direct_key_precedes_environment_and_clear_restores_fallback() {
        let config: crate::Config = serde_json::from_value(json!({
            "model": "test",
            "api_key": "private-direct",
            "api_key_env": "CUSTOM_KEY",
        }))
        .unwrap();
        let env = |key: &str| (key == "CUSTOM_KEY").then(|| "private-env".to_owned());
        assert_eq!(config.settings_with(env).unwrap().key, "private-direct");
        let config = crate::Config {
            api_key: None,
            ..config
        };
        assert_eq!(config.settings_with(env).unwrap().key, "private-env");
    }

    #[test]
    fn descriptor_and_default_form_hide_all_credential_material() {
        let config = json!({
            "model": "test",
            "api_key": "PRIVATE-direct",
            "unknown": { "keep": true },
            "credentials": {
                "oauth": { "openai": { "client_secret": "PRIVATE-oauth" } },
                "future": "PRIVATE-future",
                "providers": {
                    "openai": {
                        "literal": "PRIVATE-key",
                        "headers": { "Authorization": "PRIVATE-header" },
                        "command": "PRIVATE-command",
                        "cloud": { "token": "PRIVATE-cloud" },
                    },
                    "custom/a~b": { "literal": "PRIVATE-custom", "extension": "PRIVATE-extension" },
                },
            },
        });
        let description = description(&config);
        assert!(
            c::validate(&description, &config)
                .unwrap()
                .errors
                .is_empty()
        );
        assert!(description.live_paths.is_empty());
        assert!(
            description
                .secret_paths
                .contains(&"/credentials/providers/custom~1a~0b/literal".into())
        );
        assert!(
            description
                .secret_paths
                .contains(&"/credentials/providers/deepseek/literal".into())
        );
        let effective = c::redact(&config, &description.secret_paths);
        assert_eq!(effective["unknown"], config["unknown"]);
        assert!(
            !serde_json::to_string(&effective)
                .unwrap()
                .contains("PRIVATE")
        );
        assert!(
            !serde_json::to_string(&description)
                .unwrap()
                .contains("PRIVATE")
        );
        let model = eden_plugin_sdk::configuration_form::Model {
            binding: eden_protocol::configuration_form::Binding {
                instance: "model-access".into(),
                generation: Some(1),
                revision: 0,
                profile: 1,
            },
            description,
            effective,
            sources: Default::default(),
            secrets_configured: Default::default(),
        };
        let view = eden_plugin_sdk::configuration_form::default_form(&model);
        assert!(!serde_json::to_string(&view).unwrap().contains("PRIVATE"));
        let eden_protocol::presentation::Node::ConfigurationForm { fields, .. } = &view.nodes[0]
        else {
            panic!("configuration form")
        };
        for path in [
            "/api_key",
            "/credentials/providers/openai/literal",
            "/credentials/providers/deepseek/literal",
        ] {
            assert!(fields.iter().any(|field| field.path == path
                && field.control == eden_protocol::configuration_form::Control::Secret
                && field.writable
                && field.value.is_none()));
        }
    }
}
