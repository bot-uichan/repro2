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

// Nix 2.34's version 4 envelope uses store basenames, unlike the legacy map.
fn derivation_output_names(
    value: &serde_json::Value,
    job: &Job,
) -> Result<BTreeMap<String, String>> {
    let root = value
        .as_object()
        .context("Nix derivation JSON must be an object")?;
    let current = root.contains_key("derivations") || root.contains_key("version");
    if current {
        ensure!(
            root.get("version").and_then(|v| v.as_u64()) == Some(4),
            "unsupported Nix derivation JSON version"
        );
    }
    let drv = if current {
        // Queue jobs already validate the full top-level /nix/store derivation path.
        let basename = job
            .drv_path
            .strip_prefix("/nix/store/")
            .context("requested derivation must be a /nix/store path")?;
        value.get("derivations").and_then(|v| v.get(basename))
    } else {
        value.get(&job.drv_path)
    }
    .context("requested Nix derivation missing")?;
    if current {
        ensure!(
            drv.get("version").and_then(|v| v.as_u64()) == Some(4),
            "unsupported Nix derivation entry version"
        );
    } else {
        ensure!(
            drv.get("version").is_none(),
            "unexpected legacy derivation version"
        );
    }
    let outputs = drv
        .get("outputs")
        .and_then(|v| v.as_object())
        .context("Nix derivation output map missing")?;
    let mut available = BTreeMap::new();
    for (name, output) in outputs {
        ensure!(!name.is_empty(), "empty Nix output name");
        let output = output
            .as_object()
            .context("malformed Nix output specification")?;
        if current {
            // Version 4's input-addressed variant serializes an explicit path.
            // Other variants (including fixed CA) are outside this parser's scope.
            ensure!(
                output.len() == 1 && output.contains_key("path"),
                "unsupported or unresolved Nix output specification"
            );
        }
        let path = output
            .get("path")
            .and_then(|v| v.as_str())
            .context("unresolved or malformed Nix output path")?;
        let full_path = if current {
            ensure!(
                !path.contains('/'),
                "Nix output path must be a store basename"
            );
            format!("/nix/store/{path}")
        } else {
            path.to_owned()
        };
        let basename = full_path
            .strip_prefix("/nix/store/")
            .context("Nix output must be a /nix/store path")?;
        ensure!(
            !basename.contains('/'),
            "Nix output must be a top-level store path"
        );
        let _: nix_derivation::StorePath = full_path
            .parse()
            .context("malformed Nix output store path")?;
        ensure!(
            available.insert(full_path, name.clone()).is_none(),
            "ambiguous Nix output path"
        );
    }
    job.outputs
        .iter()
        .map(|path| {
            let name = available
                .get(path)
                .context("queued output absent from derivation")?;
            Ok((path.clone(), name.clone()))
        })
        .collect()
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
        let names = derivation_output_names(&derivation, job)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DRV: &str = "/nix/store/y1a49lg2ja68djssigz14lhdxvxcwbxa-example.drv";
    const OUT: &str = "/nix/store/y1a49lg2ja68djssigz14lhdxvxcwbxa-example";

    fn job() -> Job {
        Job::new(DRV.into(), vec![OUT.into()]).unwrap()
    }

    fn current() -> serde_json::Value {
        json!({"version": 4, "derivations": {
            &DRV[11..]: {"version": 4, "outputs": {"out": {"path": &OUT[11..]}}}
        }})
    }

    #[test]
    fn rejects_unknown_or_malformed_derivation_versions() {
        for version in [
            json!(0),
            json!(1),
            json!(2),
            json!(3),
            json!(5),
            json!(null),
            json!("4"),
            json!(4.0),
            json!(true),
        ] {
            let mut value = current();
            value["version"] = version.clone();
            assert!(
                derivation_output_names(&value, &job()).is_err(),
                "root {version}"
            );
            let mut value = current();
            value["derivations"][&DRV[11..]]["version"] = version.clone();
            assert!(
                derivation_output_names(&value, &job()).is_err(),
                "drv {version}"
            );
        }
        let mut value = current();
        value.as_object_mut().unwrap().remove("version");
        assert!(derivation_output_names(&value, &job()).is_err());
        let mut value = current();
        value["derivations"][&DRV[11..]]
            .as_object_mut()
            .unwrap()
            .remove("version");
        assert!(derivation_output_names(&value, &job()).is_err());
        // Never fall back to a plausible legacy entry when a version is present.
        let value = json!({"version": 99, DRV: {"outputs": {"out": {"path": OUT}}}});
        assert!(derivation_output_names(&value, &job()).is_err());
        let value = json!({DRV: {"version": 99, "outputs": {"out": {"path": OUT}}}});
        assert!(derivation_output_names(&value, &job()).is_err());
    }

    #[test]
    fn rejects_unresolved_malformed_or_ambiguous_outputs() {
        for output in [
            json!(null),
            json!(7),
            json!({}),
            json!({"path": null}),
            json!({"path": ""}),
            json!({"path": "../example"}),
            json!({"path": "y1a49lg2ja68djssigz14lhdxvxcwbxa-example/subpath"}),
            json!({"path": "e1a49lg2ja68djssigz14lhdxvxcwbxa-example"}),
            json!({"path": OUT}),
            json!({"path": "y1a49lg2ja68djssigz14lhdxvxcwbxa-example-dev", "unexpected": true}),
            json!({"method": "nar", "hashAlgo": "sha256"}),
        ] {
            let mut value = current();
            // Even an unqueued output may not hide malformed/unresolved identities.
            value["derivations"][&DRV[11..]]["outputs"]["dev"] = output.clone();
            assert!(derivation_output_names(&value, &job()).is_err(), "{output}");
        }
        let mut value = current();
        value["derivations"][&DRV[11..]]["outputs"]["dev"] = json!({"path": &OUT[11..]});
        assert!(
            derivation_output_names(&value, &job()).is_err(),
            "ambiguous path"
        );
        let mut value = current();
        let output = value["derivations"][&DRV[11..]]["outputs"]["out"].take();
        value["derivations"][&DRV[11..]]["outputs"] = json!({"": output});
        assert!(
            derivation_output_names(&value, &job()).is_err(),
            "empty output name"
        );
        let value =
            json!({DRV: {"outputs": {"out": {"path": OUT}, "dev": {"path": "../example"}}}});
        assert!(
            derivation_output_names(&value, &job()).is_err(),
            "legacy traversal"
        );
    }

    #[test]
    fn maps_real_nix_234_multioutput_fixture_exactly() {
        let value: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/nix-2.34.8-derivation.json"))
                .unwrap();
        let drv = "/nix/store/51yi00crbyf51kxdp6gsxxzmd4f83dzc-repro2-json-probe.drv";
        let out = "/nix/store/limr2sn9jwp351gh26cgfb4c9664yv6y-repro2-json-probe";
        let dev = "/nix/store/vs00n6z39xsmq54kndj5spz74vp839zn-repro2-json-probe-dev";
        let job = Job::new(drv.into(), vec![out.into(), dev.into()]).unwrap();
        assert_eq!(
            derivation_output_names(&value, &job).unwrap(),
            BTreeMap::from([(out.into(), "out".into()), (dev.into(), "dev".into())])
        );
        let job = Job::new(drv.into(), vec![dev.into()]).unwrap();
        assert_eq!(
            derivation_output_names(&value, &job).unwrap(),
            BTreeMap::from([(dev.into(), "dev".into())])
        );
    }

    #[test]
    fn maps_legacy_multioutput_with_content_address_metadata() {
        let dev = "/nix/store/y1a49lg2ja68djssigz14lhdxvxcwbxa-example-dev";
        let value = json!({DRV: {"name": "example", "env": {}, "inputDrvs": {},
            "outputs": {"out": {"path": OUT, "method": "nar", "hashAlgo": "sha256", "hash": "6fc80dcc62179dbc12fc0b5881275898f93444833d21b89dfe5f7fbcbb1d0d62"},
                "dev": {"path": dev}}, "structuredAttrs": {"arbitrary": true}}});
        let job = Job::new(DRV.into(), vec![OUT.into(), dev.into()]).unwrap();
        assert_eq!(
            derivation_output_names(&value, &job).unwrap(),
            BTreeMap::from([(OUT.into(), "out".into()), (dev.into(), "dev".into())])
        );
    }

    #[test]
    fn rejects_wrong_drv_without_inferring_first_or_env_output() {
        let other = "y1a49lg2ja68djssigz14lhdxvxcwbxa-other.drv";
        let mut value = current();
        let drv = value["derivations"]
            .as_object_mut()
            .unwrap()
            .remove(&DRV[11..])
            .unwrap();
        value["derivations"][other] = drv;
        assert!(derivation_output_names(&value, &job()).is_err());
        let value = json!({format!("/nix/store/{other}"): {"outputs": {"out": {"path": OUT}}}});
        assert!(derivation_output_names(&value, &job()).is_err());
        let mut value = current();
        value["derivations"][&DRV[11..]]["outputs"]["out"]["path"] =
            "y1a49lg2ja68djssigz14lhdxvxcwbxa-other".into();
        value["derivations"][&DRV[11..]]["env"] = json!({"out": OUT});
        assert!(derivation_output_names(&value, &job()).is_err());
    }

    #[test]
    fn rejects_malformed_envelopes_and_output_maps() {
        for value in [
            json!(null),
            json!([]),
            json!({}),
            json!({"version": 4}),
            json!({"version": 4, "derivations": []}),
            json!({"version": 4, "derivations": {DRV: {"version": 4, "outputs": {"out": {"path": OUT}}}}}),
            json!({DRV: null}),
            json!({DRV: {"outputs": []}}),
            json!({DRV: {"outputs": {"out": {"path": &OUT[11..]}}}}),
        ] {
            assert!(derivation_output_names(&value, &job()).is_err(), "{value}");
        }
    }

    #[test]
    fn selects_requested_drv_even_when_another_drv_is_first() {
        let other = "00000000000000000000000000000000-other.drv";
        let mut value = current();
        value["derivations"][other] = json!({"version": 4,
            "outputs": {"wrong": {"path": &OUT[11..]}}});
        assert_eq!(
            derivation_output_names(&value, &job()).unwrap(),
            BTreeMap::from([(OUT.into(), "out".into())])
        );
        let value = json!({format!("/nix/store/{other}"): {"outputs": {"wrong": {"path": OUT}}},
            DRV: {"outputs": {"out": {"path": OUT}}}});
        assert_eq!(
            derivation_output_names(&value, &job()).unwrap(),
            BTreeMap::from([(OUT.into(), "out".into())])
        );
    }
}
