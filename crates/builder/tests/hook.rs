use std::{os::unix::fs::PermissionsExt, process::Command};

#[test]
fn deployment_wrapper_keeps_build_success_when_sender_cannot_launch() {
    let wrapper = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/repro2-post-build-hook");
    let result = Command::new("/usr/bin/bash")
        .arg(wrapper)
        .env("PATH", "/does-not-exist")
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    assert!(String::from_utf8_lossy(&result.stderr).contains("CRITICAL"));
}

#[test]
fn local_queue_errors_warn_without_failing_the_build() {
    let tmp = tempfile::tempdir().unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_repro2-sender"))
        .args([
            "--spool",
            tmp.path().join("missing").to_str().unwrap(),
            "hook",
        ])
        .env(
            "DRV_PATH",
            "/nix/store/y1a49lg2ja68djssigz14lhdxvxcwbxa-example.drv",
        )
        .env(
            "OUT_PATHS",
            "/nix/store/y1a49lg2ja68djssigz14lhdxvxcwbxa-example",
        )
        .output()
        .unwrap();
    assert!(result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("CRITICAL") && stderr.contains("NOT guaranteed"),
        "{stderr}"
    );
}

#[test]
fn hook_is_local_only_and_persists_every_output() {
    let tmp = tempfile::tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let roots = tmp.path().join("roots");
    for path in [&spool, &roots] {
        std::fs::create_dir(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let drv = "/nix/store/y1a49lg2ja68djssigz14lhdxvxcwbxa-example.drv";
    let out = "/nix/store/y1a49lg2ja68djssigz14lhdxvxcwbxa-example";
    let dev = "/nix/store/y1a49lg2ja68djssigz14lhdxvxcwbxa-dev";
    let result = Command::new(env!("CARGO_BIN_EXE_repro2-sender"))
        .args([
            "--spool",
            spool.to_str().unwrap(),
            "--gc-roots",
            roots.to_str().unwrap(),
            "hook",
        ])
        .env("DRV_PATH", drv)
        .env("OUT_PATHS", format!("{out} {dev}"))
        .env("PATH", "/does-not-exist")
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    let queue = builder::queue::Queue::open(&spool, &roots).unwrap();
    assert_eq!(queue.jobs().unwrap()[0].1.outputs, [out, dev]);
}
