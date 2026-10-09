use reqwest::{Client, StatusCode};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    process::{Child, Command},
    time::Duration,
};

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn start(root: &Path, max_bytes: usize) -> (Process, String) {
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = reservation.local_addr().unwrap();
    drop(reservation);
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_file-server"))
            .env("FILE_SERVER_ROOT", root)
            .env("FILE_SERVER_BIND", addr.to_string())
            .env("FILE_SERVER_MAX_UPLOAD_BYTES", max_bytes.to_string())
            .spawn()
            .unwrap(),
    );
    let url = format!("http://{addr}");
    let client = Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    let missing = format!("{url}/nar/{}.nar", "0".repeat(64));
    for _ in 0..100 {
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "file-server exited before becoming ready"
        );
        if let Ok(response) = client.get(&missing).send().await {
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            return (process, url);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("file-server did not become ready");
}

#[tokio::test]
async fn binary_creates_owner_only_storage_directory() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("private/blobs");
    let (_process, _url) = start(&root, 1024).await;
    assert_eq!(
        std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[tokio::test]
async fn binary_creates_configured_root_and_serves_persisted_nar_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("private/blobs");
    let mut nar = Vec::new();
    for field in [
        b"nix-archive-1".as_slice(),
        b"(",
        b"type",
        b"regular",
        b"contents",
        b"persistent real file\n",
        b")",
    ] {
        nar.extend_from_slice(&(field.len() as u64).to_le_bytes());
        nar.extend_from_slice(field);
        nar.resize(nar.len().next_multiple_of(8), 0);
    }
    let hash = format!("{:x}", Sha256::digest(&nar));
    let (process, url) = start(&root, nar.len()).await;
    let client = Client::new();
    let blob = format!("{url}/nar/{hash}.nar");
    assert_eq!(
        client
            .put(&blob)
            .header("Tailscale-User-Login", "alice@example.com")
            .body(nar.clone())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        std::fs::read(root.join(format!("{hash}.nar"))).unwrap(),
        nar
    );
    assert_eq!(
        client
            .put(&blob)
            .header("Tailscale-User-Login", "alice@example.com")
            .body(vec![0_u8; nar.len() + 1])
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    drop(process);
    let (_process, url) = start(&root, nar.len()).await;
    let blob = format!("{url}/nar/{hash}.nar");
    let response = client.head(&blob).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-length"], nar.len().to_string());
    assert!(response.bytes().await.unwrap().is_empty());
    assert_eq!(
        client
            .get(&blob)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        nar
    );
    assert_eq!(
        client
            .put(&blob)
            .header("Tailscale-User-Login", "bob@example.com")
            .body(nar)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}
