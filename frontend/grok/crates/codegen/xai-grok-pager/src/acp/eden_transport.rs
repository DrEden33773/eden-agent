//! In-process presentation transport. The shared lifecycle library alone opens hosts.
use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, simplex};
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;
use xai_acp_lib::{AcpGatewayReceiver, AcpGatewaySender, LineBufferedRead, acp_channels};

pub(super) async fn connect(
    cancel: &CancellationToken,
    flags: super::ConnectFlags,
) -> Result<super::AcpConnection> {
    let endpoint = std::env::var_os("EDEN_FRONTEND_ENDPOINT")
        .context("missing selected endpoint")?
        .into();
    let lifecycle = serde_json::from_str(&std::env::var("EDEN_SESSION_LIFECYCLE")?)?;
    let opened = serde_json::from_str(&std::env::var("EDEN_FRONTEND_OPENED")?)?;
    let (client_channel, agent_channel) = acp_channels();
    let stop = cancel.clone();
    let thread = std::thread::Builder::new()
        .name("eden-presentation".into())
        .spawn(move || -> Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let local = tokio::task::LocalSet::new();
            local.block_on(&runtime, async move {
                let (request_tx, request_rx) = mpsc::unbounded_channel();
                let (output_tx, mut output_rx) = mpsc::unbounded_channel();
                let (incoming_read, mut incoming_write) = simplex(8 * 1024 * 1024);
                let (outgoing_read, outgoing_write) = simplex(8 * 1024 * 1024);
                let server = tokio::task::spawn_local(eden_frontend_session::serve(
                    endpoint, lifecycle, opened, request_rx, output_tx,
                ));
                let writer = tokio::task::spawn_local(async move {
                    let mut lines = BufReader::new(outgoing_read).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let Ok(request) = serde_json::from_str(&line) else {
                            break;
                        };
                        if request_tx.send(request).is_err() {
                            break;
                        }
                    }
                });
                let reader = tokio::task::spawn_local(async move {
                    while let Some(value) = output_rx.recv().await {
                        let mut bytes = serde_json::to_vec(&value)?;
                        bytes.push(b'\n');
                        incoming_write.write_all(&bytes).await?;
                    }
                    Ok::<_, anyhow::Error>(())
                });
                let gateway = AcpGatewaySender::new(agent_channel.tx).with_tracing(false);
                let incoming = LineBufferedRead::spawn_local(incoming_read.compat());
                let (connection, io) = agent_client_protocol::ClientSideConnection::new(
                    gateway,
                    outgoing_write.compat_write(),
                    incoming,
                    |future| {
                        tokio::task::spawn_local(future);
                    },
                );
                tokio::task::spawn_local(io);
                tokio::task::spawn_local(
                    AcpGatewayReceiver::new(agent_channel.rx, connection)
                        .with_tracing(false)
                        .run(),
                );
                stop.cancelled().await;
                // Dropping the only request sender starts the server's attachment/reader barrier.
                writer.abort();
                let _ = writer.await;
                let result = server.await?;
                reader.abort();
                result.map_err(anyhow::Error::from)
            })
        })?;
    // This inert local object remains a display dependency; it has no refresher or cloud transport.
    let home = std::env::var_os("GROK_HOME").context("missing frontend preferences directory")?;
    let auth = std::sync::Arc::new(xai_grok_login::AuthManager::new_with_proxy_base_url(
        std::path::Path::new(&home),
        Default::default(),
        "http://127.0.0.1:9".into(),
    ));
    super::initialize_connection(
        super::AgentEndpoint {
            tx: client_channel.tx,
            rx: client_channel.rx,
            cancel: cancel.clone(),
            location: super::AgentLocation::Thread(thread),
        },
        &flags,
        auth,
    )
    .await
}
