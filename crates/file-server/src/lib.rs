pub mod config;

use axum::{
    Router,
    body::Body,
    extract::{Path, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};
use tokio::io::AsyncWriteExt;

struct Store {
    root: PathBuf,
    max_upload_bytes: u64,
}

#[derive(Debug, thiserror::Error)]
enum BlobError {
    #[error("invalid blob path or request body")]
    BadRequest,
    #[error("trusted identity required")]
    Unauthorized,
    #[error("upload exceeds configured limit")]
    TooLarge,
    #[error("SHA256 does not match blob key")]
    HashMismatch,
    #[error("conflicting or nonregular blob entry")]
    Conflict,
    #[error("blob not found")]
    NotFound,
    #[error("blob storage I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("blob storage task: {0}")]
    Task(#[from] tokio::task::JoinError),
}

impl IntoResponse for BlobError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::BadRequest => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::HashMismatch => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Conflict => StatusCode::CONFLICT,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Io(_) | Self::Task(_) => {
                eprintln!("file-server: {self}");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        status.into_response()
    }
}

/// The caller must use a private, trusted local directory and loopback listener.
/// See the README for the trusted-proxy identity and tailnet ACL boundary.
pub fn router(root: PathBuf, max_upload_bytes: u64) -> Router {
    Router::new()
        .route("/nar/{name}", get(download).put(upload))
        .with_state(Arc::new(Store {
            root,
            max_upload_bytes,
        }))
}

fn valid_name(name: &str) -> bool {
    name.strip_suffix(".nar").is_some_and(|hash| {
        hash.len() == 64
            && hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn open_blob(path: &FsPath) -> Result<std::fs::File, BlobError> {
    use std::os::unix::fs::OpenOptionsExt;
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            BlobError::NotFound
        } else {
            error.into()
        }
    })?;
    if !metadata.is_file() {
        return Err(BlobError::Conflict);
    }
    // No symlink following; NONBLOCK prevents hanging on a substituted FIFO.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| {
            if error.raw_os_error() == Some(libc::ELOOP) {
                BlobError::Conflict
            } else {
                error.into()
            }
        })?;
    if !file.metadata()?.is_file() {
        return Err(BlobError::Conflict);
    }
    Ok(file)
}

async fn upload(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    request: Request,
) -> Result<StatusCode, BlobError> {
    if !valid_name(&name) {
        return Err(BlobError::BadRequest);
    }
    // This is not standalone authentication: only a trusted local proxy may
    // populate this header. The executable rejects non-loopback bind addresses.
    let identities = request.headers().get_all("Tailscale-User-Login");
    let mut identities = identities.iter();
    if identities
        .next()
        .and_then(|value| value.to_str().ok())
        .is_none_or(|value| value.trim().is_empty())
        || identities.next().is_some()
    {
        return Err(BlobError::Unauthorized);
    }
    if request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > store.max_upload_bytes)
    {
        return Err(BlobError::TooLarge);
    }
    let root = store.root.clone();
    // Keep the RAII owner until publication so rejected/cancelled uploads clean up.
    let temp = tokio::task::spawn_blocking(move || tempfile::NamedTempFile::new_in(root)).await??;
    let mut file = tokio::fs::File::from_std(temp.as_file().try_clone()?);
    let size = receive(
        request.into_body(),
        &name,
        store.max_upload_bytes,
        &mut file,
    )
    .await?;
    file.sync_all().await?;
    drop(file);
    let destination = store.root.join(name);
    tokio::task::spawn_blocking(move || publish(temp, &destination, size)).await?
}

async fn receive(
    body: Body,
    name: &str,
    max_upload_bytes: u64,
    file: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<u64, BlobError> {
    // Bound per-upload buffering; coalesce small frames without collecting the body.
    // Explicitly flush before returning so the caller fsyncs every byte, including the tail.
    let mut file = tokio::io::BufWriter::with_capacity(64 * 1024, file);
    let mut stream = body.into_data_stream();
    let mut size = 0_u64;
    let mut digest = Sha256::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| BlobError::BadRequest)?;
        size = size
            .checked_add(chunk.len() as u64)
            .ok_or(BlobError::TooLarge)?;
        if size > max_upload_bytes {
            return Err(BlobError::TooLarge);
        }
        digest.update(&chunk);
        file.write_all(&chunk).await?;
    }
    if name != format!("{:x}.nar", digest.finalize()) {
        return Err(BlobError::HashMismatch);
    }
    file.flush().await?;
    Ok(size)
}

fn publish(
    temp: tempfile::NamedTempFile,
    destination: &FsPath,
    size: u64,
) -> Result<StatusCode, BlobError> {
    publish_with_sync(temp, destination, size, || {
        let parent = destination.parent().ok_or(BlobError::BadRequest)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })
}

fn publish_with_sync(
    temp: tempfile::NamedTempFile,
    destination: &FsPath,
    size: u64,
    sync: impl FnOnce() -> Result<(), BlobError>,
) -> Result<StatusCode, BlobError> {
    // Same-directory no-clobber publication is atomic, including concurrent PUTs.
    match temp.persist_noclobber(destination) {
        Ok(_) => {
            sync()?;
            Ok(StatusCode::CREATED)
        }
        Err(mut error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut existing = open_blob(destination)?;
            if existing.metadata()?.len() != size {
                return Err(BlobError::Conflict);
            }
            error.file.seek(SeekFrom::Start(0))?;
            let mut left = [0_u8; 65536];
            let mut right = [0_u8; 65536];
            loop {
                let count = error.file.read(&mut left)?;
                if count == 0 {
                    // A previous request may have published but failed directory fsync.
                    sync()?;
                    return Ok(StatusCode::OK);
                }
                existing.read_exact(&mut right[..count])?;
                if left[..count] != right[..count] {
                    return Err(BlobError::Conflict);
                }
            }
        }
        Err(error) => Err(error.error.into()),
    }
}

async fn download(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
) -> Result<Response, BlobError> {
    download_with_sync(State(store), Path(name), std::fs::File::sync_all).await
}

async fn download_with_sync(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    mut sync: impl FnMut(&std::fs::File) -> std::io::Result<()> + Send + 'static,
) -> Result<Response, BlobError> {
    if !valid_name(&name) {
        return Err(BlobError::BadRequest);
    }
    let path = store.root.join(name);
    let file = tokio::task::spawn_blocking(move || -> Result<_, BlobError> {
        let file = open_blob(&path)?;
        // Publication becomes visible before its directory fsync can complete/fail.
        // Independently repair that window, on this same open file, before any ack.
        sync(&file)?;
        let parent = std::fs::File::open(path.parent().ok_or(BlobError::BadRequest)?)?;
        sync(&parent)?;
        Ok(file)
    })
    .await??;
    let file = tokio::fs::File::from_std(file);
    let size = file.metadata().await?.len();
    Ok((
        [
            (header::CONTENT_LENGTH, size.to_string()),
            (header::CONTENT_TYPE, "application/x-nix-nar".to_owned()),
            (
                axum::http::HeaderName::from_static("x-repro2-durable"),
                "file-and-parent-v1".to_owned(),
            ),
        ],
        Body::from_stream(tokio_util::io::ReaderStream::new(file)),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::{Client, StatusCode};
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    struct Server {
        root: TempDir,
        url: String,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn server(limit: u64) -> Server {
        let root = tempfile::tempdir().unwrap();
        let app = router(root.path().to_owned(), limit);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Server { root, url, task }
    }

    // Serialize a real, uncompressed NAR containing one regular file.
    fn nar(contents: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for field in [
            b"nix-archive-1".as_slice(),
            b"(",
            b"type",
            b"regular",
            b"contents",
            contents,
            b")",
        ] {
            bytes.extend_from_slice(&(field.len() as u64).to_le_bytes());
            bytes.extend_from_slice(field);
            bytes.resize(bytes.len().next_multiple_of(8), 0);
        }
        bytes
    }

    fn key(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    #[derive(Default)]
    struct CountingWriter {
        bytes: Vec<u8>,
        writes: usize,
        flushes: usize,
        fail_write: bool,
        fail_flush: bool,
    }

    impl tokio::io::AsyncWrite for CountingWriter {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.writes += 1;
            if self.fail_write {
                return std::task::Poll::Ready(Err(std::io::Error::other(
                    "injected write failure",
                )));
            }
            self.bytes.extend_from_slice(bytes);
            std::task::Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            self.flushes += 1;
            if self.fail_flush {
                return std::task::Poll::Ready(Err(std::io::Error::other(
                    "injected flush failure",
                )));
            }
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            self.poll_flush(cx)
        }
    }

    #[tokio::test]
    async fn receive_coalesces_small_writes_and_flushes_exact_partial_tail() {
        let bytes = nar(&vec![b'x'; 2 * 64 * 1024 + 17]);
        let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
            bytes.chunks(64).map(|chunk| Ok(chunk.to_vec())).collect();
        let body = Body::from_stream(futures_util::stream::iter(chunks));
        let mut writer = CountingWriter::default();
        let size = receive(
            body,
            &format!("{}.nar", key(&bytes)),
            bytes.len() as u64,
            &mut writer,
        )
        .await
        .unwrap();
        assert_eq!(size, bytes.len() as u64);
        assert_eq!(
            writer.bytes, bytes,
            "partial tail must reach the file before fsync"
        );
        assert_eq!(writer.flushes, 1);
        assert!(
            writer.writes <= 3,
            "small chunks caused {} writes",
            writer.writes
        );
    }

    #[tokio::test]
    async fn receive_propagates_buffer_write_and_flush_errors() {
        for (size, fail_write, fail_flush, expected) in [
            (65537, true, false, "injected write failure"),
            (17, true, false, "injected write failure"),
            (17, false, true, "injected flush failure"),
        ] {
            let bytes = nar(&vec![b'x'; size]);
            let mut writer = CountingWriter {
                fail_write,
                fail_flush,
                ..Default::default()
            };
            let error = receive(
                Body::from(bytes.clone()),
                &format!("{}.nar", key(&bytes)),
                bytes.len() as u64,
                &mut writer,
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
            assert!(writer.writes > 0);
            if fail_flush {
                assert_eq!(writer.bytes, bytes);
                assert_eq!(writer.flushes, 1);
            }
        }
    }

    #[tokio::test]
    async fn chunked_upload_publishes_exact_bytes_with_partial_buffer_tail() {
        let bytes = nar(&vec![b't'; 2 * 64 * 1024 + 17]);
        let server = server(bytes.len() as u64).await;
        let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
            bytes.chunks(64).map(|chunk| Ok(chunk.to_vec())).collect();
        let url = format!("{}/nar/{}.nar", server.url, key(&bytes));
        let client = Client::new();
        let response = client
            .put(&url)
            .header("Tailscale-User-Login", "alice@example.com")
            .body(reqwest::Body::wrap_stream(futures_util::stream::iter(
                chunks,
            )))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let response = client.get(&url).send().await.unwrap();
        assert_eq!(
            response.headers()["content-length"],
            bytes.len().to_string()
        );
        assert_eq!(response.bytes().await.unwrap().as_ref(), bytes);
        assert_eq!(std::fs::read_dir(server.root.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn cancelled_or_broken_body_removes_unpublished_temporary_file() {
        use std::time::Duration;
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store {
            root: root.path().to_owned(),
            max_upload_bytes: 1024 * 1024,
        });
        let name = format!("{}.nar", "0".repeat(64));
        let request = |body| {
            Request::builder()
                .header("Tailscale-User-Login", "alice@example.com")
                .body(body)
                .unwrap()
        };
        // Leave a partial receive buffer and a real temporary file live, then cancel.
        let (sent, received) = tokio::sync::oneshot::channel();
        let stream = futures_util::stream::once(async move {
            let _ = sent.send(());
            Ok::<_, std::io::Error>(vec![b'x'; 17])
        })
        .chain(futures_util::stream::pending());
        let task = tokio::spawn(upload(
            State(store.clone()),
            Path(name.clone()),
            request(Body::from_stream(stream)),
        ));
        tokio::time::timeout(Duration::from_secs(5), received)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        let stream = futures_util::stream::iter([
            Ok(vec![b'x'; 17]),
            Err(std::io::Error::other("broken request body")),
        ]);
        assert!(matches!(
            upload(State(store), Path(name), request(Body::from_stream(stream))).await,
            Err(BlobError::BadRequest)
        ));
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn head_repairs_failed_publication_before_marking_durable() {
        use std::io::Write;
        let server = server(1024).await;
        let bytes = nar(b"failed publication");
        let name = format!("{}.nar", key(&bytes));
        let path = server.root.path().join(&name);
        let mut temp = tempfile::NamedTempFile::new_in(server.root.path()).unwrap();
        temp.write_all(&bytes).unwrap();
        temp.as_file().sync_all().unwrap();
        assert!(
            publish_with_sync(temp, &path, bytes.len() as u64, || {
                Err(std::io::Error::other("injected publication directory fsync failure").into())
            })
            .is_err()
        );
        assert!(path.exists(), "failed publication leaves a visible entry");
        let response = Client::new()
            .head(format!("{}/nar/{name}", server.url))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("x-repro2-durable")
                .and_then(|v| v.to_str().ok()),
            Some("file-and-parent-v1")
        );
        assert_eq!(
            response.headers()["content-length"],
            bytes.len().to_string()
        );
    }

    #[tokio::test]
    async fn read_sync_errors_have_no_usable_marker_and_retry_syncs_file_then_parent() {
        let root = tempfile::tempdir().unwrap();
        let bytes = nar(b"repair me");
        let name = format!("{}.nar", key(&bytes));
        std::fs::write(root.path().join(&name), &bytes).unwrap();
        let store = Arc::new(Store {
            root: root.path().to_owned(),
            max_upload_bytes: 1024,
        });
        // Injectable fault/ordering test, NOT a simulated power-cut durability proof.
        for fail_at in [0, 1] {
            let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
            let observed = calls.clone();
            let response =
                download_with_sync(State(store.clone()), Path(name.clone()), move |file| {
                    let mut calls = observed.lock().unwrap();
                    calls.push(file.metadata()?.is_dir());
                    if calls.len() == fail_at + 1 {
                        return Err(std::io::Error::other("injected read fsync failure"));
                    }
                    file.sync_all()
                })
                .await
                .unwrap_err()
                .into_response();
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            assert!(!response.headers().contains_key("x-repro2-durable"));
            assert_eq!(
                *calls.lock().unwrap(),
                if fail_at == 0 {
                    vec![false]
                } else {
                    vec![false, true]
                }
            );
        }
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = calls.clone();
        let response = download_with_sync(State(store), Path(name), move |file| {
            observed.lock().unwrap().push(file.metadata()?.is_dir());
            file.sync_all()
        })
        .await
        .unwrap();
        assert_eq!(*calls.lock().unwrap(), vec![false, true]);
        assert_eq!(response.headers()["x-repro2-durable"], "file-and-parent-v1");
        assert_eq!(
            response.headers()["content-length"],
            bytes.len().to_string()
        );
    }

    #[tokio::test]
    async fn concurrent_head_does_its_own_sync_before_ack_while_publication_is_pending() {
        use std::{io::Write, time::Duration};
        let root = tempfile::tempdir().unwrap();
        let bytes = nar(b"concurrent");
        let name = format!("{}.nar", key(&bytes));
        let path = root.path().join(&name);
        let mut temp = tempfile::NamedTempFile::new_in(root.path()).unwrap();
        temp.write_all(&bytes).unwrap();
        temp.as_file().sync_all().unwrap();
        let (visible_tx, visible_rx) = tokio::sync::oneshot::channel();
        let (publish_tx, publish_rx) = std::sync::mpsc::channel();
        let publisher = tokio::task::spawn_blocking(move || {
            publish_with_sync(temp, &path, bytes.len() as u64, || {
                visible_tx.send(()).unwrap();
                publish_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                Err(std::io::Error::other("injected pending publication fsync failure").into())
            })
        });
        visible_rx.await.unwrap();
        let store = Arc::new(Store {
            root: root.path().to_owned(),
            max_upload_bytes: 1024,
        });
        let (sync_tx, sync_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let mut sync_tx = Some(sync_tx);
        let reader = tokio::spawn(download_with_sync(
            State(store.clone()),
            Path(name.clone()),
            move |file| {
                if file.metadata()?.is_dir() {
                    sync_tx.take().unwrap().send(()).unwrap();
                    finish_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                file.sync_all()
            },
        ));
        tokio::time::timeout(Duration::from_secs(5), sync_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !reader.is_finished(),
            "HEAD acknowledged before its parent fsync"
        );
        finish_tx.send(()).unwrap();
        let response = reader.await.unwrap().unwrap();
        assert_eq!(response.headers()["x-repro2-durable"], "file-and-parent-v1");
        assert!(
            !publisher.is_finished(),
            "test did not overlap publication and HEAD"
        );
        publish_tx.send(()).unwrap();
        assert!(publisher.await.unwrap().is_err());
        let retry = download(State(store), Path(name)).await.unwrap();
        assert_eq!(retry.headers()["x-repro2-durable"], "file-and-parent-v1");
    }

    #[test]
    fn publication_never_acknowledges_before_directory_sync_including_dedup_retry() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("blob.nar");
        let make = || {
            let mut temp = tempfile::NamedTempFile::new_in(root.path()).unwrap();
            temp.write_all(b"bytes").unwrap();
            temp.as_file().sync_all().unwrap();
            temp
        };
        let failure = || {
            Err(BlobError::Io(std::io::Error::other(
                "injected directory fsync failure",
            )))
        };
        assert!(publish_with_sync(make(), &destination, 5, failure).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"bytes");
        assert!(publish_with_sync(make(), &destination, 5, failure).is_err());
        let mut synced = false;
        assert_eq!(
            publish_with_sync(make(), &destination, 5, || {
                std::fs::File::open(root.path())?.sync_all()?;
                synced = true;
                Ok(())
            })
            .unwrap(),
            StatusCode::OK
        );
        assert!(synced);
    }

    #[tokio::test]
    async fn never_follows_symlinks_or_serves_nonregular_blob_entries() {
        use std::os::unix::fs::symlink;
        let server = server(1024).await;
        let outside = tempfile::tempdir().unwrap();
        let bytes = nar(b"outside secret");
        let hash = key(&bytes);
        let secret = outside.path().join("secret");
        std::fs::write(&secret, &bytes).unwrap();
        let path = server.root.path().join(format!("{hash}.nar"));
        symlink(&secret, &path).unwrap();
        let client = Client::new();
        let url = format!("{}/nar/{hash}.nar", server.url);
        assert_eq!(
            client.get(&url).send().await.unwrap().status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            client
                .put(&url)
                .header("Tailscale-User-Login", "alice@example.com")
                .body(bytes.clone())
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(std::fs::read(&secret).unwrap(), bytes);
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            client.get(&url).send().await.unwrap().status(),
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn uploads_require_one_nonempty_trusted_identity_header() {
        let server = server(1024).await;
        let bytes = nar(b"authenticated");
        let url = format!("{}/nar/{}.nar", server.url, key(&bytes));
        let client = Client::new();
        for identity in [
            None,
            Some(b"".as_slice()),
            Some(b"   ".as_slice()),
            Some(b"\xff".as_slice()),
        ] {
            let mut request = client.put(&url).body(bytes.clone());
            if let Some(identity) = identity {
                request = request.header(
                    "Tailscale-User-Login",
                    reqwest::header::HeaderValue::from_bytes(identity).unwrap(),
                );
            }
            assert_eq!(
                request.send().await.unwrap().status(),
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(
            client
                .put(&url)
                .header("Tailscale-User-Login", "alice@example.com")
                .header("Tailscale-User-Login", "bob@example.com")
                .body(bytes)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(std::fs::read_dir(server.root.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn enforces_upload_limit_for_known_length_and_chunked_bodies() {
        let bytes = nar(b"limit test");
        let server = server(bytes.len() as u64 - 1).await;
        let client = Client::new();
        let url = format!("{}/nar/{}.nar", server.url, key(&bytes));
        let response = client
            .put(&url)
            .header("Tailscale-User-Login", "alice@example.com")
            .body(bytes.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
            bytes.chunks(9).map(|chunk| Ok(chunk.to_vec())).collect();
        let response = client
            .put(&url)
            .header("Tailscale-User-Login", "alice@example.com")
            .body(reqwest::Body::wrap_stream(futures_util::stream::iter(
                chunks,
            )))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(std::fs::read_dir(server.root.path()).unwrap().count(), 0);
        let exact = super::tests::server(bytes.len() as u64).await;
        assert_eq!(
            client
                .put(format!("{}/nar/{}.nar", exact.url, key(&bytes)))
                .header("Tailscale-User-Login", "alice@example.com")
                .body(bytes)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::CREATED
        );
    }

    #[tokio::test]
    async fn conflicting_existing_content_is_never_overwritten_or_acknowledged() {
        let server = server(1024).await;
        let bytes = nar(b"valid upload");
        let hash = key(&bytes);
        let path = server.root.path().join(format!("{hash}.nar"));
        let corrupt = nar(b"preexisting corruption");
        std::fs::write(&path, &corrupt).unwrap();
        let response = Client::new()
            .put(format!("{}/nar/{hash}.nar", server.url))
            .header("Tailscale-User-Login", "alice@example.com")
            .body(bytes)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(std::fs::read(path).unwrap(), corrupt);
        assert_eq!(std::fs::read_dir(server.root.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn identical_uploads_dedupe_atomically_without_replacing_the_file() {
        use std::os::unix::fs::MetadataExt;
        let server = server(1024).await;
        let bytes = nar(b"same bytes");
        let hash = key(&bytes);
        let url = format!("{}/nar/{hash}.nar", server.url);
        let client = Client::new();
        let put = || {
            client
                .put(&url)
                .header("Tailscale-User-Login", "alice@example.com")
                .body(bytes.clone())
                .send()
        };
        let (first, second) = tokio::join!(put(), put());
        let mut statuses = [
            first.unwrap().status().as_u16(),
            second.unwrap().status().as_u16(),
        ];
        statuses.sort();
        assert_eq!(statuses, [200, 201]);
        let path = server.root.path().join(format!("{hash}.nar"));
        let before = std::fs::metadata(&path).unwrap();
        assert_eq!(put().await.unwrap().status(), StatusCode::OK);
        let after = std::fs::metadata(path).unwrap();
        assert_eq!(before.ino(), after.ino());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        assert_eq!(std::fs::read_dir(server.root.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn rejects_noncanonical_hashes_and_encoded_path_traversal() {
        let server = server(1024).await;
        let client = Client::new();
        for name in [
            "invalid.nar".to_owned(),
            format!("{}.nar", "A".repeat(64)),
            format!("{}.nar", "g".repeat(64)),
            format!("{}.nar", "0".repeat(63)),
            "..%2F..%2FREADME.md".to_owned(),
            "%2Fetc%2Fpasswd".to_owned(),
            "%5C..%5Csecret".to_owned(),
            format!("{}.nar%00", "0".repeat(64)),
            format!("{}.xz", "0".repeat(64)),
        ] {
            let url = format!("{}/nar/{name}", server.url);
            assert_eq!(
                client.get(&url).send().await.unwrap().status(),
                StatusCode::BAD_REQUEST,
                "GET {name}"
            );
            assert_eq!(
                client
                    .put(&url)
                    .header("Tailscale-User-Login", "alice@example.com")
                    .body(nar(b"content"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::BAD_REQUEST,
                "PUT {name}"
            );
        }
        assert_eq!(std::fs::read_dir(server.root.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn rejects_hash_mismatch_without_creating_or_overwriting_a_blob() {
        let server = server(1024).await;
        let bytes = nar(b"original");
        let url = format!("{}/nar/{}.nar", server.url, key(&bytes));
        let client = Client::new();
        let put = |body: Vec<u8>| {
            client
                .put(&url)
                .header("Tailscale-User-Login", "alice@example.com")
                .body(body)
        };
        assert_eq!(
            put(nar(b"wrong")).send().await.unwrap().status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(std::fs::read_dir(server.root.path()).unwrap().count(), 0);
        assert_eq!(
            put(bytes.clone()).send().await.unwrap().status(),
            StatusCode::CREATED
        );
        assert_eq!(
            put(nar(b"replacement")).send().await.unwrap().status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            client
                .get(&url)
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
                .as_ref(),
            bytes
        );
    }

    #[tokio::test]
    async fn uploads_and_downloads_exact_uncompressed_nar_bytes() {
        let server = server(1024).await;
        let bytes = nar(b"real file contents\n");
        let hash = key(&bytes);
        let url = format!("{}/nar/{hash}.nar", server.url);
        let client = Client::new();
        let response = client
            .put(&url)
            .header("Tailscale-User-Login", "alice@example.com")
            .body(bytes.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let response = client.get(&url).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["content-length"],
            bytes.len().to_string()
        );
        assert_eq!(response.bytes().await.unwrap().as_ref(), bytes);
        assert_eq!(
            std::fs::read(server.root.path().join(format!("{hash}.nar"))).unwrap(),
            bytes
        );
    }
}
