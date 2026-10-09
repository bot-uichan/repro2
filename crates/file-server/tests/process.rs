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

/// Observe the real startup syscalls rather than a mocked provisioning helper.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn binary_syncs_root_ancestry_before_binding_and_fails_closed_on_sync_error() {
    let dir = tempfile::tempdir().unwrap();
    let probe = dir.path().join("startup-sync.so");
    let compiled = Command::new("cc")
        .args(["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror"])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/startup_sync.c"))
        .args(["-o"])
        .arg(&probe)
        .arg("-ldl")
        .status()
        .unwrap();
    assert!(compiled.success(), "compile startup syscall probe");

    let root = dir.path().join("private/blobs");
    let expected: Vec<_> = root.ancestors().map(|path| path.to_owned()).collect();
    // Check first provisioning, every ancestor's failure, then a retry after
    // those failures with a now-existing hierarchy (existence is not durability).
    let failures = std::iter::once(None)
        .chain(expected.iter().map(Some))
        .chain(std::iter::once(None));
    for (attempt, failure) in failures.enumerate() {
        let trace = dir.path().join(format!("trace-{attempt}"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_file-server"));
        command
            .env("FILE_SERVER_ROOT", &root)
            .env("FILE_SERVER_BIND", "127.0.0.1:0")
            .env("LD_PRELOAD", &probe)
            .env("STARTUP_SYNC_TRACE", &trace)
            .env_remove("STARTUP_SYNC_FAIL")
            .stderr(std::process::Stdio::piped());
        if let Some(path) = failure {
            command.env("STARTUP_SYNC_FAIL", path);
        }
        let mut process = Process(command.spawn().unwrap());
        let mut finished = false;
        for _ in 0..200 {
            let events = std::fs::read_to_string(&trace).unwrap_or_default();
            if events.lines().any(|line| line == "BIND") || process.0.try_wait().unwrap().is_some()
            {
                finished = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(finished, "startup neither bound nor exited");
        let events = std::fs::read_to_string(&trace).unwrap_or_default();
        let actual: Vec<_> = events.lines().collect();
        if let Some(path) = failure {
            assert!(
                !actual.contains(&"BIND"),
                "startup bound despite fsync failure at {}: {actual:?}",
                path.display()
            );
            assert!(
                !process.0.wait().unwrap().success(),
                "fsync failure must fail startup"
            );
            let mut stderr = String::new();
            std::io::Read::read_to_string(process.0.stderr.as_mut().unwrap(), &mut stderr).unwrap();
            assert!(stderr.contains("sync FILE_SERVER_ROOT"));
            let stop = expected.iter().position(|entry| entry == path).unwrap();
            let wanted: Vec<_> = expected[..=stop]
                .iter()
                .map(|entry| entry.to_str().unwrap())
                .collect();
            assert_eq!(actual, wanted);
        } else {
            let mut wanted: Vec<_> = expected
                .iter()
                .map(|entry| entry.to_str().unwrap())
                .collect();
            wanted.push("BIND");
            assert_eq!(
                actual, wanted,
                "root and every ancestor must sync before bind"
            );
            assert!(process.0.try_wait().unwrap().is_none());
        }
    }
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
