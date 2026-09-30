//! Eden transport for the fixed Grok pager. The host remains the execution owner.
use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(super) async fn connect(
    cancel: &CancellationToken,
    flags: super::ConnectFlags,
    adapter: std::path::PathBuf,
) -> Result<super::AcpConnection> {
    let mut command = if adapter
        .extension()
        .is_some_and(|extension| extension == "py")
    {
        let mut command = tokio::process::Command::new("python3");
        command.arg(&adapter);
        command
    } else {
        tokio::process::Command::new(&adapter)
    };
    let mut child = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("start Eden host adapter")?;
    let mut input = child.stdin.take().context("adapter stdin")?;
    let output = child.stdout.take().context("adapter stdout")?;
    let (to_adapter, mut requests) = mpsc::unbounded_channel::<String>();
    let (responses, from_adapter) = mpsc::unbounded_channel();
    let stop = cancel.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(output).lines();
        loop {
            tokio::select! {
                _ = stop.cancelled() => break,
                request = requests.recv() => match request {
                    Some(request) => {
                        if input.write_all(request.as_bytes()).await.is_err()
                            || input.write_all(b"\n").await.is_err()
                        {
                            break;
                        }
                    }
                    None => break,
                },
                response = lines.next_line() => match response {
                    Ok(Some(response)) => {
                        if responses.send(response).is_err() {
                            break;
                        }
                    }
                    _ => break,
                },
            }
        }
        // EOF lets the adapter detach its lease without cancelling accepted work.
        drop(input);
        if tokio::time::timeout(std::time::Duration::from_secs(3), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
        stop.cancel();
    });
    let bridge = super::leader_bridge::bridge_channels(
        to_adapter,
        from_adapter,
        cancel.clone(),
        None,
        xai_grok_shell::leader::ReconnectPolicy::unbounded(),
    )?;
    let home = std::env::var_os("GROK_HOME").context("isolated GROK_HOME is required")?;
    let auth = std::sync::Arc::new(xai_grok_login::AuthManager::new_with_proxy_base_url(
        std::path::Path::new(&home),
        Default::default(),
        "http://127.0.0.1:9".into(),
    ));
    super::initialize_connection(
        super::AgentEndpoint {
            tx: bridge.channel.tx,
            rx: bridge.channel.rx,
            cancel: bridge.cancel,
            location: super::AgentLocation::Thread(bridge.thread_handle),
        },
        &flags,
        auth,
    )
    .await
}
