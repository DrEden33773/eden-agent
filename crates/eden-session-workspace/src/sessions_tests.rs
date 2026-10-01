//! Cancellation at a transport gate must not drop an entire retired-reader batch.
use super::*;
use crate::{Cleanup, OpenOutcome, Opened};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Notify,
};

struct Reader {
    opened: Opened,
    detached: Arc<Notify>,
    closed: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Reader {
    fn drop(&mut self) {
        self.task.abort();
    }
}
struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn reader(directory: &std::path::Path, name: &str, gated: bool) -> Reader {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = directory.join(format!("{name}.json"));
    std::fs::write(
        &endpoint,
        json!({
            "address": listener.local_addr().unwrap().to_string(),
            "token": "fixture",
            "session_id": 1,
            "instance": name,
        })
        .to_string(),
    )
    .unwrap();
    let opened = Opened {
        endpoint: endpoint.clone(),
        history: Some(directory.join("history.jsonl")),
        session_id: 1,
        instance: name.into(),
        outcome: OpenOutcome::ReadOnly,
        cleanup: Cleanup::OwnedReader,
        read_only: true,
        draft: false,
        diagnostic: None,
    };
    let closed = Arc::new(AtomicBool::new(false));
    let closed_server = closed.clone();
    let detached = Arc::new(Notify::new());
    let detached_server = detached.clone();
    let task = tokio::spawn(async move {
        let mut requests = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted.unwrap();
                    let closed = closed_server.clone();
                    let detached = detached_server.clone();
                    let endpoint = endpoint.clone();
                    requests.spawn(async move {
                        let mut request = Vec::new();
                        let end = loop {
                            let mut chunk = [0; 4096];
                            let count = stream.read(&mut chunk).await.unwrap();
                            if count == 0 {
                                return;
                            }
                            request.extend_from_slice(&chunk[..count]);
                            if let Some(index) = request.windows(4).position(|b| b == b"\r\n\r\n") {
                                break index + 4;
                            }
                        };
                        let headers = String::from_utf8_lossy(&request[..end]).into_owned();
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        while request.len() < end + length {
                            let mut chunk = [0; 4096];
                            let count = stream.read(&mut chunk).await.unwrap();
                            if count == 0 {
                                return;
                            }
                            request.extend_from_slice(&chunk[..count]);
                        }
                        let route = headers.split_whitespace().nth(1).unwrap();
                        if route == "/detach" && gated {
                            detached.notify_one();
                            std::future::pending::<()>().await;
                        }
                        let shutting_down = route == "/shutdown";
                        let result = if route == "/tui/snapshot" {
                            json!({
                                "presentation": {
                                    "version": 1,
                                    "session_id": 1,
                                    "sequence": 0,
                                    "views": [],
                                    "activity": [],
                                    "pending_interactions": [],
                                },
                                "state": {
                                    "session_id": 1,
                                    "cwd": "",
                                    "closed": true,
                                    "read_only": true,
                                    "active_run": null,
                                },
                                "history": [],
                                "events": [],
                            })
                        } else {
                            json!({})
                        };
                        let body = json!({ "ok": true, "result": result })
                        .to_string();
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        stream.write_all(header.as_bytes()).await.unwrap();
                        stream.write_all(body.as_bytes()).await.unwrap();
                        stream.shutdown().await.unwrap();
                        if shutting_down {
                            std::fs::remove_file(endpoint).unwrap();
                            closed.store(true, Ordering::Release);
                        }
                    });
                },
                _ = requests.join_next(), if !requests.is_empty() => {}
            }
        }
    });
    Reader {
        opened,
        detached,
        closed,
        task,
    }
}

#[tokio::test]
async fn cancelled_retirement_keeps_cleanup_for_the_entire_reader_batch() {
    let directory = Directory(std::env::temp_dir().join(format!(
            "eden-retirement-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    std::fs::create_dir_all(&directory.0).unwrap();
    let lifecycle = crate::Lifecycle::new(
        directory.0.join("unused"),
        vec![],
        directory.0.clone(),
        directory.0.join("state"),
        vec![],
    );
    let first = reader(&directory.0, "a", true).await;
    let second = reader(&directory.0, "b", false).await;
    let selected = reader(&directory.0, "z", false).await;
    let (output, _events) = tokio::sync::mpsc::unbounded_channel();
    let mut adapters = Vec::new();
    for (name, reader) in [("a", &first), ("b", &second), ("z", &selected)] {
        let mut adapter = Adapter::new(
            reader.opened.endpoint.clone(),
            output.clone(),
            Some(name.into()),
        )
        .await
        .unwrap();
        adapter.opened = Some(reader.opened.clone());
        adapter.lease.store(1, Ordering::Release);
        adapters.push(Arc::new(adapter));
    }
    let server = Arc::new(Server::new(
        adapters[0].clone(),
        lifecycle.clone(),
        first.opened.clone(),
    ));
    for adapter in &adapters[1..] {
        server
            .sessions
            .lock()
            .await
            .insert(adapter.identity.clone(), adapter.clone());
    }
    let retiring = server.clone();
    let operation = tokio::spawn(async move { retiring.finish_load(Some("z"), true).await });
    tokio::time::timeout(std::time::Duration::from_secs(3), first.detached.notified())
        .await
        .unwrap();
    operation.abort();
    assert!(operation.await.unwrap_err().is_cancelled());
    lifecycle.finish_startup_cleanup().await.unwrap();
    assert!(
        first.closed.load(Ordering::Acquire),
        "first reader lost cleanup while detach was pending"
    );
    assert!(
        second.closed.load(Ordering::Acquire),
        "later readers lost cleanup before the loop reached them"
    );
    lifecycle.close(&selected.opened).await.unwrap();
}
