use builder::queue::{Job, Queue};
use std::os::unix::fs::PermissionsExt;

const DRV: &str = "/nix/store/y1a49lg2ja68djssigz14lhdxvxcwbxa-example.drv";
const OUT: &str = "/nix/store/y1a49lg2ja68djssigz14lhdxvxcwbxa-example";
const DEV: &str = "/nix/store/y1a49lg2ja68djssigz14lhdxvxcwbxa-example-dev";

fn private_dir(path: &std::path::Path) {
    std::fs::create_dir(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn cleanup_refuses_replaced_root_parent_even_if_job_roots_appear_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let roots = tmp.path().join("roots");
    let victim = tmp.path().join("victim");
    for path in [&spool, &roots, &victim] {
        private_dir(path);
    }
    let queue = Queue::open(&spool, &roots).unwrap();
    let job = Job::new(DRV.into(), vec![OUT.into()]).unwrap();
    let id = queue.enqueue(job.clone()).unwrap();
    builder::queue::atomic_json(&spool.join(&id).join("done.json"), &true).unwrap();
    std::fs::rename(&roots, tmp.path().join("retained-roots")).unwrap();
    std::os::unix::fs::symlink(&victim, &roots).unwrap();
    assert!(queue.cleanup(&id, &job).is_err());
    assert!(spool.join(&id).join("job.json").exists());
    assert!(
        tmp.path()
            .join("retained-roots")
            .join(&id)
            .join("0")
            .is_symlink()
    );
}

#[test]
fn root_retention_rejects_traversal_before_creating_anything() {
    let tmp = tempfile::tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let roots = tmp.path().join("roots");
    for path in [&spool, &roots] {
        private_dir(path);
    }
    let queue = Queue::open(&spool, &roots).unwrap();
    let job = Job::new(DRV.into(), vec![OUT.into()]).unwrap();
    assert!(queue.retain("../victim", &job).is_err());
    assert!(!tmp.path().join("victim").exists());
}

#[test]
fn cleanup_requires_durable_success_and_refuses_unexpected_roots() {
    let tmp = tempfile::tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let roots = tmp.path().join("roots");
    for path in [&spool, &roots] {
        private_dir(path);
    }
    let queue = Queue::open(&spool, &roots).unwrap();
    let job = Job::new(DRV.into(), vec![OUT.into()]).unwrap();
    let id = queue.enqueue(job.clone()).unwrap();
    assert!(queue.cleanup(&id, &job).is_err());
    builder::queue::atomic_json(&spool.join(&id).join("done.json"), &true).unwrap();
    let unexpected = roots.join(&id).join("other");
    std::os::unix::fs::symlink("/etc", &unexpected).unwrap();
    assert!(queue.cleanup(&id, &job).is_err());
    assert!(roots.join(&id).join("0").is_symlink());
    assert!(spool.join(&id).join("job.json").exists());
    std::fs::remove_file(unexpected).unwrap();
    // Simulate process death after one root was removed, but before job removal.
    std::fs::remove_file(roots.join(&id).join("0")).unwrap();
    queue.cleanup(&id, &job).unwrap();
    assert!(!roots.join(&id).exists());
    assert!(queue.jobs().unwrap().is_empty());
}

#[test]
fn retention_failure_keeps_the_manifest_for_recovery() {
    let tmp = tempfile::tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let roots = tmp.path().join("roots");
    for path in [&spool, &roots] {
        private_dir(path);
    }
    let queue = Queue::open(&spool, &roots).unwrap();
    std::fs::remove_dir(&roots).unwrap();
    assert!(
        queue
            .enqueue(Job::new(DRV.into(), vec![OUT.into(), DEV.into()]).unwrap())
            .is_err()
    );
    let jobs = queue.jobs().unwrap();
    assert_eq!(jobs.len(), 1);
    private_dir(&roots);
    queue.retain(&jobs[0].0, &jobs[0].1).unwrap();
    assert_eq!(
        std::fs::read_dir(roots.join(&jobs[0].0)).unwrap().count(),
        3
    );
}

#[test]
fn stale_atomic_temporary_file_cannot_block_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("retry.json");
    let stale = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(stale, b"partial").unwrap();
    builder::queue::atomic_json(&path, &serde_json::json!({"attempts": 2})).unwrap();
    let value: serde_json::Value = builder::queue::read_json(&path).unwrap();
    assert_eq!(value["attempts"], 2);
}

#[test]
fn symlink_job_manifest_is_not_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let roots = tmp.path().join("roots");
    for path in [&spool, &roots] {
        private_dir(path);
    }
    let queue = Queue::open(&spool, &roots).unwrap();
    let job = Job::new(DRV.into(), vec![OUT.into()]).unwrap();
    let id = queue.enqueue(job.clone()).unwrap();
    let target = tmp.path().join("external.json");
    builder::queue::atomic_json(&target, &job).unwrap();
    let manifest = spool.join(id).join("job.json");
    std::fs::remove_file(&manifest).unwrap();
    std::os::unix::fs::symlink(target, manifest).unwrap();
    assert!(queue.jobs().unwrap().is_empty());
}

#[test]
fn corrupt_job_is_retained_without_blocking_other_jobs() {
    let tmp = tempfile::tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let roots = tmp.path().join("roots");
    for path in [&spool, &roots] {
        private_dir(path);
    }
    let queue = Queue::open(&spool, &roots).unwrap();
    queue
        .enqueue(Job::new(DRV.into(), vec![OUT.into()]).unwrap())
        .unwrap();
    let bad = spool.join("1-1");
    private_dir(&bad);
    std::fs::write(bad.join("job.json"), b"broken").unwrap();
    assert_eq!(queue.jobs().unwrap().len(), 1);
    assert!(bad.join("job.json").exists());
}

#[test]
fn job_claim_is_nonblocking_and_excludes_hook_worker_overlap() {
    let tmp = tempfile::tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let roots = tmp.path().join("roots");
    for path in [&spool, &roots] {
        private_dir(path);
    }
    let queue = Queue::open(&spool, &roots).unwrap();
    let id = queue
        .enqueue(Job::new(DRV.into(), vec![OUT.into()]).unwrap())
        .unwrap();
    let first = queue.claim(&id).unwrap().unwrap();
    assert!(queue.claim(&id).unwrap().is_none());
    drop(first);
    assert!(queue.claim(&id).unwrap().is_some());
}

#[test]
fn tampered_root_directory_is_never_followed_or_cleaned() {
    let tmp = tempfile::tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let roots = tmp.path().join("roots");
    let victim = tmp.path().join("victim");
    for path in [&spool, &roots, &victim] {
        private_dir(path);
    }
    let queue = Queue::open(&spool, &roots).unwrap();
    let job = Job::new(DRV.into(), vec![OUT.into()]).unwrap();
    let id = queue.enqueue(job.clone()).unwrap();
    std::fs::remove_dir_all(roots.join(&id)).unwrap();
    std::os::unix::fs::symlink(&victim, roots.join(&id)).unwrap();
    assert!(
        queue.retain(&id, &job).is_err(),
        "root directory symlink followed"
    );
    assert!(std::fs::read_dir(&victim).unwrap().next().is_none());
    assert_eq!(queue.jobs().unwrap().len(), 1);
}

#[test]
fn unsafe_queue_or_root_directories_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let roots = tmp.path().join("roots");
    private_dir(&spool);
    private_dir(&roots);
    let alias = tmp.path().join("alias");
    std::os::unix::fs::symlink(&roots, &alias).unwrap();
    assert!(
        Queue::open(&spool, &alias).is_err(),
        "symlink GC root directory accepted"
    );
    std::fs::set_permissions(&roots, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(
        Queue::open(&spool, &roots).is_err(),
        "untrusted writable GC roots accepted"
    );
    std::fs::set_permissions(&roots, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(Queue::open(&spool, &spool.join("nested")).is_err());
    std::fs::set_permissions(&spool, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Queue::open(&spool, &roots).is_err(), "spool not private");
}

#[test]
fn persisted_multioutput_job_survives_reopen_with_all_gc_roots() {
    let tmp = tempfile::tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let roots = tmp.path().join("roots");
    private_dir(&spool);
    private_dir(&roots);
    let queue = Queue::open(&spool, &roots).unwrap();
    let id = queue
        .enqueue(Job::new(DRV.into(), vec![OUT.into(), DEV.into()]).unwrap())
        .unwrap();
    drop(queue);
    let queue = Queue::open(&spool, &roots).unwrap();
    let jobs = queue.jobs().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].0, id);
    assert_eq!(jobs[0].1.outputs, [OUT, DEV]);
    for (index, path) in [DRV, OUT, DEV].iter().enumerate() {
        assert_eq!(
            std::fs::read_link(roots.join(&id).join(index.to_string())).unwrap(),
            std::path::Path::new(path)
        );
    }
    assert_eq!(
        std::fs::metadata(spool.join(&id).join("job.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}
