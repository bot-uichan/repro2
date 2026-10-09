mod cache;
mod nar_record;
mod store_path_hash;
mod templates;

use anyhow::Context;
use std::num::NonZeroUsize;

use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use reqwest::Client;
use templates::narinfo::{NarInfoPath, NarInfoResponse};
use thiserror::Error;

use crate::{
    cache::get::{CacheServer, FetchNarInfoError},
    nar_record::{NarRecord, RegistryNarRecord},
};

#[derive(Clone)]
struct AppState {
    registry_url: String,
    http: Client,
    required_users: NonZeroUsize,
    blob_base_url: Option<url::Url>,
}

#[derive(Debug, Error)]
enum NarInfoError {
    #[error("narinfo not found")]
    NotFound,
    #[error("invalid cache URL")]
    Url(#[from] url::ParseError),
    #[error("failed to fetch upstream narinfo")]
    Upstream(#[from] FetchNarInfoError),
    #[error("failed to fetch registry record")]
    Registry(#[from] reqwest::Error),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for NarInfoError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Upstream(FetchNarInfoError::Request(error))
                if error.status() == Some(StatusCode::NOT_FOUND) =>
            {
                StatusCode::NOT_FOUND
            }
            Self::Upstream(_) | Self::Registry(_) => StatusCode::BAD_GATEWAY,
            Self::Url(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        if status.is_server_error() {
            eprintln!("narinfo error: {self:#}");
        }

        status.into_response()
    }
}

fn parse_blob_base_url(value: &str) -> anyhow::Result<url::Url> {
    anyhow::ensure!(
        !value.is_empty()
            && !value.chars().any(|c| c.is_whitespace() || c.is_control())
            && !value.contains(['\\', '%'])
            && !value.split('/').any(|v| matches!(v, "." | "..")),
        "unsafe blob base URL"
    );
    let mut url = url::Url::parse(value)?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "HTTP(S) base without credentials/query/fragment required"
    );
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let required_users = std::env::var("REQUIRED_USERS")
        .context("REQUIRED_USERS must be set")?
        .parse::<NonZeroUsize>()
        .context("REQUIRED_USERS must be a positive integer")?;
    let state = AppState {
        registry_url: std::env::var("REGISTRY_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:3001".to_owned()),
        http: Client::new(),
        required_users,
        blob_base_url: std::env::var("BLOB_BASE_URL")
            .ok()
            .map(|v| parse_blob_base_url(&v))
            .transpose()
            .context("invalid BLOB_BASE_URL")?,
    };

    let app = Router::new()
        .route("/nix-cache-info", get(nix_cache_info))
        .route("/{narinfo_path}", get(narinfo))
        // /narは上流キャッシュサーバーに任せることにした
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();

    axum::serve(listener, app).await?;
    Ok(())
}

async fn narinfo(
    State(state): State<AppState>,
    Path(narinfo_path): Path<NarInfoPath>,
) -> Result<NarInfoResponse, NarInfoError> {
    let response = state
        .http
        .get(format!(
            "{}/nar-info/{}",
            state.registry_url.trim_end_matches('/'),
            narinfo_path.hash()
        ))
        .send()
        .await?;
    if response.status() == StatusCode::NOT_FOUND {
        return Err(NarInfoError::NotFound);
    }
    let reports = response
        .error_for_status()?
        .json::<Vec<RegistryNarRecord>>()
        .await?;
    let reports = reports
        .into_iter()
        .filter(|report| report.store_path_hash == narinfo_path.hash().as_str())
        .collect();
    let record =
        select_report_for_backend(reports, state.required_users, state.blob_base_url.is_some())?;

    if let (Some(base), Some(artifact), Some(metadata)) =
        (&state.blob_base_url, &record.artifact, &record.metadata)
    {
        let url = base.join(&format!("nar/{}.nar", artifact.file_hash))?;
        let available = state.http.head(url.clone()).send().await?;
        if available.status() == StatusCode::NOT_FOUND {
            return Err(NarInfoError::NotFound);
        }
        let available = available.error_for_status()?;
        if available
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            != Some(artifact.file_size)
        {
            return Err(NarInfoError::NotFound);
        }
        return Ok(NarInfoResponse::from_blob(
            &record,
            metadata,
            artifact,
            url.to_string(),
        )?);
    }
    if record.cache_url.is_empty() {
        return Err(NarInfoError::NotFound);
    }
    let cache_server = CacheServer::try_from(record.cache_url.as_str())?;
    let server_info = cache_server
        .fetch_narinfo(&state.http, &record.store_path_hash)
        .await?;
    let server_record = NarRecord::try_from(server_info.clone())?;
    let nar_url = cache_server.url().join(&server_record.cache_url)?;
    if !record.matches_nar(&server_record) {
        return Err(NarInfoError::NotFound);
    }

    if let Some(metadata) = &record.metadata {
        let mut upstream_metadata = nar_metadata::Metadata {
            references: server_info
                .references()
                .iter()
                .map(|p| p.to_absolute_path())
                .collect(),
            deriver: server_info.deriver().map(|p| p.to_absolute_path()),
        };
        upstream_metadata.canonicalize();
        if metadata != &upstream_metadata {
            return Err(NarInfoError::NotFound);
        }
    }
    Ok(NarInfoResponse::from_upstream(
        server_info,
        nar_url.to_string(),
    )?)
}

// Implementation detail: choose the lexicographically first qualifying result/cache.
#[cfg(test)]
fn select_report(
    reports: Vec<RegistryNarRecord>,
    required_users: NonZeroUsize,
) -> Result<NarRecord, NarInfoError> {
    select_report_for_backend(reports, required_users, true)
}

fn select_report_for_backend(
    reports: Vec<RegistryNarRecord>,
    required_users: NonZeroUsize,
    blob_enabled: bool,
) -> Result<NarRecord, NarInfoError> {
    // Invalid report metadata cannot vote and must not veto other users' results.
    let mut records = reports
        .into_iter()
        .filter_map(|report| NarRecord::try_from(report).ok())
        .collect::<Vec<_>>();
    records.sort_by_key(|record| {
        (
            record.drv_path.clone(),
            record.output_name.clone(),
            record.store_path_hash.as_str().to_owned(),
            record.store_path.to_basename(),
            record.nar_hash.to_sri_string(),
            record.nar_size,
            record.metadata.clone(),
            // Within one result, prefer direct blob publication when configured.
            !(blob_enabled && record.artifact.is_some()),
            record.cache_url.clone(),
        )
    });
    let winner = records
        .iter()
        .position(|record| {
            if (record.cache_url.is_empty()
                && (!blob_enabled || record.artifact.is_none() || record.metadata.is_none()))
                || record
                    .user_id
                    .as_deref()
                    .is_none_or(|id| id.trim().is_empty())
            {
                return false;
            }
            records
                .iter()
                .filter(|other| record.matches_result(other))
                .filter_map(|other| other.user_id.as_deref().filter(|id| !id.trim().is_empty()))
                .collect::<std::collections::HashSet<_>>()
                .len()
                >= required_users.get()
        })
        .ok_or(NarInfoError::NotFound)?;
    Ok(records.swap_remove(winner))
}

async fn nix_cache_info() -> &'static str {
    "\
StoreDir: /nix/store
WantMassQuery: 0
Priority: 30
"
}

#[cfg(test)]
mod tests {
    use axum::{Json, Router, routing::get};

    use super::*;

    const STORE_PATH_HASH: &str = "y1a49lg2ja68djssigz14lhdxvxcwbxa";
    const STORE_PATH: &str = "/nix/store/y1a49lg2ja68djssigz14lhdxvxcwbxa-hello-2.12.3";
    const NAR_HASH: &str = "sha256-rS0qEqEXArxnAdzxNkNv+4PaHxXcQ/JdN0Kjkuq6XSY=";
    const NAR_SIZE: i64 = 226640;

    async fn mock_cache() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cache_url = format!("http://{}/", listener.local_addr().unwrap());
        let body = format!(
            "StorePath: {STORE_PATH}\nURL: nar/archive.nar\nCompression: none\nNarHash: {NAR_HASH}\nNarSize: {NAR_SIZE}\n"
        );
        let app = Router::new().route(
            &format!("/{STORE_PATH_HASH}.narinfo"),
            get(move || async move { body }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        cache_url
    }

    async fn mock_missing_cache() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cache_url = format!("http://{}/", listener.local_addr().unwrap());
        let app = Router::new().fallback(|| async { StatusCode::NOT_FOUND });
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        cache_url
    }

    async fn mock_registry(cache_url: String) -> String {
        let record = RegistryNarRecord {
            user_id: Some("alice@example.com".into()),
            drv_path: None,
            output_name: None,
            store_path_hash: STORE_PATH_HASH.to_owned(),
            store_path: STORE_PATH.to_owned(),
            nar_hash: NAR_HASH.to_owned(),
            nar_size: NAR_SIZE,
            cache_url: Some(cache_url),
            metadata: None,
            artifact: None,
        };
        mock_registry_reports(vec![
            record.clone(),
            RegistryNarRecord {
                user_id: Some("bob@example.com".into()),
                ..record
            },
        ])
        .await
    }

    async fn mock_registry_reports(reports: Vec<RegistryNarRecord>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let registry_url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            &format!("/nar-info/{STORE_PATH_HASH}"),
            get(move || {
                let reports = reports.clone();
                async move { Json(reports) }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        registry_url
    }

    #[test]
    fn candidate_selection_is_stable_across_registry_row_order() {
        let first: RegistryNarRecord = serde_json::from_value(serde_json::json!({
            "user_id":"alice", "store_path_hash":STORE_PATH_HASH,"store_path":STORE_PATH,
            "nar_hash":"sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","nar_size":10,
            "metadata":{"references":[STORE_PATH],"deriver":null},
            "artifact":{"file_hash":"0000000000000000000000000000000000000000000000000000000000000000","file_size":10,"compression":"none"}
        })).unwrap();
        let mut second = first.clone();
        second.metadata.as_mut().unwrap().references.clear();
        let threshold = NonZeroUsize::new(1).unwrap();
        let forward = select_report(vec![first.clone(), second.clone()], threshold).unwrap();
        let reverse = select_report(vec![second, first], threshold).unwrap();
        assert_eq!(forward.metadata, reverse.metadata);
    }

    #[test]
    fn configured_blob_backend_prefers_artifact_for_the_same_qualifying_candidate() {
        let with_artifact: RegistryNarRecord = serde_json::from_value(serde_json::json!({
            "user_id": "alice", "store_path_hash": STORE_PATH_HASH, "store_path": STORE_PATH,
            "nar_hash": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=", "nar_size": 10,
            "cache_url": "https://cache.example/", "metadata": {"references": [], "deriver": null},
            "artifact": {"file_hash": "0".repeat(64), "file_size": 10, "compression": "none"}
        }))
        .unwrap();
        let mut cache_only = with_artifact.clone();
        cache_only.user_id = Some("bob".into());
        cache_only.artifact = None;
        for reports in [
            vec![cache_only.clone(), with_artifact.clone()],
            vec![with_artifact, cache_only],
        ] {
            let selected =
                select_report_for_backend(reports, NonZeroUsize::new(2).unwrap(), true).unwrap();
            assert!(
                selected.artifact.is_some(),
                "registry row order selected legacy over the agreed blob"
            );
        }
    }

    #[test]
    fn canonical_reference_sets_agree_without_artifact_location_votes() {
        let a = format!("/nix/store/{STORE_PATH_HASH}-a");
        let b = format!("/nix/store/{STORE_PATH_HASH}-b");
        let report = serde_json::json!({
            "user_id":"alice", "store_path_hash":STORE_PATH_HASH,"store_path":STORE_PATH,
            "nar_hash":"sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","nar_size":10,
            "metadata":{"references":[b,a,a],"deriver":null},
            "artifact":{"file_hash":"0000000000000000000000000000000000000000000000000000000000000000","file_size":10,"compression":"none"}
        });
        let first: RegistryNarRecord = serde_json::from_value(report.clone()).unwrap();
        let mut second = report;
        second["user_id"] = "bob".into();
        second["artifact"] = serde_json::Value::Null;
        second["metadata"]["references"] = serde_json::json!([a, b]);
        let second: RegistryNarRecord = serde_json::from_value(second).unwrap();
        assert!(
            select_report(
                vec![first.clone(), second.clone()],
                NonZeroUsize::new(2).unwrap()
            )
            .is_ok()
        );
        let mut dissent = second.clone();
        dissent.metadata.as_mut().unwrap().references.clear();
        assert!(matches!(
            select_report(vec![first.clone(), dissent], NonZeroUsize::new(2).unwrap()),
            Err(NarInfoError::NotFound)
        ));
        let mut dissent = second;
        dissent.metadata.as_mut().unwrap().deriver =
            Some(format!("/nix/store/{STORE_PATH_HASH}-different.drv"));
        assert!(matches!(
            select_report(vec![first, dissent], NonZeroUsize::new(2).unwrap()),
            Err(NarInfoError::NotFound)
        ));
    }

    #[test]
    fn invalid_artifact_metadata_from_registry_cannot_be_selected() {
        let report = serde_json::json!({
            "user_id":"alice", "store_path_hash":STORE_PATH_HASH, "store_path":STORE_PATH,
            "nar_hash":"sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=", "nar_size":10,
            "metadata":{"references":[],"deriver":null},
            "artifact":{"file_hash":"0000000000000000000000000000000000000000000000000000000000000000","file_size":10,"compression":"none"}
        });
        for (pointer, value) in [
            ("/artifact/compression", serde_json::json!("xz")),
            ("/artifact/file_hash", serde_json::json!("../escape")),
            ("/artifact/file_hash", serde_json::json!("1".repeat(64))),
            ("/artifact/file_size", serde_json::json!(11)),
            ("/metadata", serde_json::Value::Null),
            (
                "/metadata/references",
                serde_json::json!(["evil\nURL: evil"]),
            ),
        ] {
            let mut invalid = report.clone();
            *invalid.pointer_mut(pointer).unwrap() = value;
            assert!(
                matches!(
                    select_report(
                        vec![serde_json::from_value(invalid).unwrap()],
                        NonZeroUsize::new(1).unwrap()
                    ),
                    Err(NarInfoError::NotFound)
                ),
                "{pointer}"
            );
        }
    }

    #[test]
    fn dissent_does_not_block_a_qualifying_result_even_when_tied() {
        let record: RegistryNarRecord = serde_json::from_value(serde_json::json!({
            "user_id": "alice@example.com", "store_path_hash": STORE_PATH_HASH,
            "store_path": STORE_PATH, "nar_hash": NAR_HASH, "nar_size": NAR_SIZE,
            "cache_url": "https://cache.example/"
        }))
        .unwrap();
        let reports = vec![
            RegistryNarRecord {
                user_id: Some("carol@example.com".into()),
                nar_size: NAR_SIZE + 1,
                ..record.clone()
            },
            RegistryNarRecord {
                user_id: Some("dan@example.com".into()),
                nar_size: NAR_SIZE + 1,
                ..record.clone()
            },
            RegistryNarRecord {
                user_id: Some("bob@example.com".into()),
                ..record.clone()
            },
            record,
        ];
        assert_eq!(
            select_report(reports, NonZeroUsize::new(2).unwrap())
                .unwrap()
                .nar_size,
            NAR_SIZE as u64
        );
    }

    #[test]
    fn different_build_targets_cannot_be_combined() {
        let mut report = serde_json::json!({
            "user_id": "alice@example.com", "drv_path": "/nix/store/first.drv", "output_name": "out",
            "store_path_hash": STORE_PATH_HASH, "store_path": STORE_PATH,
            "nar_hash": NAR_HASH, "nar_size": NAR_SIZE, "cache_url": "https://cache.example/"
        });
        let first: RegistryNarRecord = serde_json::from_value(report.clone()).unwrap();
        report["user_id"] = "bob@example.com".into();
        for (field, value) in [
            ("drv_path", "/nix/store/second.drv"),
            ("output_name", "dev"),
        ] {
            let mut other = report.clone();
            other[field] = value.into();
            let second = serde_json::from_value(other).unwrap();
            assert!(matches!(
                select_report(vec![first.clone(), second], NonZeroUsize::new(2).unwrap()),
                Err(NarInfoError::NotFound)
            ));
        }
    }

    #[test]
    fn counts_unpublished_matching_users_but_requires_one_published_cache() {
        let mut report = serde_json::json!({
            "user_id": "alice@example.com", "store_path_hash": STORE_PATH_HASH, "store_path": STORE_PATH,
            "nar_hash": NAR_HASH, "nar_size": NAR_SIZE, "cache_url": "https://cache.example/"
        });
        let first: RegistryNarRecord = serde_json::from_value(report.clone()).unwrap();
        report["cache_url"] = serde_json::Value::Null;
        report["user_id"] = "bob@example.com".into();
        let second: RegistryNarRecord =
            serde_json::from_value(report).expect("unpublished user is still a voter");
        assert_eq!(
            select_report(vec![first, second.clone()], NonZeroUsize::new(2).unwrap())
                .unwrap()
                .cache_url,
            "https://cache.example/"
        );
        assert!(matches!(
            select_report(vec![second], NonZeroUsize::new(1).unwrap()),
            Err(NarInfoError::NotFound)
        ));
    }

    #[test]
    fn ignores_reports_with_inconsistent_store_hash_and_path() {
        let report: RegistryNarRecord = serde_json::from_value(serde_json::json!({
            "user_id": "alice@example.com", "store_path_hash": "0123456789abcdfghijklmnpqrsvwxyz",
            "store_path": STORE_PATH, "nar_hash": NAR_HASH, "nar_size": NAR_SIZE, "cache_url": "https://cache.example/"
        })).unwrap();
        assert!(matches!(
            select_report(vec![report], NonZeroUsize::new(1).unwrap()),
            Err(NarInfoError::NotFound)
        ));
    }

    #[test]
    fn legacy_unowned_reports_never_count_as_users() {
        for user_id in [None, Some(""), Some("   ")] {
            let report: RegistryNarRecord = serde_json::from_value(serde_json::json!({
                "user_id": user_id, "store_path_hash": STORE_PATH_HASH, "store_path": STORE_PATH,
                "nar_hash": NAR_HASH, "nar_size": NAR_SIZE, "cache_url": "https://cache.example/"
            }))
            .unwrap();
            assert!(matches!(
                select_report(vec![report.clone(), report], NonZeroUsize::new(1).unwrap()),
                Err(NarInfoError::NotFound)
            ));
        }
    }

    #[test]
    fn threshold_is_configurable_and_counts_each_user_once_per_result() {
        let first: RegistryNarRecord = serde_json::from_value(serde_json::json!({
            "user_id": "alice@example.com", "store_path_hash": STORE_PATH_HASH, "store_path": STORE_PATH,
            "nar_hash": NAR_HASH, "nar_size": NAR_SIZE, "cache_url": "https://cache.example/"
        })).unwrap();
        assert!(select_report(vec![first.clone()], NonZeroUsize::new(1).unwrap()).is_ok());
        let second = RegistryNarRecord {
            user_id: Some("bob@example.com".into()),
            ..first.clone()
        };
        let dissent = RegistryNarRecord {
            nar_size: NAR_SIZE + 1,
            ..first.clone()
        };
        let reports = vec![first.clone(), first.clone(), second, dissent];
        assert!(select_report(reports.clone(), NonZeroUsize::new(2).unwrap()).is_ok());
        assert!(matches!(
            select_report(reports.clone(), NonZeroUsize::new(3).unwrap()),
            Err(NarInfoError::NotFound)
        ));
        let third = RegistryNarRecord {
            user_id: Some("carol@example.com".into()),
            ..first
        };
        let mut reports = reports;
        reports.push(third);
        assert!(select_report(reports, NonZeroUsize::new(3).unwrap()).is_ok());
    }

    #[test]
    fn repeated_reports_from_one_user_do_not_reach_threshold() {
        let record: RegistryNarRecord = serde_json::from_value(serde_json::json!({
            "user_id": "alice@example.com", "drv_path": "/nix/store/example.drv", "output_name": "out",
            "store_path_hash": STORE_PATH_HASH, "store_path": STORE_PATH,
            "nar_hash": NAR_HASH, "nar_size": NAR_SIZE, "cache_url": "https://cache.example/"
        })).unwrap();
        let result = select_report(
            vec![record.clone(), record.clone(), record],
            NonZeroUsize::new(2).unwrap(),
        );
        assert!(matches!(result, Err(NarInfoError::NotFound)));
    }

    #[tokio::test]
    async fn rejects_registry_reports_for_a_different_requested_store_hash() {
        let other_hash = "0123456789abcdfghijklmnpqrsvwxyz";
        let other_path = format!("/nix/store/{other_hash}-hello-2.12.3");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cache_url = format!("http://{}/", listener.local_addr().unwrap());
        let body = format!(
            "StorePath: {other_path}\nURL: nar/archive.nar\nCompression: none\nNarHash: {NAR_HASH}\nNarSize: {NAR_SIZE}\n"
        );
        let app = Router::new().fallback(get(move || async move { body }));
        let cache = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let record: RegistryNarRecord = serde_json::from_value(serde_json::json!({
            "user_id": "alice@example.com", "store_path_hash": other_hash, "store_path": other_path,
            "nar_hash": NAR_HASH, "nar_size": NAR_SIZE, "cache_url": cache_url
        }))
        .unwrap();
        let state = AppState {
            registry_url: mock_registry_reports(vec![record]).await,
            http: Client::new(),
            blob_base_url: None,
            required_users: NonZeroUsize::new(1).unwrap(),
        };
        let path = NarInfoPath::try_from(format!("{STORE_PATH_HASH}.narinfo")).unwrap();
        let response = narinfo(State(state), Path(path)).await;
        cache.abort();
        assert!(matches!(response, Err(NarInfoError::NotFound)));
    }

    #[tokio::test]
    async fn rejects_upstream_metadata_that_does_not_match_the_report() {
        let cache_url = mock_cache().await;
        let report: RegistryNarRecord = serde_json::from_value(serde_json::json!({
            "user_id": "alice@example.com", "store_path_hash": STORE_PATH_HASH, "store_path": STORE_PATH,
            "nar_hash": NAR_HASH, "nar_size": NAR_SIZE, "cache_url": cache_url
        })).unwrap();
        let wrong_path = RegistryNarRecord {
            store_path: format!("/nix/store/{STORE_PATH_HASH}-different-name"),
            ..report.clone()
        };
        let wrong_hash = RegistryNarRecord {
            nar_hash: "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            ..report.clone()
        };
        let wrong_size = RegistryNarRecord {
            nar_size: NAR_SIZE + 1,
            ..report
        };
        for record in [wrong_path, wrong_hash, wrong_size] {
            let second = RegistryNarRecord {
                user_id: Some("bob@example.com".into()),
                ..record.clone()
            };
            let state = AppState {
                registry_url: mock_registry_reports(vec![record, second]).await,
                http: Client::new(),
                blob_base_url: None,
                required_users: NonZeroUsize::new(2).unwrap(),
            };
            let path = NarInfoPath::try_from(format!("{STORE_PATH_HASH}.narinfo")).unwrap();
            assert!(matches!(
                narinfo(State(state), Path(path)).await,
                Err(NarInfoError::NotFound)
            ));
        }
    }

    #[tokio::test]
    async fn legacy_cache_must_match_agreed_references_and_deriver_when_present() {
        for metadata in [
            serde_json::json!({"references":[STORE_PATH],"deriver":null}),
            serde_json::json!({"references":[],"deriver":format!("/nix/store/{STORE_PATH_HASH}-hello.drv")}),
        ] {
            let record: RegistryNarRecord = serde_json::from_value(serde_json::json!({
                "user_id":"alice", "store_path_hash":STORE_PATH_HASH,"store_path":STORE_PATH,
                "nar_hash":NAR_HASH,"nar_size":NAR_SIZE,"metadata":metadata,"cache_url":mock_cache().await,
            })).unwrap();
            let state = AppState {
                registry_url: mock_registry_reports(vec![record]).await,
                http: Client::new(),
                required_users: NonZeroUsize::new(1).unwrap(),
                blob_base_url: None,
            };
            let path = NarInfoPath::try_from(format!("{STORE_PATH_HASH}.narinfo")).unwrap();
            assert!(matches!(
                narinfo(State(state), Path(path)).await,
                Err(NarInfoError::NotFound)
            ));
        }
    }

    #[tokio::test]
    async fn disabled_blob_backend_does_not_hide_an_available_legacy_cache() {
        let cache_url = mock_cache().await;
        let record: RegistryNarRecord = serde_json::from_value(serde_json::json!({
            "user_id":"alice", "store_path_hash":STORE_PATH_HASH,"store_path":STORE_PATH,
            "nar_hash":NAR_HASH,"nar_size":NAR_SIZE,
            "metadata":{"references":[],"deriver":null},
            "artifact":{"file_hash":"ad2d2a12a11702bc6701dcf136436ffb83da1f15dc43f25d3742a392eaba5d26","file_size":NAR_SIZE,"compression":"none"}
        })).unwrap();
        let mut second = record.clone();
        second.user_id = Some("bob".into());
        second.artifact = None;
        second.cache_url = Some(cache_url);
        let state = AppState {
            registry_url: mock_registry_reports(vec![record, second]).await,
            http: Client::new(),
            required_users: NonZeroUsize::new(2).unwrap(),
            blob_base_url: None,
        };
        let path = NarInfoPath::try_from(format!("{STORE_PATH_HASH}.narinfo")).unwrap();
        assert!(narinfo(State(state), Path(path)).await.is_ok());
    }

    #[tokio::test]
    async fn returns_narinfo_when_registry_and_upstream_records_match() {
        let cache_url = mock_cache().await;
        let state = AppState {
            registry_url: mock_registry(cache_url.clone()).await,
            http: Client::new(),
            blob_base_url: None,
            required_users: NonZeroUsize::new(2).unwrap(),
        };
        let path = NarInfoPath::try_from(format!("{STORE_PATH_HASH}.narinfo")).unwrap();

        let response = narinfo(State(state), Path(path))
            .await
            .unwrap()
            .into_response();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();

        assert!(body.contains(&format!("URL: {cache_url}nar/archive.nar")));
    }

    #[tokio::test]
    async fn returns_not_found_when_upstream_narinfo_is_missing() {
        let state = AppState {
            registry_url: mock_registry(mock_missing_cache().await).await,
            http: Client::new(),
            blob_base_url: None,
            required_users: NonZeroUsize::new(2).unwrap(),
        };
        let path = NarInfoPath::try_from(format!("{STORE_PATH_HASH}.narinfo")).unwrap();

        let result = narinfo(State(state), Path(path)).await;

        match result {
            Err(error) => assert_eq!(error.into_response().status(), StatusCode::NOT_FOUND),
            Ok(_) => panic!("expected the missing upstream narinfo to return an error"),
        }
    }

    #[tokio::test]
    async fn returns_narinfo_for_a_qualifying_result_despite_dissent() {
        let cache_url = mock_cache().await;
        let record = RegistryNarRecord {
            user_id: Some("alice@example.com".into()),
            drv_path: None,
            output_name: None,
            store_path_hash: STORE_PATH_HASH.to_owned(),
            store_path: STORE_PATH.to_owned(),
            nar_hash: NAR_HASH.to_owned(),
            nar_size: NAR_SIZE,
            cache_url: Some(cache_url.clone()),
            metadata: None,
            artifact: None,
        };
        let other = RegistryNarRecord {
            user_id: Some("charlie@example.com".into()),
            nar_size: NAR_SIZE + 1,
            ..record.clone()
        };
        let registry_url = mock_registry_reports(vec![
            other,
            RegistryNarRecord {
                user_id: Some("bob@example.com".into()),
                ..record.clone()
            },
            record,
        ])
        .await;

        let state = AppState {
            registry_url,
            http: Client::new(),
            blob_base_url: None,
            required_users: NonZeroUsize::new(2).unwrap(),
        };
        let path = NarInfoPath::try_from(format!("{STORE_PATH_HASH}.narinfo")).unwrap();
        let response = narinfo(State(state), Path(path))
            .await
            .unwrap()
            .into_response();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();

        assert!(
            String::from_utf8(body.to_vec())
                .unwrap()
                .contains(&format!("URL: {cache_url}nar/archive.nar"))
        );
    }

    #[test]
    fn counts_matching_results_from_different_caches_together() {
        let record = RegistryNarRecord {
            user_id: Some("alice@example.com".into()),
            drv_path: None,
            output_name: None,
            store_path_hash: STORE_PATH_HASH.to_owned(),
            store_path: STORE_PATH.to_owned(),
            nar_hash: NAR_HASH.to_owned(),
            nar_size: NAR_SIZE,
            cache_url: Some("https://first.example/".to_owned()),
            metadata: None,
            artifact: None,
        };
        let matching = RegistryNarRecord {
            user_id: Some("bob@example.com".into()),
            cache_url: Some("https://second.example/".to_owned()),
            ..record.clone()
        };
        let other = RegistryNarRecord {
            user_id: Some("charlie@example.com".into()),
            nar_size: NAR_SIZE + 1,
            ..record.clone()
        };

        let winner =
            select_report(vec![other, record, matching], NonZeroUsize::new(2).unwrap()).unwrap();

        assert_eq!(winner.cache_url, "https://first.example/");
    }

    #[tokio::test]
    async fn returns_not_found_when_neither_result_reaches_threshold() {
        let record = RegistryNarRecord {
            user_id: Some("alice@example.com".into()),
            drv_path: None,
            output_name: None,
            store_path_hash: STORE_PATH_HASH.to_owned(),
            store_path: STORE_PATH.to_owned(),
            nar_hash: NAR_HASH.to_owned(),
            nar_size: NAR_SIZE,
            cache_url: Some("http://127.0.0.1:1/".to_owned()),
            metadata: None,
            artifact: None,
        };
        let other = RegistryNarRecord {
            user_id: Some("charlie@example.com".into()),
            nar_size: NAR_SIZE + 1,
            ..record.clone()
        };
        let state = AppState {
            registry_url: mock_registry_reports(vec![record, other]).await,
            http: Client::new(),
            blob_base_url: None,
            required_users: NonZeroUsize::new(2).unwrap(),
        };
        let path = NarInfoPath::try_from(format!("{STORE_PATH_HASH}.narinfo")).unwrap();
        let result = narinfo(State(state), Path(path)).await;

        match result {
            Err(error) => assert_eq!(error.into_response().status(), StatusCode::NOT_FOUND),
            Ok(_) => panic!("expected insufficient distinct users to be rejected"),
        }
    }
}
