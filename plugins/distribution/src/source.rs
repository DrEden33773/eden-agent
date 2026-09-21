//! Package sources: local directories, archives and builds, resolved before installation.
use super::{
    files,
    manager::{Source, error, io},
};
use eden_plugin_sdk::{Cancellation, protocol::Fault};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;
pub(crate) async fn command(
    executable: &str,
    args: &[&str],
    cwd: &Path,
    cancel: &Cancellation,
) -> Result<String, Fault> {
    check_cancel(cancel)?;
    let (mut child, tree) = eden_process::spawn(executable, args, cwd).await?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| error("PackageFailure", "missing stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| error("PackageFailure", "missing stderr"))?;
    async fn capture(mut input: impl tokio::io::AsyncRead + Unpin) -> Result<String, Fault> {
        let mut output = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            let n = input.read(&mut buffer).await.map_err(io)?;
            if n == 0 {
                break;
            }
            let keep = n.min(65536 - output.len());
            output.extend_from_slice(&buffer[..keep]);
        }
        Ok(String::from_utf8_lossy(&output).into_owned())
    }
    let wait = async {
        // Same contract as the shell tool: completion reads the leader's own
        // status and then only clears descendants that outlived it, while
        // cancellation stops the whole domain and records that decision
        // explicitly.
        let (stop, status, cancelled) = tokio::select! {
            status = child.wait() => {
                let status = status.map_err(io)?;
                let stop = eden_process::Stop::complete(&status);
                tree.cleanup_descendants()?;
                (stop, status, false)
            },
            _ = cancel.cancelled() => {
                tree.terminate_group()?;
                let status = child.wait().await.map_err(io)?;
                (eden_process::Stop::stopped(&status), status, true)
            }
        };
        tree.settle().await?;
        Ok::<_, Fault>((stop, status, cancelled))
    };
    let (waited, stdout, stderr) = tokio::join!(wait, capture(stdout), capture(stderr));
    let (_stop, status, cancelled) = waited?;
    let stdout = stdout?;
    let stderr = stderr?;
    if cancelled {
        return Err(error("Cancelled", "package command stopped"));
    }
    if status.success() {
        return Ok(stdout);
    }
    // A cancelled command is reported above; this branch keeps the outcome
    // text every other unsuccessful status already produced.
    Err(error(
        "BuildFailure",
        format!("{executable} exited {status}: {stdout}\n{stderr}"),
    ))
}
pub(crate) async fn prepare(
    source: &Source,
    destination: &Path,
    cancel: &Cancellation,
    client: &reqwest::Client,
) -> Result<(PathBuf, Value), Fault> {
    match source {
        Source::Local { path } => {
            let path = std::fs::canonicalize(path).map_err(io)?;
            if path.is_dir() {
                let actual_destination = std::fs::canonicalize(
                    destination
                        .parent()
                        .ok_or_else(|| error("InvalidInput", "staging has no parent"))?,
                )
                .map_err(io)?
                .join(
                    destination
                        .file_name()
                        .ok_or_else(|| error("InvalidInput", "staging has no name"))?,
                );
                if actual_destination.starts_with(&path) {
                    return Err(error(
                        "InvalidInput",
                        "package source contains its staging destination; choose a separate \
                         global directory",
                    ));
                }
                files::copy(&path, destination, cancel)?;
            } else {
                files::unpack(&path, destination, cancel)?;
            }
            Ok((
                files::bundle_root(destination)?,
                json!({ "kind": "local", "path": path }),
            ))
        }
        Source::Git { url, revision } => {
            if url.starts_with('-') || revision.starts_with('-') || revision.is_empty() {
                return Err(error(
                    "InvalidInput",
                    "Git source needs a URL and explicit revision",
                ));
            }
            let parent = destination
                .parent()
                .ok_or_else(|| error("InvalidInput", "missing staging parent"))?;
            let dest = destination.to_string_lossy();
            command(
                "git",
                &["clone", "--no-checkout", "--", url, &dest],
                parent,
                cancel,
            )
            .await?;
            let commit = command(
                "git",
                &["rev-parse", "--verify", &format!("{revision}^{{commit}}")],
                destination,
                cancel,
            )
            .await?
            .trim()
            .to_owned();
            if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(error(
                    "InvalidInput",
                    "Git revision did not resolve to a commit",
                ));
            }
            command(
                "git",
                &["checkout", "--detach", &commit],
                destination,
                cancel,
            )
            .await?;
            Ok((
                files::bundle_root(destination)?,
                json!({ "kind": "git", "url": url, "commit": commit }),
            ))
        }
        Source::Https { url, sha256 } => {
            let parsed =
                reqwest::Url::parse(url).map_err(|e| error("InvalidInput", e.to_string()))?;
            if !parsed.username().is_empty() || parsed.password().is_some() {
                return Err(error(
                    "InvalidInput",
                    "package URLs must not embed credentials",
                ));
            }
            if !url.starts_with("https://")
                || sha256.len() != 64
                || !sha256.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(error(
                    "InvalidInput",
                    "HTTPS archives require an https URL and SHA-256 digest",
                ));
            }
            let mut response = tokio::select! {
                _ = cancel.cancelled() => return Err(error("Cancelled", "download cancelled")),
                result = client.get(url).send() =>
                    result.map_err(|e| error("DownloadFailure", format!("{e:?}")))?,
            }
            .error_for_status()
            .map_err(|e| error("DownloadFailure", format!("{e:?}")))?;
            let archive = destination.with_extension("archive");
            let mut file = tokio::fs::File::create(&archive).await.map_err(io)?;
            let mut digest = Sha256::new();
            loop {
                let chunk = tokio::select! {
                    _ = cancel.cancelled() => return Err(error("Cancelled", "download cancelled")),
                    chunk = response.chunk() =>
                        chunk.map_err(|e| error("DownloadFailure", format!("{e:?}")))?,
                };
                let Some(chunk) = chunk else {
                    break;
                };
                digest.update(&chunk);
                use tokio::io::AsyncWriteExt;
                file.write_all(&chunk).await.map_err(io)?;
            }
            file.sync_all().await.map_err(io)?;
            drop(file);
            let actual = format!("{:x}", digest.finalize());
            if !actual.eq_ignore_ascii_case(sha256) {
                return Err(error(
                    "ChecksumMismatch",
                    format!("expected {sha256}, got {actual}"),
                ));
            }
            files::unpack(&archive, destination, cancel)?;
            Ok((
                files::bundle_root(destination)?,
                json!({ "kind": "https", "url": url, "sha256": actual }),
            ))
        }
    }
}
pub(crate) fn check_cancel(cancel: &Cancellation) -> Result<(), Fault> {
    if cancel.is_cancelled() {
        Err(error(
            "Cancelled",
            "package operation stopped before publication",
        ))
    } else {
        Ok(())
    }
}
pub(crate) fn client(ca: Option<&std::path::Path>) -> Result<reqwest::Client, Fault> {
    let mut builder =
        reqwest::Client::builder().redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.url().scheme() != "https" {
                attempt.error("HTTPS downgrade rejected")
            } else if attempt.previous().len() > 10 {
                attempt.error("too many redirects")
            } else {
                attempt.follow()
            }
        }));
    if let Some(path) = ca {
        if !path.is_absolute() {
            return Err(error(
                "InvalidInput",
                "distribution.ca_certificate must be an absolute PEM path",
            ));
        }
        let cert = reqwest::Certificate::from_pem(&std::fs::read(path).map_err(io)?)
            .map_err(|e| error("InvalidInput", e.to_string()))?;
        builder = builder.tls_certs_merge([cert]);
    }
    builder
        .build()
        .map_err(|e| error("DownloadFailure", format!("{e:?}")))
}
