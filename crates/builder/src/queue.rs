use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, symlink},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub drv_path: String,
    pub outputs: Vec<String>,
}

impl Job {
    pub fn new(drv_path: String, outputs: Vec<String>) -> Result<Self> {
        ensure!(drv_path.ends_with(".drv"), "expected derivation path");
        validate_path(&drv_path)?;
        ensure!(!outputs.is_empty(), "empty OUT_PATHS");
        for path in &outputs {
            validate_path(path)?;
        }
        Ok(Self { drv_path, outputs })
    }
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit() || b == b'-'),
        "invalid queue id"
    );
    Ok(())
}

fn validate_path(path: &str) -> Result<()> {
    ensure!(path.starts_with("/nix/store/"), "expected /nix/store path");
    let _: nix_derivation::StorePath = path.parse()?;
    ensure!(!path[11..].contains('/'), "not a top-level store path");
    Ok(())
}

/// Ancestors may be root-owned; all must be real directories without untrusted writers.
/// This deliberately rejects /tmp, symlink aliases and group-writable deployment paths.
pub fn secure_dir(path: &Path, private: bool) -> Result<()> {
    ensure!(
        path.is_absolute()
            && !path.components().any(|c| matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )),
        "absolute normalized path required"
    );
    // SAFETY: geteuid has no arguments or memory preconditions.
    let uid = unsafe { libc::geteuid() };
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)
            .with_context(|| format!("directory {}", ancestor.display()))?;
        ensure!(
            metadata.is_dir()
                && (metadata.uid() == uid || metadata.uid() == 0)
                && metadata.mode() & 0o022 == 0,
            "unsafe directory {}",
            ancestor.display()
        );
        if ancestor == path && private {
            ensure!(
                metadata.uid() == uid && metadata.mode() & 0o077 == 0,
                "private owned directory required: {}",
                path.display()
            );
        }
    }
    Ok(())
}

pub struct Queue {
    pub spool: PathBuf,
    pub roots: PathBuf,
}

pub fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no memory preconditions.
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "private regular state file required"
    );
    Ok(serde_json::from_reader(file)?)
}

pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("missing parent")?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(&serde_json::to_vec(value)?)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)?;
    sync_dir(parent)
}

impl Queue {
    pub fn open(spool: &Path, roots: &Path) -> Result<Self> {
        ensure!(
            spool.is_absolute() && roots.is_absolute(),
            "absolute queue paths required"
        );
        ensure!(
            spool != roots && !spool.starts_with(roots) && !roots.starts_with(spool),
            "separate spool and GC roots required"
        );
        secure_dir(spool, true)?;
        secure_dir(roots, true)?;
        Ok(Self {
            spool: spool.into(),
            roots: roots.into(),
        })
    }

    pub fn claim(&self, id: &str) -> Result<Option<File>> {
        ensure!(
            !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit() || b == b'-'),
            "invalid queue id"
        );
        let dir = self.spool.join(id);
        secure_dir(&dir, true)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(dir.join("job.lock"))?;
        ensure!(file.metadata()?.is_file(), "job lock must be regular");
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn enqueue(&self, job: Job) -> Result<String> {
        let id = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        );
        let dir = self.spool.join(&id);
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
        sync_dir(&self.spool)?;
        let _claim = self.claim(&id)?.context("new job unexpectedly locked")?;
        atomic_json(&dir.join("job.json"), &job)?;
        self.retain(&id, &job)?;
        Ok(id)
    }

    pub fn retain(&self, id: &str, job: &Job) -> Result<()> {
        validate_id(id)?;
        secure_dir(&self.roots, true)?;
        let dir = self.roots.join(id);
        if !dir.exists() {
            fs::DirBuilder::new().mode(0o700).create(&dir)?;
            sync_dir(&self.roots)?;
        }
        secure_dir(&dir, true)?;
        for (index, path) in std::iter::once(&job.drv_path)
            .chain(&job.outputs)
            .enumerate()
        {
            let root = dir.join(index.to_string());
            match fs::read_link(&root) {
                Ok(target) => ensure!(target == Path::new(path), "GC root mismatch"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => symlink(path, root)?,
                Err(e) => return Err(e.into()),
            }
        }
        sync_dir(&dir)
    }

    /// Called only after a durable done marker; interrupted cleanup is replayable.
    pub fn cleanup(&self, id: &str, job: &Job) -> Result<()> {
        validate_id(id)?;
        let dir = self.spool.join(id);
        secure_dir(&dir, true)?;
        ensure!(
            read_json::<bool>(&dir.join("done.json"))?,
            "cleanup requires durable success"
        );
        let roots = self.roots.join(id);
        secure_dir(&self.roots, true)?;
        match fs::symlink_metadata(&roots) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::remove_dir_all(&dir)?;
                return sync_dir(&self.spool);
            }
            Err(error) => return Err(error.into()),
        }
        {
            secure_dir(&roots, true)?;
            let paths: Vec<_> = std::iter::once(&job.drv_path).chain(&job.outputs).collect();
            // Validate the whole directory before removing any root; never follow unexpected links.
            for entry in fs::read_dir(&roots)? {
                let entry = entry?;
                let index: usize = entry
                    .file_name()
                    .to_str()
                    .context("non-text root")?
                    .parse()?;
                ensure!(
                    paths
                        .get(index)
                        .is_some_and(|path| fs::read_link(entry.path()).ok().as_deref()
                            == Some(Path::new(path))),
                    "unexpected GC root"
                );
            }
            for entry in fs::read_dir(&roots)? {
                fs::remove_file(entry?.path())?;
            }
            sync_dir(&roots)?;
            fs::remove_dir(&roots)?;
            sync_dir(&self.roots)?;
        }
        fs::remove_dir_all(&dir)?;
        sync_dir(&self.spool)
    }

    pub fn jobs(&self) -> Result<Vec<(String, Job)>> {
        let mut jobs = Vec::new();
        for entry in fs::read_dir(&self.spool)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path().join("job.json");
            if !path.exists() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            let result = (|| {
                ensure!(
                    !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit() || b == b'-'),
                    "invalid queue id"
                );
                secure_dir(&entry.path(), true)?;
                let job: Job = read_json(&path)?;
                Job::new(job.drv_path, job.outputs)
            })();
            match result {
                Ok(job) => jobs.push((id, job)),
                Err(error) => eprintln!(
                    "CRITICAL repro2-sender corrupt job {id} retained for operator recovery: {error:#}"
                ),
            }
        }
        jobs.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(jobs)
    }
}
