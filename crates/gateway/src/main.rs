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
    let record = select_report(reports, state.required_users)?;

    let cache_server = CacheServer::try_from(record.cache_url.as_str())?;
    let server_info = cache_server
        .fetch_narinfo(&state.http, &record.store_path_hash)
        .await?;
    let server_record = NarRecord::try_from(server_info.clone())?;
    let nar_url = cache_server.url().join(&server_record.cache_url)?;
    if !record.matches_nar(&server_record) {
        return Err(NarInfoError::NotFound);
    }

    Ok(NarInfoResponse::from_upstream(
        server_info,
        nar_url.to_string(),
    )?)
}

// Implementation detail: choose the lexicographically first qualifying result/cache.
fn select_report(
    reports: Vec<RegistryNarRecord>,
    required_users: NonZeroUsize,
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
            record.cache_url.clone(),
        )
    });
    let winner = records
        .iter()
        .position(|record| {
            if record.cache_url.is_empty()
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
    async fn returns_narinfo_when_registry_and_upstream_records_match() {
        let cache_url = mock_cache().await;
        let state = AppState {
            registry_url: mock_registry(cache_url.clone()).await,
            http: Client::new(),
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
        };
        let other = RegistryNarRecord {
            user_id: Some("charlie@example.com".into()),
            nar_size: NAR_SIZE + 1,
            ..record.clone()
        };
        let state = AppState {
            registry_url: mock_registry_reports(vec![record, other]).await,
            http: Client::new(),
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
