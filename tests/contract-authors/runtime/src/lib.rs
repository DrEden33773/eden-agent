//! Independent consumer of instance routing, continuations and managed jobs.
use eden_plugin_sdk::{
    Package, export_plugin,
    protocol::{self as p, Fault},
    serde_json::{Value, json},
    tokio,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
const CONTROL: &str = "author.runtime.control.v1";
const WORK: &str = "author.runtime.work.v1";
fn descriptor() -> p::Descriptor {
    p::Descriptor {
        package: "runtime-author".into(),
        version: "0.1.0".into(),
        provides: vec![
            p::CONTEXT.into(),
            CONTROL.into(),
            WORK.into(),
            p::runtime::READY.into(),
            p::configuration::CONFIGURATION.into(),
            p::INSTANCE_STOP.into(),
        ],
    }
}
fn io(e: std::io::Error) -> Fault {
    Fault::new("IoFailure", "runtime-author", e.to_string())
}
fn factory(config: Value) -> Result<Package, Fault> {
    let attempts = if let Some(path) = config["attempts_file"].as_str() {
        let previous = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| text.parse::<u64>().ok())
            .unwrap_or(0);
        std::fs::write(path, (previous + 1).to_string()).map_err(io)?;
        previous + 1
    } else {
        0
    };
    if config["fail_prepare"] == true
        || config["fail_after"]
            .as_u64()
            .is_some_and(|limit| attempts > limit)
    {
        return Err(Fault::new(
            "AuthorPrepareFailure",
            "runtime-author",
            "controlled initialization failure",
        ));
    }
    let mode = config["mode"].as_str().unwrap_or("tail").to_owned();
    let effective = std::sync::Arc::new(std::sync::Mutex::new(config.clone()));
    let context_config = effective.clone();
    let control_config = effective.clone();
    let update_config = effective.clone();
    let environment = p::environment::HostEnvironment::from_config(&config)?;
    Ok(Package::new("runtime-author")
        .service(p::CONTEXT, move |input: p::RunInput, cx| {
            let mode = mode.clone();
            let label = context_config
                .lock()
                .unwrap()
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or("author")
                .to_owned();
            async move {
                if label == "one"
                    && let Some(endpoint) = input.prompt.strip_prefix("hold@")
                {
                    let stream = std::sync::Arc::new(tokio::sync::Mutex::new(
                        tokio::net::TcpStream::connect(endpoint).await.map_err(io)?,
                    ));
                    let cleanup = stream.clone();
                    cx.scope.cleanup(async move {
                        let mut stream = cleanup.lock().await;
                        stream.write_all(b"cleanup").await.map_err(io)?;
                        let mut ack = [0; 3];
                        stream.read_exact(&mut ack).await.map_err(io)?;
                        if &ack != b"ack" {
                            return Err(Fault::new(
                                "CleanupFailure",
                                "runtime-author",
                                "bad foreground cleanup acknowledgement",
                            ));
                        }
                        stream.shutdown().await.map_err(io)?;
                        Ok(())
                    })?;
                    let mut stream = stream.lock().await;
                    stream.write_all(b"ready").await.map_err(io)?;
                    let mut release = [0; 3];
                    stream.read_exact(&mut release).await.map_err(io)?;
                }
                if input.prompt.contains("fail") && label == "two" {
                    return Err(Fault::new(
                        "AuthorFailure",
                        &label,
                        "native downstream failure",
                    ));
                }
                if label == "one"
                    && let Some(endpoint) = input.prompt.strip_prefix("job@")
                {
                    let job = cx.submit_job(WORK, &endpoint).await?;
                    cx.emit("author_job_registered", json!(job))?;
                }
                if mode == "wrapper" {
                    if input.prompt == "short" {
                        return Ok(p::ModelInput {
                            text: format!("{label}:short"),
                            tool_result: None,
                        });
                    }
                    let result: p::ModelInput = cx
                        .delegate(&p::RunInput {
                            prompt: format!("{label}({})", input.prompt),
                        })
                        .await?;
                    if cx
                        .delegate::<_, p::ModelInput>(&input)
                        .await
                        .unwrap_err()
                        .code
                        != "ExpiredContinuation"
                    {
                        return Err(Fault::new("ContractFailure", &label, "continuation reused"));
                    }
                    Ok(p::ModelInput {
                        text: format!("{label}[{}]", result.text),
                        tool_result: result.tool_result,
                    })
                } else {
                    Ok(p::ModelInput {
                        text: format!("{label}({})", input.prompt),
                        tool_result: None,
                    })
                }
            }
        })
        .service(CONTROL, move |input: Value, cx| {
            let environment = environment.clone();
            let effective = control_config.clone();
            async move {
                match input["op"].as_str().unwrap_or("") {
                    "identity" => Ok(json!(cx.identity())),
                    "configuration" => Ok(effective.lock().unwrap().clone()),
                    "environment" => {
                        let value = match environment {
                            Some(value) => Some(value),
                            None => cx.host_environment().await?,
                        };
                        Ok(json!(value))
                    }
                    "start" => {
                        let value = cx.submit_job(WORK, &input["endpoint"]).await?;
                        Ok(json!(value))
                    }
                    "inspect" => {
                        let value = cx.inspect_job(input["job"].as_u64().unwrap()).await?;
                        Ok(json!(value))
                    }
                    "cancel" => {
                        cx.cancel_job(input["job"].as_u64().unwrap()).await?;
                        Ok(Value::Null)
                    }
                    "join" => {
                        let value = cx.join_job(input["job"].as_u64().unwrap()).await?;
                        Ok(json!(value))
                    }
                    "scoped" => {
                        let value: p::ModelInput = cx
                            .call_in(
                                input["scope"].as_str().unwrap(),
                                p::CONTEXT,
                                &p::RunInput { prompt: "x".into() },
                            )
                            .await?;
                        Ok(json!(value.text))
                    }
                    _ => Err(Fault::new(
                        "InvalidInput",
                        "runtime-author",
                        "unknown operation",
                    )),
                }
            }
        })
        .service(WORK, |endpoint: String, cx| async move {
            let stream = std::sync::Arc::new(tokio::sync::Mutex::new(
                tokio::net::TcpStream::connect(endpoint).await.map_err(io)?,
            ));
            let cleanup = stream.clone();
            cx.scope.cleanup(async move {
                let mut stream = cleanup.lock().await;
                stream.write_all(b"cleanup").await.map_err(io)?;
                let mut ack = [0; 3];
                stream.read_exact(&mut ack).await.map_err(io)?;
                if &ack != b"ack" {
                    return Err(Fault::new(
                        "CleanupFailure",
                        "runtime-author",
                        "missing cleanup acknowledgement",
                    ));
                }
                stream.shutdown().await.map_err(io)?;
                Ok(())
            })?;
            stream.lock().await.write_all(b"ready").await.map_err(io)?;
            let mut cursor = 0;
            loop {
                let batch = cx
                    .events_after(cursor, vec!["next_input".into(), "accepted".into()])
                    .await?;
                cursor = batch.cursor;
                for event in batch.events {
                    cx.emit(
                        "job_observed_input",
                        json!({ "input_run": event.run_id, "identity": cx.identity() }),
                    )?;
                }
            }
            #[allow(unreachable_code)]
            Ok::<(), Fault>(())
        })
        .service(p::runtime::READY, |_: (), cx| async move {
            cx.emit("author_ready", json!(cx.identity()))
        })
        .service(
            p::configuration::CONFIGURATION,
            move |request: p::configuration::PluginRequest, _| {
                let effective = update_config.clone();
                async move {
                    use p::configuration::{Description, FieldError, PluginRequest, Validation};
                    match request {
                        PluginRequest::Describe => Ok(json!(Description {
                            schema: Some(json!({
                                "type": "object",
                                "properties": {
                                    "label": { "type": "string", "minLength": 1 },
                                    "fail_prepare": { "type": "boolean" },
                                    "fail_stop": { "type": "boolean" },
                                },
                            })),
                            live_paths: vec!["/label".into()],
                            description: Some(
                                "Independent native restart and atomic live-update fixture".into()
                            ),
                            ..Description::default()
                        })),
                        PluginRequest::Validate { config } | PluginRequest::Update { config }
                            if config["label"] == "forbidden" =>
                        {
                            Ok(json!(Validation {
                                errors: vec![FieldError {
                                    path: "/label".into(),
                                    code: "author_label".into(),
                                    message: "Label is reserved".into()
                                }]
                            }))
                        }
                        PluginRequest::Validate { .. } => Ok(json!(Validation::default())),
                        PluginRequest::Update { config } => {
                            *effective.lock().unwrap() = config;
                            Ok(json!(Validation::default()))
                        }
                    }
                }
            },
        )
        .service(p::INSTANCE_STOP, move |_: (), _| {
            let fail = effective.lock().unwrap()["fail_stop"] == true;
            async move {
                if fail {
                    Err(Fault::new(
                        "CleanupFailure",
                        "runtime-author",
                        "controlled finalizer failure",
                    ))
                } else {
                    Ok(())
                }
            }
        }))
}
export_plugin!(descriptor, factory);
