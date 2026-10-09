use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[test]
fn refuses_unsafe_blob_base_urls_before_listening() {
    for value in [
        "",
        "file:///tmp/blobs",
        "ftp://example.org/",
        "https://user:pass@example.org/",
        "https://example.org/?query",
        "https://example.org/#fragment",
        "https://example.org/a/../b/",
        "https://example.org/%2e%2e/",
        "https://example.org/\\n",
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_gateway"))
            .env("REQUIRED_USERS", "2")
            .env("BLOB_BASE_URL", value)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        if status.is_none() {
            child.kill().unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            status.is_some_and(|s| !s.success()),
            "gateway accepted BLOB_BASE_URL={value:?}"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("BLOB_BASE_URL"));
    }
}

#[test]
fn refuses_missing_or_invalid_user_threshold_before_listening() {
    for value in [
        None,
        Some(""),
        Some("0"),
        Some("-1"),
        Some("abc"),
        Some("1.5"),
        Some("99999999999999999999999999999999"),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_gateway"));
        command
            .env_remove("REQUIRED_USERS")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if let Some(value) = value {
            command.env("REQUIRED_USERS", value);
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        if status.is_none() {
            child.kill().unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            status.is_some_and(|status| !status.success()),
            "gateway accepted REQUIRED_USERS={value:?}"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("REQUIRED_USERS"));
    }
}
