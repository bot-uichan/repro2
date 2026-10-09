use crate::queue::{Job, Queue, atomic_json, read_json, secure_dir, sync_dir};
use anyhow::{Context, Result, ensure};
use nar_metadata::{Artifact, Metadata};
use nix_derivation::NixHash;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

#[derive(clap::Args)]
pub struct Sender {
    #[arg(long, env = "REPRO2_REGISTRY_URL")]
    pub registry_url: String,
    #[arg(long, env = "REPRO2_BLOB_URL")]
    pub blob_url: String,
    #[arg(long, env = "REPRO2_NIX", default_value = "nix")]
    pub nix: PathBuf,
    #[arg(long, env = "REPRO2_STORE", default_value = "auto")]
    pub store: String,
    /// Debug mode: one scan, nonzero if any delivery fails. Normal operation is resident.
    #[arg(long)]
    pub once: bool,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Retry {
    attempts: u32,
    next_attempt: u64,
    last_error: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PathInfo {
    #[serde(default)]
    path: String,
    nar_hash: String,
    nar_size: u64,
    references: Vec<String>,
    deriver: Option<String>,
}

#[derive(Serialize)]
struct Report<'a> {
    drv_path: &'a str,
    output_name: &'a str,
    store_path_hash: &'a str,
    store_path: &'a str,
    nar_hash: &'a str,
    nar_size: i64,
    cache_url: Option<&'a str>,
    metadata: Metadata,
    artifact: Artifact,
}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn base_url(url: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(url)?;
    ensure!(
        matches!(parsed.scheme(), "http" | "https")
            && parsed.host_str().is_some()
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none(),
        "HTTP(S) endpoint without credentials/query/fragment required"
    );
    Ok(url.trim_end_matches('/').into())
}

impl Sender {
    pub fn run(&self, queue: &Queue) -> Result<()> {
        base_url(&self.registry_url)?;
        base_url(&self.blob_url)?;
        // Never share worker locks with enqueue: a slow network cannot block the hook.
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(queue.spool.join("worker.lock"))?;
        ensure!(lock.metadata()?.is_file(), "worker lock must be regular");
        lock.try_lock()
            .context("another resident sender holds this queue")?;
        STOP.store(false, Ordering::Relaxed);
        // SAFETY: the handler only stores an atomic bool and has the libc signal ABI.
        unsafe {
            libc::signal(libc::SIGTERM, stop as *const () as libc::sighandler_t);
            libc::signal(libc::SIGINT, stop as *const () as libc::sighandler_t);
        }
        let http = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        loop {
            let mut failed = false;
            for (id, job) in queue.jobs()? {
                if STOP.load(Ordering::Relaxed) {
                    break;
                }
                let dir = queue.spool.join(&id);
                let result = (|| {
                    let Some(_claim) = queue.claim(&id)? else {
                        return Ok(());
                    };
                    secure_dir(&dir, true)?;
                    if dir.join("done.json").exists() {
                        return queue.cleanup(&id, &job);
                    }
                    let retry: Retry = if dir.join("retry.json").exists() {
                        read_json(&dir.join("retry.json"))?
                    } else {
                        Retry::default()
                    };
                    if !self.once && retry.next_attempt > now()? {
                        return Ok(());
                    }
                    queue.retain(&id, &job)?;
                    match self.deliver(&http, &dir, &job) {
                        Ok(()) => {
                            atomic_json(&dir.join("done.json"), &true)?;
                            queue.cleanup(&id, &job)
                        }
                        Err(error) => {
                            let attempts = retry.attempts.saturating_add(1);
                            let next_attempt =
                                now()?.saturating_add((1u64 << attempts.min(8)).min(300));
                            atomic_json(
                                &dir.join("retry.json"),
                                &Retry {
                                    attempts,
                                    next_attempt,
                                    last_error: format!("{error:#}"),
                                },
                            )?;
                            Err(error)
                        }
                    }
                })();
                if let Err(error) = result {
                    failed = true;
                    eprintln!("repro2-sender retained {id}: {error:#}");
                }
            }
            if self.once {
                ensure!(!failed, "delivery failed; jobs and roots retained");
                return Ok(());
            }
            for _ in 0..10 {
                if STOP.load(Ordering::Relaxed) {
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }

    fn nix_command(&self) -> Command {
        let mut command = Command::new(&self.nix);
        command.args([
            "--extra-experimental-features",
            "nix-command",
            "--option",
            "post-build-hook",
            "",
            "--store",
            &self.store,
        ]);
        command
    }

    /// File-backed output avoids a full NAR in memory; also bounds hung Nix subprocesses.
    fn nix_output(&self, dir: &Path, args: &[&str]) -> Result<tempfile::NamedTempFile> {
        let mut output = tempfile::NamedTempFile::new_in(dir)?;
        let mut errors = tempfile::NamedTempFile::new_in(dir)?;
        let mut child = self
            .nix_command()
            .args(args)
            .stdin(Stdio::null())
            .stdout(output.as_file().try_clone()?)
            .stderr(errors.as_file().try_clone()?)
            .spawn()
            .context("start Nix reader")?;
        let deadline = Instant::now() + Duration::from_secs(120);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if STOP.load(Ordering::Relaxed) || Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("Nix reader interrupted or timed out");
            }
            thread::sleep(Duration::from_millis(50));
        };
        errors.seek(SeekFrom::Start(0))?;
        let mut message = String::new();
        errors.take(65536).read_to_string(&mut message)?;
        ensure!(status.success(), "Nix {args:?} failed: {message}");
        output.seek(SeekFrom::Start(0))?;
        Ok(output)
    }

    fn deliver(&self, http: &Client, dir: &Path, job: &Job) -> Result<()> {
        let registry = base_url(&self.registry_url)?;
        let blob = base_url(&self.blob_url)?;
        let derivation: serde_json::Value =
            serde_json::from_reader(self.nix_output(dir, &["derivation", "show", &job.drv_path])?)?;
        let outputs = derivation
            .get(&job.drv_path)
            .and_then(|v| v.get("outputs"))
            .and_then(|v| v.as_object())
            .context("Nix derivation output map missing")?;
        let mut names = BTreeMap::new();
        for path in &job.outputs {
            let name = outputs
                .iter()
                .find(|(_, v)| v.get("path").and_then(|v| v.as_str()) == Some(path))
                .map(|(name, _)| name.clone())
                .context("queued output absent from derivation")?;
            names.insert(path.clone(), name);
        }
        let args: Vec<&str> = ["path-info", "--json", "--recursive"]
            .into_iter()
            .chain(job.outputs.iter().map(String::as_str))
            .collect();
        let value: serde_json::Value = serde_json::from_reader(self.nix_output(dir, &args)?)?;
        let infos: BTreeMap<String, PathInfo> = if let Some(array) = value.as_array() {
            array
                .iter()
                .map(|v| {
                    let info: PathInfo = serde_json::from_value(v.clone())?;
                    Ok((info.path.clone(), info))
                })
                .collect::<Result<_>>()?
        } else {
            serde_json::from_value(value)?
        };
        for path in &job.outputs {
            ensure!(
                infos.contains_key(path),
                "Nix path-info omitted output {path}"
            );
        }
        let mut artifacts = BTreeMap::new();
        for (path, info) in &infos {
            let store_hash = path
                .strip_prefix("/nix/store/")
                .and_then(|v| v.split_once('-'))
                .context("unexpected store path")?
                .0;
            let mut metadata = Metadata {
                references: info.references.clone(),
                deriver: info.deriver.clone().filter(|v| v != "unknown-deriver"),
            };
            metadata.canonicalize();
            nar_metadata::validate_candidate(
                Some(&metadata),
                None,
                path,
                store_hash,
                &info.nar_hash,
                info.nar_size.try_into()?,
            )?;
            for reference in &metadata.references {
                ensure!(
                    infos.contains_key(reference),
                    "Nix recursive closure omitted reference {reference}"
                );
            }
            let mut nar = self.nix_output(dir, &["store", "dump-path", path])?;
            let mut hash = Sha256::new();
            let mut buffer = [0u8; 65536];
            let mut size = 0u64;
            loop {
                let count = nar.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                hash.update(&buffer[..count]);
                size += count as u64;
            }
            let key = format!("{:x}", hash.finalize());
            let expected: NixHash = info.nar_hash.parse()?;
            let actual: NixHash = format!("sha256:{key}").parse()?;
            ensure!(
                actual == expected && size == info.nar_size,
                "dump-path does not match Nix path-info for {path}"
            );
            let artifact = Artifact {
                file_hash: key.clone(),
                file_size: size,
                compression: "none".into(),
            };
            let url = format!("{blob}/nar/{key}.nar");
            nar.seek(SeekFrom::Start(0))?;
            http.put(&url)
                .body(nar.as_file().try_clone()?)
                .send()?
                .error_for_status()?;
            // The immutable, hash-validated backend is trusted; read back the exact key and size.
            let head = http.head(&url).send()?.error_for_status()?;
            ensure!(
                head.headers()
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    == Some(size),
                "uploaded blob readback size mismatch"
            );
            artifacts.insert(path.clone(), (metadata, artifact));
        }
        // Only paths actually named by this build's hook cast reports. Closure copies never do.
        for (path, name) in names {
            let info = &infos[&path];
            let (metadata, artifact) =
                artifacts.remove(&path).context("output artifact missing")?;
            let store_path_hash = path
                .strip_prefix("/nix/store/")
                .and_then(|v| v.split_once('-'))
                .context("store hash missing")?
                .0;
            let report = Report {
                drv_path: &job.drv_path,
                output_name: &name,
                store_path_hash,
                store_path: &path,
                nar_hash: &info.nar_hash,
                nar_size: info.nar_size.try_into()?,
                cache_url: None,
                metadata,
                artifact,
            };
            http.post(format!("{registry}/build-reports"))
                .json(&report)
                .send()?
                .error_for_status()?;
            let records: Vec<serde_json::Value> = http
                .get(format!("{registry}/nar-info/{store_path_hash}"))
                .send()?
                .error_for_status()?
                .json()?;
            let canonical = info.nar_hash.parse::<NixHash>()?.to_sri_string();
            let mut expected = serde_json::to_value(&report)?;
            expected["nar_hash"] = canonical.into();
            ensure!(
                records.iter().any(|row| expected
                    .as_object()
                    .unwrap()
                    .iter()
                    .all(|(key, value)| row.get(key) == Some(value))
                    && row
                        .get("user_id")
                        .and_then(|v| v.as_str())
                        .is_some_and(|v| !v.is_empty())),
                "registry readback missing authenticated candidate"
            );
        }
        sync_dir(dir)?;
        Ok(())
    }
}
