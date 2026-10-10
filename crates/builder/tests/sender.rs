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
    let spool = tmp.join("spool");
    let roots = tmp.join("roots");
    for dir in [&spool, &roots] {
        fs::create_dir(dir).unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let queue = builder::queue::Queue::open(&spool, &roots).unwrap();
    let id = queue
        .enqueue(builder::queue::Job::new(DRV.into(), vec![OUT.into()]).unwrap())
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
    let result = run(&spool, &roots, &nix).arg("--once").output().unwrap();
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
