//! SDK-only replacements whose outputs are checked at the HTTP receiver.
use eden_plugin_sdk::tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
};
use eden_plugin_sdk::{
    CallContext, Package,
    protocol::{
        Descriptor, Fault,
        coding::{Item, ModelInput, ModelReply, PROVIDER},
        models::*,
    },
    serde_json::{self, Value, json},
};
use std::{collections::BTreeMap, path::PathBuf};

async fn custom_wire(
    input: ModelInput,
    cx: CallContext,
    address: String,
) -> Result<ModelReply, Fault> {
    let mut stream = TcpStream::connect(address).await.map_err(fault)?;
    let request = json!({ "version": "author-lines-v1", "items": input.items });
    stream
        .write_all(format!("{request}\n").as_bytes())
        .await
        .map_err(fault)?;
    let mut stream = BufReader::new(stream);
    let mut staged = Vec::<Item>::new();
    loop {
        let mut line = String::new();
        if stream.read_line(&mut line).await.map_err(fault)? == 0 {
            return Err(fault("custom wire closed before commit"));
        }
        if line.len() > 65536 {
            return Err(fault("custom wire frame exceeds limit"));
        }
        let frame: Value = serde_json::from_str(&line).map_err(fault)?;
        match frame["frame"].as_str() {
            Some("delta") => cx.emit("model_text_delta", json!({ "delta": frame["text"] }))?,
            Some("item") => {
                staged.push(serde_json::from_value(frame["item"].clone()).map_err(fault)?)
            }
            Some("commit") => {
                return Ok(ModelReply {
                    items: staged,
                    usage: json!({ "author_wire": true }),
                });
            }
            _ => return Err(fault("unknown custom wire frame")),
        }
    }
}

fn fault(error: impl std::fmt::Display) -> Fault {
    Fault::new("AuthorModelFailure", "model-services", error.to_string())
}
fn descriptor() -> Descriptor {
    Descriptor {
        package: "model-services".into(),
        version: "0.1.0".into(),
        provides: vec![
            PROVIDER.into(),
            MODEL_CATALOG.into(),
            MODEL_MANAGER.into(),
            CREDENTIAL_SOURCE.into(),
        ],
    }
}
fn create(config: Value) -> Result<Package, Fault> {
    let path = PathBuf::from(
        config["target_path"]
            .as_str()
            .ok_or_else(|| fault("target_path missing"))?,
    );
    let manager_path = path.clone();
    let address = config["wire_address"]
        .as_str()
        .unwrap_or("127.0.0.1:1")
        .to_owned();
    Ok(Package::new("model-services")
        .service(PROVIDER, move |input: ModelInput, cx| {
            custom_wire(input, cx, address.clone())
        })
        .service(MODEL_CATALOG, move |request: CatalogRequest, _| {
            let path = path.clone();
            async move {
                // Deliberately reread on every call: a mid-run file change reveals
                // accidental re-resolution instead of returning a cached fixture.
                let target: ModelTarget =
                    serde_json::from_slice(&std::fs::read(path).map_err(fault)?).map_err(fault)?;
                if let CatalogRequest::Resolve {
                    selection: Some(selection),
                }
                | CatalogRequest::SetDefault { selection } = request
                    && (selection.provider != target.provider || selection.model != target.model)
                {
                    return Err(fault("author selection does not exist"));
                }
                Ok(CatalogReply {
                    providers: vec![target.provider.clone()],
                    models: vec![CatalogEntry {
                        name: "Independent route".into(),
                        status: "configured".into(),
                        target: target.clone(),
                    }],
                    source: target.source.clone(),
                    target: Some(target),
                    status: "author".into(),
                })
            }
        })
        .service(MODEL_MANAGER, move |request: ManagerRequest, _| {
            let path = manager_path.clone();
            async move {
                let mut target: ModelTarget =
                    serde_json::from_slice(&std::fs::read(&path).map_err(fault)?).map_err(fault)?;
                if let ManagerRequest::Load { model, .. } = request {
                    target.model = model;
                    std::fs::write(&path, serde_json::to_vec(&target).map_err(fault)?)
                        .map_err(fault)?;
                }
                Ok(ManagerReply {
                    status: "author-managed".into(),
                    models: vec![ManagedModel {
                        id: target.model.clone(),
                        state: "loaded".into(),
                        source: "independent-author".into(),
                        failed: false,
                        selectable: true,
                        progress: None,
                        target,
                    }],
                    ..Default::default()
                })
            }
        })
        .service(CREDENTIAL_SOURCE, |_: CredentialRequest, _| async move {
            Ok(CredentialReply {
                base_url: None,
                available_model_ids: None,
                catalog_scope: None,
                api_key: Some(std::env::var("EDEN_AUTHOR_SECRET").map_err(fault)?),
                headers: BTreeMap::new(),
                source: "independent-author".into(),
            })
        }))
}
eden_plugin_sdk::export_plugin!(descriptor, create);
