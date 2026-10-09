mod nix;

use std::{
    path::PathBuf,
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
    /// Existing HTTP(S) binary cache serving this output; does not publish the build.
    #[arg(long, value_parser = parse_cache_url)]
    cache_url: Option<String>,
}

fn parse_cache_url(value: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(value).map_err(|_| "invalid cache URL".to_owned())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
    {
        return Err("cache URL must be HTTP(S), without credentials, query or fragment".into());
    }
    Ok(value.to_owned())
}

#[derive(Serialize)]
struct BuildReport<'a> {
    drv_path: Option<&'a str>,
    output_name: &'a str,
    store_path_hash: &'a str,
    store_path: &'a str,
    nar_hash: &'a str,
    nar_size: i64,
    cache_url: Option<&'a str>,
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

    println!("Store: {}", store.display());
    for result in build.run()? {
        let drv_path = result.drv_path.as_ref().and_then(|path| path.to_str());
        for (name, path) in &result.outputs {
            let info = build.path_info(path)?;
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
                cache_url: cli.cache_url.as_deref(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_serializes_optional_cache_location() {
        let report = BuildReport {
            drv_path: None,
            output_name: "out",
            store_path_hash: "abc",
            store_path: "/nix/store/abc-example",
            nar_hash: "sha256-example",
            nar_size: 10,
            cache_url: None,
        };
        let json = serde_json::to_value(report).unwrap();
        assert!(json.as_object().unwrap().contains_key("cache_url"));
        assert!(json["cache_url"].is_null());
    }

    #[test]
    fn accepts_only_http_cache_publication_urls() {
        for cache_url in ["https://cache.example/nix", "http://127.0.0.1:8080/"] {
            assert!(
                Cli::try_parse_from(["builder", "nixpkgs#hello", "--cache-url", cache_url]).is_ok()
            );
        }
        for cache_url in [
            "file:///etc/passwd",
            "ftp://cache.example/",
            "not a URL",
            "https://user:password@cache.example/",
            "https://cache.example/#fragment",
        ] {
            assert!(
                Cli::try_parse_from(["builder", "nixpkgs#hello", "--cache-url", cache_url])
                    .is_err()
            );
        }
    }
}
