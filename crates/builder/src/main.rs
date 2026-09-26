mod nix;

use std::{
    path::PathBuf,
    process::Command,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result};
use clap::Parser;
use nix::{build::Build, installable::Installable};
use serde::Serialize;

#[derive(Parser)]
struct Cli {
    /// Flake installable to build, for example nixpkgs#hello
    installable: String,
    #[arg(long, default_value = "http://127.0.0.1:3001")]
    registry_url: String,
    #[arg(long, default_value = "/tmp/repro2-demo-cache")]
    cache_dir: PathBuf,
    #[arg(long, default_value = "http://127.0.0.1:8001/")]
    cache_url: String,
}

#[derive(Serialize)]
struct BuildReport<'a> {
    drv_path: Option<&'a str>,
    output_name: &'a str,
    store_path_hash: &'a str,
    store_path: &'a str,
    nar_hash: &'a str,
    nar_size: i64,
    cache_url: &'a str,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let installable = Installable::try_from(cli.installable)
        .map_err(|_| anyhow::anyhow!("expected a flake installable such as nixpkgs#hello"))?;
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos();
    let store =
        PathBuf::from("/tmp").join(format!("repro2-build-store-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&store).context("failed to create an empty build store")?;
    let build = Build::new(installable)
        .rebuild(false)
        .substitute(true)
        .store(Some(store.clone()))
        .substituters(Some("https://cache.nixos.org/".to_owned()));
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    std::fs::create_dir_all(&cli.cache_dir).context("failed to create binary cache directory")?;
    let cache_dir = cli.cache_dir.canonicalize()?;
    let cache_store = format!("file://{}", cache_dir.display());

    println!("Store: {}", store.display());
    for result in build.run()? {
        let drv_path = result.drv_path.as_ref().and_then(|path| path.to_str());
        for (name, path) in &result.outputs {
            let info = build.path_info(path)?;
            let copy = Command::new("nix")
                .args(["copy", "--from"])
                .arg(&store)
                .args(["--to", &cache_store, path])
                .output()
                .context("failed to start nix copy")?;
            if !copy.status.success() {
                anyhow::bail!(
                    "nix copy failed: {}",
                    String::from_utf8_lossy(&copy.stderr).trim()
                );
            }
            println!(
                "{name}: {path}\n  NarHash: {}\n  NarSize: {}",
                info.nar_hash, info.nar_size
            );
            let store_path_hash = path
                .strip_prefix("/nix/store/")
                .and_then(|name| name.split_once('-'))
                .map(|(hash, _)| hash)
                .context("unexpected Nix store path")?;
            let report = BuildReport {
                drv_path,
                output_name: name,
                store_path_hash,
                store_path: path,
                nar_hash: &info.nar_hash,
                nar_size: info.nar_size.try_into()?,
                cache_url: &cli.cache_url,
            };
            http.post(format!(
                "{}/build-reports",
                cli.registry_url.trim_end_matches('/')
            ))
            .json(&report)
            .send()?
            .error_for_status()?;
            println!("Registered: {path}");
        }
    }

    Ok(())
}
