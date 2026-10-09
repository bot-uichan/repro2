use anyhow::Context;
use file_server::{config::Config, router};
use std::{env, path::PathBuf};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let bind = optional_env("FILE_SERVER_BIND")?;
    let limit = optional_env("FILE_SERVER_MAX_UPLOAD_BYTES")?;
    let config = Config::from_values(
        env::var_os("FILE_SERVER_ROOT").map(PathBuf::from),
        bind.as_deref(),
        limit.as_deref(),
    )?;
    let directory = config.root.clone();
    tokio::task::spawn_blocking(move || {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
    })
    .await?
    .context("create FILE_SERVER_ROOT")?;
    let root = tokio::fs::canonicalize(&config.root)
        .await
        .context("resolve FILE_SERVER_ROOT")?;
    let durable_root = root.clone();
    tokio::task::spawn_blocking(move || {
        // Persist every directory entry up to the filesystem root before bind.
        // Include existing ancestors: a previous startup may have created them
        // but failed fsync, so existence alone does not establish durability.
        for directory in durable_root.ancestors() {
            std::fs::File::open(directory)?.sync_all()?;
        }
        Ok::<_, std::io::Error>(())
    })
    .await?
    .context("sync FILE_SERVER_ROOT ancestry")?;
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .context("bind file-server")?;
    eprintln!("file-server listening on {}", listener.local_addr()?);
    axum::serve(listener, router(root, config.max_upload_bytes))
        .await
        .context("serve NAR blobs")
}

fn optional_env(name: &str) -> anyhow::Result<Option<String>> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {name}")),
    }
}
