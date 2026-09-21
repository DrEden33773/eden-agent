//! Provider-specific routing is resolved before a run and contains no credentials.
use crate::wire::failure;
use eden_protocol::{
    Fault,
    models::{CredentialReply, ModelTarget},
};
use serde_json::{Value, json};

pub(crate) fn freeze(
    target: &mut ModelTarget,
    config: &Value,
    env: impl Fn(&str) -> Option<String>,
) {
    if !target.compat.is_object() {
        target.compat = json!({});
    }
    let cloud = &config["credentials"]["providers"][&target.provider]["cloud"];
    for (field, names) in [
        ("region", &["AWS_REGION", "AWS_DEFAULT_REGION"][..]),
        ("project", &["GOOGLE_CLOUD_PROJECT", "GCLOUD_PROJECT"][..]),
        ("location", &["GOOGLE_CLOUD_LOCATION"][..]),
    ] {
        if target.compat.get(field).is_none()
            && let Some(value) = cloud[field]
                .as_str()
                .map(str::to_owned)
                .or_else(|| names.iter().find_map(|name| env(name)))
        {
            target.compat[field] = json!(value);
        }
    }
    if target.api == "azure-openai-responses" {
        if target.base_url.is_empty() {
            target.base_url = env("AZURE_OPENAI_BASE_URL")
                .or_else(|| {
                    env("AZURE_OPENAI_RESOURCE_NAME")
                        .map(|r| format!("https://{r}.openai.azure.com/openai/v1"))
                })
                .unwrap_or_default();
        }
        if target.compat.get("apiVersion").is_none() {
            target.compat["apiVersion"] =
                json!(env("AZURE_OPENAI_API_VERSION").unwrap_or_else(|| "v1".into()));
        }
        if target.compat.get("deployment").is_none()
            && let Some(map) = env("AZURE_OPENAI_DEPLOYMENT_NAME_MAP")
            && let Some(deployment) = map
                .split(',')
                .filter_map(|p| p.split_once('='))
                .find_map(|(model, d)| (model.trim() == target.model).then(|| d.trim().to_owned()))
        {
            target.compat["deployment"] = json!(deployment);
        }
    }
    if target.api == "google-vertex" && target.base_url.contains("{location}") {
        let location = target.compat["location"].as_str().unwrap_or("global");
        target.base_url = if location == "global" {
            "https://aiplatform.googleapis.com".into()
        } else {
            target.base_url.replace("{location}", location)
        };
    }
    if target.api == "bedrock-converse-stream"
        && target.compat["endpointExplicit"] != true
        && target.base_url.starts_with("https://bedrock-runtime.")
        && target.base_url.ends_with(".amazonaws.com")
        && let Some(region) = target.compat["region"].as_str()
    {
        target.base_url = format!("https://bedrock-runtime.{region}.amazonaws.com");
    }
    if target.provider == "cloudflare-ai-gateway" {
        for name in ["CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_GATEWAY_ID"] {
            if let Some(value) = env(name) {
                target.base_url = target.base_url.replace(&format!("{{{name}}}"), &value);
            }
        }
    }
}
pub(crate) fn endpoint(
    target: &ModelTarget,
    credential: &CredentialReply,
    suffix: &str,
) -> Result<reqwest::Url, Fault> {
    let base = target.base_url.trim_end_matches('/');
    if base.contains('{') || base.contains('}') {
        return Err(failure("model endpoint requires provider configuration"));
    }
    let mut url = reqwest::Url::parse(base).map_err(|_| failure("invalid model endpoint"))?;
    if !["http", "https"].contains(&url.scheme())
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(failure(
            "model endpoint must use HTTP(S) without URL credentials",
        ));
    }
    let path = url.path().trim_end_matches('/').to_owned();
    let suffix = match target.api.as_str() {
        "google-generative-ai" => format!(
            "models/{}:streamGenerateContent",
            target.model.trim_start_matches("models/")
        ),
        "google-vertex" => {
            if credential.api_key.is_some() {
                format!(
                    "publishers/google/models/{}:streamGenerateContent",
                    target.model
                )
            } else {
                let project = target.compat["project"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| failure("Vertex ADC requires project configuration"))?;
                let location = target.compat["location"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| failure("Vertex ADC requires location configuration"))?;
                format!(
                    "projects/{project}/locations/{location}/publishers/google/models/{}:streamGenerateContent",
                    target.model
                )
            }
        }
        _ => suffix.into(),
    };
    let path = if path.ends_with(&suffix) {
        path
    } else {
        let prefix = match target.api.as_str() {
            "google-vertex" if !path.ends_with("/v1") && !path.ends_with("/v1beta1") => {
                format!("{path}/v1")
            }
            "azure-openai-responses" if path.is_empty() => "/openai/v1".into(),
            "azure-openai-responses" if path == "/openai" => "/openai/v1".into(),
            "mistral-conversations" if path.is_empty() => "/v1".into(),
            _ => path,
        };
        let suffix = if suffix == "v1/messages" && prefix.ends_with("/v1") {
            "messages"
        } else {
            &suffix
        };
        format!("{prefix}/{suffix}")
    };
    url.set_path(&path);
    if matches!(
        target.api.as_str(),
        "google-generative-ai" | "google-vertex"
    ) {
        url.query_pairs_mut().append_pair("alt", "sse");
    }
    if target.api == "azure-openai-responses" {
        let version = target.compat["apiVersion"].as_str().unwrap_or("v1");
        if version != "v1" {
            url.query_pairs_mut().append_pair("api-version", version);
        }
    }
    Ok(url)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::test_target;
    fn credential() -> CredentialReply {
        CredentialReply {
            api_key: Some("secret".into()),
            headers: Default::default(),
            source: "test".into(),
        }
    }
    #[test]
    fn region_override_updates_default_endpoint_but_preserves_explicit_endpoint() {
        let mut t = test_target("bedrock-converse-stream");
        t.base_url = "https://bedrock-runtime.us-east-1.amazonaws.com".into();
        t.source.kind = "explicit".into();
        t.compat = json!({ "region": "eu-west-1" });
        freeze(&mut t, &json!({}), |_| None);
        assert_eq!(
            t.base_url,
            "https://bedrock-runtime.eu-west-1.amazonaws.com"
        );
        t.base_url = "http://localhost:1234".into();
        t.compat["endpointExplicit"] = json!(true);
        freeze(&mut t, &json!({}), |_| None);
        assert_eq!(t.base_url, "http://localhost:1234");
    }
    #[test]
    fn azure_resource_deployment_and_version_are_frozen() {
        let mut t = test_target("azure-openai-responses");
        t.base_url.clear();
        freeze(&mut t, &json!({}), |name| match name {
            "AZURE_OPENAI_RESOURCE_NAME" => Some("resource".into()),
            "AZURE_OPENAI_DEPLOYMENT_NAME_MAP" => Some("test=deployment".into()),
            "AZURE_OPENAI_API_VERSION" => Some("preview".into()),
            _ => None,
        });
        assert_eq!(t.compat["deployment"], "deployment");
        assert_eq!(
            endpoint(&t, &credential(), "responses").unwrap().as_str(),
            "https://resource.openai.azure.com/openai/v1/responses?api-version=preview"
        );
    }
    #[test]
    fn vertex_adc_and_cloud_key_have_distinct_resource_paths() {
        let mut t = test_target("google-vertex");
        t.base_url = "https://{location}-aiplatform.googleapis.com".into();
        t.compat = json!({ "project": "p", "location": "global" });
        freeze(&mut t, &json!({}), |_| None);
        assert_eq!(
            endpoint(&t, &credential(), "").unwrap().path(),
            "/v1/publishers/google/models/test:streamGenerateContent"
        );
        let mut adc = credential();
        adc.api_key = None;
        assert_eq!(
            endpoint(&t, &adc, "").unwrap().path(),
            "/v1/projects/p/locations/global/publishers/google/models/test:streamGenerateContent"
        );
    }
}
