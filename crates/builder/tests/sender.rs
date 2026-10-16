use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

const DRV: &str = "/nix/store/51yi00crbyf51kxdp6gsxxzmd4f83dzc-repro2-json-probe.drv";
const OUT: &str = "/nix/store/limr2sn9jwp351gh26cgfb4c9664yv6y-repro2-json-probe";

fn setup(tmp: &Path) -> (std::path::PathBuf, std::path::PathBuf, String) {
    setup_job(tmp, DRV, OUT)
}
fn setup_job(tmp: &Path, drv: &str, out: &str) -> (std::path::PathBuf, std::path::PathBuf, String) {
    let spool = tmp.join("spool");
    let roots = tmp.join("roots");
    for dir in [&spool, &roots] {
        fs::create_dir(dir).unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let queue = builder::queue::Queue::open(&spool, &roots).unwrap();
    let id = queue
        .enqueue(builder::queue::Job::new(drv.into(), vec![out.into()]).unwrap())
        .unwrap();
    (spool, roots, id)
}

fn fake_nix(tmp: &Path, body: &str) -> std::path::PathBuf {
    let path = tmp.join("fake-nix");
    fs::write(&path, format!("#!/usr/bin/bash\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn run(spool: &Path, roots: &Path, nix: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_repro2-sender"));
    command.args([
        "--spool",
        spool.to_str().unwrap(),
        "--gc-roots",
        roots.to_str().unwrap(),
        "run",
        "--registry-url",
        "http://127.0.0.1:9",
        "--blob-url",
        "http://127.0.0.1:9",
        "--nix",
        nix.to_str().unwrap(),
    ]);
    command
}

#[test]
fn inconsistent_nix_dump_retains_job_and_roots_without_publication() {
    inconsistent_nix_dump_retains_job(false, false);
}

#[test]
fn current_derivation_json_reaches_dump_validation_and_retains_job() {
    inconsistent_nix_dump_retains_job(true, false);
}

#[test]
fn legacy_derivation_with_array_path_info_reaches_dump_validation() {
    inconsistent_nix_dump_retains_job(false, true);
}

#[test]
fn current_derivation_with_array_path_info_reaches_dump_validation() {
    inconsistent_nix_dump_retains_job(true, true);
}

fn inconsistent_nix_dump_retains_job(current: bool, array_path_info: bool) {
    let tmp = tempfile::tempdir().unwrap();
    let (spool, roots, id) = setup(tmp.path());
    let derivation = if current {
        serde_json::from_str(include_str!("fixtures/nix-2.34.8-derivation.json")).unwrap()
    } else {
        serde_json::json!({DRV: {"outputs": {"out": {"path": OUT}}}})
    };
    let info = serde_json::json!({"narHash": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        "narSize": 7, "references": [], "deriver": DRV});
    let path_info = if array_path_info {
        let mut info = info;
        info["path"] = OUT.into();
        serde_json::json!([info])
    } else {
        serde_json::json!({OUT: info})
    };
    let script = format!(
        r#"case "$*" in
*"derivation show"*) printf '%s' '{derivation}' ;;
*"path-info"*) printf '%s' '{path_info}' ;;
*"dump-path"*) printf invalid ;;
esac"#
    );
    let nix = fake_nix(tmp.path(), &script);
    let peer = Peer::new(false, 200, Duration::ZERO);
    let result = transfer_run(&spool, &roots, &nix, &peer)
        .arg("--once")
        .output()
        .unwrap();
    assert!(!result.status.success());
    let retry: serde_json::Value =
        builder::queue::read_json(&spool.join(&id).join("retry.json")).unwrap();
    assert!(
        retry["last_error"]
            .as_str()
            .unwrap()
            .contains("dump-path does not match Nix path-info"),
        "{retry}"
    );
    assert!(spool.join(&id).join("job.json").exists());
    assert!(!spool.join(&id).join("done.json").exists());
    assert_eq!(retry["attempts"], 1);
    assert_eq!(fs::read_dir(roots.join(&id)).unwrap().count(), 2);
    assert_eq!(
        fs::read_link(roots.join(&id).join("0")).unwrap(),
        Path::new(DRV)
    );
    assert_eq!(
        fs::read_link(roots.join(&id).join("1")).unwrap(),
        Path::new(OUT)
    );
}

// Local HTTP peer exercises the actual blocking client and sender process.
struct Peer {
    url: String,
    calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Peer {
    fn new(present: bool, head_status: u16, delay: Duration) -> Self {
        Self::configured(present, head_status, delay, 200, Some(9))
    }
    fn configured(
        present: bool,
        head_status: u16,
        delay: Duration,
        put_status: u16,
        head_length: Option<usize>,
    ) -> Self {
        Self::with_registry(
            present,
            head_status,
            delay,
            put_status,
            head_length,
            (200, 200, true, "X-Repro2-Durable: file-and-parent-v1\r\n"),
        )
    }
    fn with_registry(
        present: bool,
        head_status: u16,
        delay: Duration,
        put_status: u16,
        head_length: Option<usize>,
        registry: (u16, u16, bool, &'static str),
    ) -> Self {
        use std::io::{BufRead, Read, Write};
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (seen, done) = (calls.clone(), stop.clone());
        let handle = thread::spawn(move || {
            let mut stored = present;
            let mut reports = Vec::<serde_json::Value>::new();
            while !done.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                seen.lock().unwrap().push(first.trim().into());
                let mut size = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':')
                        && key.eq_ignore_ascii_case("content-length")
                    {
                        size = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; size];
                reader.read_exact(&mut body).unwrap();
                let (status, response, length) = if first.starts_with("HEAD ") {
                    let status = if head_status != 200 {
                        head_status
                    } else if stored {
                        200
                    } else {
                        404
                    };
                    (
                        status,
                        String::new(),
                        if stored { head_length.unwrap_or(0) } else { 0 },
                    )
                } else if first.starts_with("PUT ") {
                    assert_eq!(body, b"nar bytes");
                    thread::sleep(delay);
                    // A redirecting peer may have mutated storage; that must not
                    // turn its redirect into a successful upload acknowledgement.
                    stored = (200..400).contains(&put_status);
                    (put_status, String::new(), 0)
                } else if first.starts_with("POST ") {
                    let mut report: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    report["nar_hash"] = report["nar_hash"]
                        .as_str()
                        .unwrap()
                        .parse::<nix_derivation::NixHash>()
                        .unwrap()
                        .to_sri_string()
                        .into();
                    report["user_id"] = "test-builder".into();
                    if registry.2 {
                        reports.push(report);
                    }
                    (registry.0, String::new(), 0)
                } else {
                    let body = serde_json::to_string(&reports).unwrap();
                    (registry.1, body.clone(), body.len())
                };
                let length_header = if first.starts_with("HEAD ") && head_length.is_none() {
                    String::new()
                } else {
                    format!("Content-Length: {length}\r\n")
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} test\r\n{length_header}{}Content-Type: application/json\r\nConnection: close\r\n\r\n{response}",
                    registry.3
                );
            }
        });
        Self {
            url,
            calls,
            stop,
            thread: Some(handle),
        }
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn transfer_nix(tmp: &Path) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};
    let hash = format!("sha256:{:x}", Sha256::digest(b"nar bytes"));
    let other = DRV.replace("probe.drv", "other.drv");
    let derivation = serde_json::json!({DRV: {"outputs": {"out": {"path": OUT}}}, other: {"outputs": {"out": {"path": OUT}}}});
    let info =
        serde_json::json!({OUT: {"narHash": hash, "narSize": 9, "references": [], "deriver": DRV}});
    fake_nix(
        tmp,
        &format!(
            r#"case "$*" in
*"derivation show"*) printf '%s' '{derivation}' ;;
*"path-info"*) printf '%s' '{info}' ;;
*"dump-path"*) printf dump >> '{}'; printf 'nar bytes' ;;
esac"#,
            tmp.join("dumps").display()
        ),
    )
}

fn transfer_run(spool: &Path, roots: &Path, nix: &Path, peer: &Peer) -> Command {
    let original = run(spool, roots, nix);
    let mut command = Command::new(original.get_program());
    command.args(original.get_args().map(|arg| {
        if arg == "http://127.0.0.1:9" {
            std::ffi::OsStr::new(&peer.url)
        } else {
            arg
        }
    }));
    command
}

#[test]
fn legacy_or_unknown_durable_head_requires_validated_put_ack_before_release() {
    for marker in [
        "",
        "X-Repro2-Durable: file-and-parent-v0\r\n",
        "X-Repro2-Durable: file-and-parent-v1\r\nX-Repro2-Durable: file-and-parent-v1\r\n",
    ] {
        for put_status in [503, 200] {
            let tmp = tempfile::tempdir().unwrap();
            let (spool, roots, id) = setup(tmp.path());
            let nix = transfer_nix(tmp.path());
            let peer = Peer::with_registry(
                true,
                200,
                Duration::ZERO,
                put_status,
                Some(9),
                (200, 200, true, marker),
            );
            let output = transfer_run(&spool, &roots, &nix, &peer)
                .arg("--once")
                .output()
                .unwrap();
            assert_eq!(
                output.status.success(),
                put_status == 200,
                "unmarked HEAD bypassed failed PUT acknowledgement"
            );
            assert_eq!(
                fs::read(tmp.path().join("dumps")).unwrap(),
                b"dump",
                "legacy HEAD skipped hash-validated dump"
            );
            let calls = peer.calls.lock().unwrap();
            assert_eq!(
                calls.iter().filter(|line| line.starts_with("PUT ")).count(),
                1
            );
            assert_eq!(
                calls.iter().any(|line| line.starts_with("POST ")),
                put_status == 200
            );
            assert_eq!(spool.join(id).join("job.json").exists(), put_status != 200);
            assert_eq!(
                fs::read_dir(roots).unwrap().count(),
                usize::from(put_status != 200)
            );
        }
    }
}

#[test]
fn preexisting_exact_blob_skips_dump_and_put_but_acknowledges_registry() {
    let tmp = tempfile::tempdir().unwrap();
    let (spool, roots, _) = setup(tmp.path());
    let nix = transfer_nix(tmp.path());
    let peer = Peer::new(true, 200, Duration::ZERO);
    let output = transfer_run(&spool, &roots, &nix, &peer)
        .arg("--once")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !tmp.path().join("dumps").exists(),
        "already stored blob was dumped again"
    );
    let calls = peer.calls.lock().unwrap();
    assert!(!calls.iter().any(|line| line.starts_with("PUT ")));
    assert!(calls.iter().any(|line| line.starts_with("POST ")));
    assert_eq!(fs::read_dir(roots).unwrap().count(), 0);
}

#[test]
fn slow_upload_uses_separate_transfer_deadline() {
    let tmp = tempfile::tempdir().unwrap();
    let (spool, roots, _) = setup(tmp.path());
    let nix = transfer_nix(tmp.path());
    let peer = Peer::new(false, 200, Duration::from_millis(1500));
    let output = transfer_run(&spool, &roots, &nix, &peer)
        .args([
            "--once",
            "--metadata-timeout-secs",
            "1",
            "--transfer-timeout-secs",
            "3",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(tmp.path().join("dumps")).unwrap(), b"dump");
    assert_eq!(fs::read_dir(roots).unwrap().count(), 0);
}

#[test]
fn shared_failed_blob_is_not_dumped_or_uploaded_for_each_job() {
    let tmp = tempfile::tempdir().unwrap();
    let (spool, roots, _) = setup(tmp.path());
    let queue = builder::queue::Queue::open(&spool, &roots).unwrap();
    queue
        .enqueue(
            builder::queue::Job::new(DRV.replace("probe.drv", "other.drv"), vec![OUT.into()])
                .unwrap(),
        )
        .unwrap();
    let nix = transfer_nix(tmp.path());
    let peer = Peer::configured(false, 200, Duration::ZERO, 503, Some(9));
    let output = transfer_run(&spool, &roots, &nix, &peer)
        .arg("--once")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        fs::read(tmp.path().join("dumps")).unwrap(),
        b"dump",
        "shared blob failure caused duplicate dump"
    );
    let calls = peer.calls.lock().unwrap();
    assert_eq!(
        calls.iter().filter(|line| line.starts_with("PUT ")).count(),
        1
    );
    assert_eq!(fs::read_dir(&roots).unwrap().count(), 2);
    for entry in fs::read_dir(roots).unwrap() {
        assert_eq!(fs::read_dir(entry.unwrap().path()).unwrap().count(), 2);
    }
}

#[test]
fn unsupported_output_retains_roots_with_hourly_not_hot_retry() {
    let tmp = tempfile::tempdir().unwrap();
    let (spool, roots, id) = setup(tmp.path());
    let value = serde_json::json!({"version":4,"derivations":{&DRV[11..]:{"version":4,"outputs":{"out":{"method":"nar","hashAlgo":"sha256"}}}}});
    let nix = fake_nix(tmp.path(), &format!("printf '%s' '{value}'"));
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let output = run(&spool, &roots, &nix).arg("--once").output().unwrap();
    assert!(!output.status.success());
    let retry: serde_json::Value =
        builder::queue::read_json(&spool.join(&id).join("retry.json")).unwrap();
    assert!(
        retry["next_attempt"].as_u64().unwrap() >= before + 3600,
        "unsupported schema retried too soon: {retry}"
    );
    assert!(spool.join(id).join("job.json").exists());
    assert_eq!(fs::read_dir(roots).unwrap().count(), 1);
}

#[test]
fn resident_does_not_hot_scan_retained_or_corrupt_queue_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let (spool, roots, id) = setup(tmp.path());
    let corrupt = spool.join("999-999");
    fs::create_dir(&corrupt).unwrap();
    fs::set_permissions(&corrupt, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(corrupt.join("job.json"), "{}").unwrap();
    let nix = fake_nix(tmp.path(), "printf '{}'");
    let mut worker = Process(
        run(&spool, &roots, &nix)
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !spool.join(&id).join("retry.json").exists() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(2200));
    // SAFETY: the PID belongs to the child owned by this test.
    unsafe {
        libc::kill(worker.0.id() as i32, libc::SIGTERM);
    }
    worker.0.wait().unwrap();
    let mut logs = String::new();
    std::io::Read::read_to_string(&mut worker.0.stderr.take().unwrap(), &mut logs).unwrap();
    assert_eq!(logs.matches("corrupt job").count(), 1, "hot scans: {logs}");
}

#[test]
fn concrete_named_fixed_ca_output_uses_nix_resolver_and_verified_blob() {
    use sha2::{Digest, Sha256};
    let drv = "/nix/store/dwmaawkaqz3zlq5m1jvnjf7b4d59kpbm-repro2-fixed-ca-probe.drv";
    let out = "/nix/store/f7s0y03cj76fjscf4j29axywp3vqfncg-repro2-fixed-ca-probe";
    let tmp = tempfile::tempdir().unwrap();
    let (spool, roots, _) = setup_job(tmp.path(), drv, out);
    let derivation = include_str!("fixtures/nix-2.34.8-fixed-ca.json");
    let resolved = serde_json::json!({"out": out});
    let info = serde_json::json!({out: {"narHash":format!("sha256:{:x}", Sha256::digest(b"nar bytes")), "narSize":9,"references":[],"deriver":drv}});
    let nix = fake_nix(
        tmp.path(),
        &format!(
            r#"case "$*" in
*"derivation show"*) printf '%s' '{derivation}' ;;
*"eval --json"*) printf '%s' '{resolved}' ;;
*"path-info"*) printf '%s' '{info}' ;;
*"dump-path"*) exit 99 ;;
esac"#
        ),
    );
    let peer = Peer::new(true, 200, Duration::ZERO);
    let output = transfer_run(&spool, &roots, &nix, &peer)
        .arg("--once")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_dir(roots).unwrap().count(), 0);
}

#[test]
fn untrusted_head_responses_never_dump_upload_or_release_roots() {
    for (status, size) in [
        (200, Some(8)),
        (200, None),
        (302, Some(9)),
        (401, Some(9)),
        (403, Some(9)),
        (500, Some(9)),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let (spool, roots, id) = setup(tmp.path());
        let nix = transfer_nix(tmp.path());
        let peer = Peer::configured(true, status, Duration::ZERO, 200, size);
        let output = transfer_run(&spool, &roots, &nix, &peer)
            .arg("--once")
            .output()
            .unwrap();
        assert!(!output.status.success(), "accepted HEAD {status}/{size:?}");
        assert!(!tmp.path().join("dumps").exists());
        assert!(spool.join(id).join("job.json").exists());
        assert_eq!(fs::read_dir(roots).unwrap().count(), 1);
        assert_eq!(peer.calls.lock().unwrap().len(), 1);
    }
}

#[test]
fn upload_readback_size_mismatch_keeps_job_and_roots() {
    let tmp = tempfile::tempdir().unwrap();
    let (spool, roots, id) = setup(tmp.path());
    let nix = transfer_nix(tmp.path());
    let peer = Peer::configured(false, 200, Duration::ZERO, 200, Some(8));
    let output = transfer_run(&spool, &roots, &nix, &peer)
        .arg("--once")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("readback size mismatch"));
    assert!(spool.join(id).join("job.json").exists());
    assert_eq!(fs::read_dir(roots).unwrap().count(), 1);
    assert!(
        !peer
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.starts_with("POST "))
    );
}

#[test]
fn transfer_timeout_bounds_a_stalled_upload_and_retains_roots() {
    let tmp = tempfile::tempdir().unwrap();
    let (spool, roots, id) = setup(tmp.path());
    let nix = transfer_nix(tmp.path());
    let peer = Peer::new(false, 200, Duration::from_secs(3));
    let started = Instant::now();
    let output = transfer_run(&spool, &roots, &nix, &peer)
        .args(["--once", "--transfer-timeout-secs", "1"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(started.elapsed() < Duration::from_millis(2500));
    assert!(spool.join(id).join("job.json").exists());
    assert_eq!(fs::read_dir(roots).unwrap().count(), 1);
}

#[test]
fn default_transfer_survives_old_thirty_second_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let (spool, roots, _) = setup(tmp.path());
    let nix = transfer_nix(tmp.path());
    let peer = Peer::new(false, 200, Duration::from_secs(31));
    let output = transfer_run(&spool, &roots, &nix, &peer)
        .arg("--once")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_dir(roots).unwrap().count(), 0);
}

#[test]
fn redirect_is_not_upload_acknowledgement_even_if_blob_appears() {
    let tmp = tempfile::tempdir().unwrap();
    let (spool, roots, id) = setup(tmp.path());
    let nix = transfer_nix(tmp.path());
    let peer = Peer::configured(false, 200, Duration::ZERO, 302, Some(9));
    let output = transfer_run(&spool, &roots, &nix, &peer)
        .arg("--once")
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "redirect was accepted as PUT acknowledgement"
    );
    assert!(spool.join(id).join("job.json").exists());
    assert_eq!(fs::read_dir(roots).unwrap().count(), 1);
    assert!(
        !peer
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.starts_with("POST "))
    );
}

#[test]
fn registry_redirect_or_missing_readback_keeps_preexisting_blob_job_rooted() {
    for registry in [(302, 200, true), (200, 302, true), (200, 200, false)] {
        let tmp = tempfile::tempdir().unwrap();
        let (spool, roots, id) = setup(tmp.path());
        let nix = transfer_nix(tmp.path());
        let peer = Peer::with_registry(
            true,
            200,
            Duration::ZERO,
            200,
            Some(9),
            (
                registry.0,
                registry.1,
                registry.2,
                "X-Repro2-Durable: file-and-parent-v1\r\n",
            ),
        );
        let output = transfer_run(&spool, &roots, &nix, &peer)
            .arg("--once")
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "registry response {registry:?} accepted"
        );
        assert!(!tmp.path().join("dumps").exists());
        assert!(spool.join(id).join("job.json").exists());
        assert_eq!(fs::read_dir(roots).unwrap().count(), 1);
    }
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn sigterm_interrupts_hung_nix_reader_and_keeps_the_job() {
    let tmp = tempfile::tempdir().unwrap();
    let (spool, roots, id) = setup(tmp.path());
    let started = tmp.path().join("started");
    let nix = fake_nix(
        tmp.path(),
        &format!(
            "printf started > '{}'\nwhile :; do :; done",
            started.display()
        ),
    );
    let mut worker = Process(run(&spool, &roots, &nix).spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !started.exists() {
        assert!(worker.0.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    // A second worker fails rather than racing the queue or blocking the build hook.
    let duplicate = run(&spool, &roots, &nix).arg("--once").output().unwrap();
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("another resident sender"));
    // SAFETY: send a standard signal to the child process we own.
    assert_eq!(
        unsafe { libc::kill(worker.0.id() as i32, libc::SIGTERM) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = worker.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "SIGTERM did not cancel Nix reader"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(spool.join(&id).join("job.json").exists());
    assert!(spool.join(&id).join("retry.json").exists());
    assert_eq!(fs::read_dir(roots.join(&id)).unwrap().count(), 2);
}
