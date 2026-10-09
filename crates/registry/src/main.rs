mod db;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set, sea_query::OnConflict,
};
use serde::{Deserialize, Serialize};

use db::entities::{build_reports, prelude::BuildReports};

#[derive(Serialize)]
struct NarRecord {
    user_id: Option<String>,
    id: i64,
    drv_path: Option<String>,
    output_name: Option<String>,
    store_path_hash: String,
    store_path: String,
    nar_hash: String,
    nar_size: i64,
    cache_url: Option<String>,
}

#[derive(Deserialize)]
struct BuildReport {
    drv_path: Option<String>,
    output_name: String,
    store_path_hash: String,
    store_path: String,
    nar_hash: String,
    nar_size: i64,
    cache_url: Option<String>,
}

impl From<build_reports::Model> for NarRecord {
    fn from(model: build_reports::Model) -> Self {
        Self {
            user_id: model.user_id,
            id: model.id,
            drv_path: model.drv_path,
            output_name: model.output_name,
            store_path_hash: model.store_path_hash,
            store_path: model.store_path,
            nar_hash: model.nar_hash,
            nar_size: model.nar_size,
            cache_url: model.cache_url,
        }
    }
}

#[tokio::main]
async fn main() {
    let db = db::connection::connect().await.unwrap();
    let app = Router::new()
        .route("/nar-info/{hash}", get(nar_info))
        .route("/build-reports", post(create_build_report))
        .with_state(db);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3001")
        .await
        .unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn create_build_report(
    State(db): State<DatabaseConnection>,
    headers: HeaderMap,
    Json(report): Json<BuildReport>,
) -> Result<StatusCode, StatusCode> {
    let user_id = headers
        .get("Tailscale-User-Login")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    if let Some(cache_url) = &report.cache_url {
        let url = url::Url::parse(cache_url).map_err(|_| StatusCode::BAD_REQUEST)?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.query().is_some()
        {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    BuildReports::insert(build_reports::ActiveModel {
        user_id: Set(Some(user_id.to_owned())),
        drv_path: Set(report.drv_path),
        output_name: Set(Some(report.output_name)),
        store_path_hash: Set(report.store_path_hash),
        store_path: Set(report.store_path),
        nar_hash: Set(report.nar_hash),
        nar_size: Set(report.nar_size),
        cache_url: Set(report.cache_url),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::new()
            .update_column(build_reports::Column::CacheUrl)
            .to_owned(),
    )
    .exec(&db)
    .await
    .map_err(|error| {
        eprintln!("registry error: {error}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(StatusCode::CREATED)
}

async fn nar_info(
    State(db): State<DatabaseConnection>,
    Path(hash): Path<String>,
) -> Result<Json<Vec<NarRecord>>, StatusCode> {
    let models = BuildReports::find()
        .filter(build_reports::Column::StorePathHash.eq(hash))
        .all(&db)
        .await
        .map_err(|error| {
            eprintln!("registry error: {error}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    if models.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(models.into_iter().map(Into::into).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use migration::{Migrator, MigratorTrait};
    use sea_orm::{ConnectionTrait, Database};

    #[tokio::test]
    async fn rejects_report_without_authenticated_user() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/build-reports", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/build-reports", post(create_build_report))
            .with_state(db.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let response = reqwest::Client::new().post(url)
            .header("content-type", "application/json")
            .body(r#"{"output_name":"out","store_path_hash":"abc","store_path":"/nix/store/abc-example","nar_hash":"sha256-example","nar_size":10}"#)
            .send().await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(BuildReports::find().all(&db).await.unwrap().is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn preserves_authenticated_user_in_storage() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let report = BuildReport {
            drv_path: None,
            output_name: "out".into(),
            store_path_hash: "abc".into(),
            store_path: "/nix/store/abc-example".into(),
            nar_hash: "sha256-example".into(),
            nar_size: 10,
            cache_url: None,
        };
        let headers = HeaderMap::from_iter([(
            "Tailscale-User-Login".parse().unwrap(),
            "alice@example.com".parse().unwrap(),
        )]);
        create_build_report(State(db.clone()), headers, Json(report))
            .await
            .unwrap();
        let rows = db
            .query_all_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                "SELECT * FROM build_reports",
            ))
            .await
            .unwrap();
        assert_eq!(
            rows[0]
                .try_get::<Option<String>>("", "user_id")
                .unwrap()
                .as_deref(),
            Some("alice@example.com")
        );
    }

    #[tokio::test]
    async fn serves_published_report_with_authenticated_user() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/build-reports", post(create_build_report))
            .route("/nar-info/{hash}", get(nar_info))
            .with_state(db);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::new();
        assert_eq!(http.post(format!("{url}/build-reports")).header("Tailscale-User-Login", "alice@example.com").header("content-type", "application/json")
            .body(r#"{"output_name":"out","store_path_hash":"abc","store_path":"/nix/store/abc-example","nar_hash":"sha256-example","nar_size":10,"cache_url":"https://cache.example/"}"#)
            .send().await.unwrap().status(), StatusCode::CREATED);
        let response = http
            .get(format!("{url}/nar-info/abc"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        assert!(body.contains(r#""user_id":"alice@example.com""#));
        assert!(body.contains("https://cache.example/"));
        server.abort();
    }

    #[tokio::test]
    async fn repeated_report_from_same_user_is_idempotent() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        for _ in 0..3 {
            let report = BuildReport {
                drv_path: None,
                output_name: "out".into(),
                store_path_hash: "abc".into(),
                store_path: "/nix/store/abc-example".into(),
                nar_hash: "sha256-example".into(),
                nar_size: 10,
                cache_url: None,
            };
            let headers = HeaderMap::from_iter([(
                "Tailscale-User-Login".parse().unwrap(),
                "alice@example.com".parse().unwrap(),
            )]);
            create_build_report(State(db.clone()), headers, Json(report))
                .await
                .unwrap();
        }
        assert_eq!(BuildReports::find().all(&db).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rejects_non_http_cache_urls() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        for cache_url in [
            "file:///etc/passwd",
            "ftp://cache.example/",
            "not a URL",
            "https://user:password@cache.example/",
            "https://cache.example/#fragment",
        ] {
            let report = BuildReport {
                drv_path: None,
                output_name: "out".into(),
                store_path_hash: "abc".into(),
                store_path: "/nix/store/abc-example".into(),
                nar_hash: "sha256-example".into(),
                nar_size: 10,
                cache_url: Some(cache_url.into()),
            };
            let headers = HeaderMap::from_iter([(
                "Tailscale-User-Login".parse().unwrap(),
                "alice@example.com".parse().unwrap(),
            )]);
            assert_eq!(
                create_build_report(State(db.clone()), headers, Json(report))
                    .await
                    .err(),
                Some(StatusCode::BAD_REQUEST)
            );
        }
        assert!(BuildReports::find().all(&db).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_empty_or_non_text_identity_without_writing() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        for value in [
            axum::http::HeaderValue::from_static(""),
            axum::http::HeaderValue::from_static("   "),
            axum::http::HeaderValue::from_bytes(b"\xff").unwrap(),
        ] {
            let report = BuildReport {
                drv_path: None,
                output_name: "out".into(),
                store_path_hash: "abc".into(),
                store_path: "/nix/store/abc-example".into(),
                nar_hash: "sha256-example".into(),
                nar_size: 10,
                cache_url: None,
            };
            let headers = HeaderMap::from_iter([("Tailscale-User-Login".parse().unwrap(), value)]);
            assert_eq!(
                create_build_report(State(db.clone()), headers, Json(report))
                    .await
                    .err(),
                Some(StatusCode::UNAUTHORIZED)
            );
        }
        assert!(BuildReports::find().all(&db).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn migration_preserves_legacy_reports_without_inventing_an_owner() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, Some(3)).await.unwrap();
        db.execute_unprepared("INSERT INTO build_reports (store_path_hash,store_path,nar_hash,nar_size,cache_url) VALUES ('abc','/nix/store/abc-example','sha256-old',10,'https://old.example/')").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let Json(reports) = nar_info(State(db), Path("abc".into())).await.unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].user_id, None);
        assert_eq!(reports[0].nar_hash, "sha256-old");
    }

    #[tokio::test]
    async fn does_not_merge_null_and_empty_derivation_inputs() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        for drv_path in [None, Some(String::new())] {
            let report = BuildReport {
                drv_path,
                output_name: "out".into(),
                store_path_hash: "abc".into(),
                store_path: "/nix/store/abc-example".into(),
                nar_hash: "sha256-example".into(),
                nar_size: 10,
                cache_url: None,
            };
            let headers = HeaderMap::from_iter([(
                "Tailscale-User-Login".parse().unwrap(),
                "alice@example.com".parse().unwrap(),
            )]);
            create_build_report(State(db.clone()), headers, Json(report))
                .await
                .unwrap();
        }
        assert_eq!(BuildReports::find().all(&db).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn returns_all_reports_for_the_same_store_path() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db.execute_unprepared(
            "INSERT INTO build_reports (drv_path, output_name, store_path_hash, store_path, nar_hash, nar_size, cache_url) VALUES \
             ('/nix/store/example.drv', 'out', 'abc', '/nix/store/abc-example', 'sha256-first', 10, 'https://one.example/'), \
             ('/nix/store/example.drv', 'out', 'abc', '/nix/store/abc-example', 'sha256-second', 10, 'https://two.example/')",
        )
        .await
        .unwrap();

        let Json(reports) = nar_info(State(db), Path("abc".to_owned())).await.unwrap();

        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].nar_hash, "sha256-first");
        assert_eq!(reports[1].nar_hash, "sha256-second");
    }

    #[tokio::test]
    async fn serves_unpublished_report_as_a_vote_without_a_cache_url() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let report = BuildReport {
            drv_path: Some("/nix/store/example.drv".to_owned()),
            output_name: "out".to_owned(),
            store_path_hash: "abc".to_owned(),
            store_path: "/nix/store/abc-example".to_owned(),
            nar_hash: "sha256-example".to_owned(),
            nar_size: 10,
            cache_url: None,
        };

        assert_eq!(
            create_build_report(
                State(db.clone()),
                HeaderMap::from_iter([(
                    "Tailscale-User-Login".parse().unwrap(),
                    "alice@example.com".parse().unwrap()
                )]),
                Json(report)
            )
            .await
            .unwrap(),
            StatusCode::CREATED
        );
        let saved = BuildReports::find().one(&db).await.unwrap().unwrap();
        assert_eq!(saved.nar_hash, "sha256-example");
        assert_eq!(saved.cache_url, None);
        let Json(reports) = nar_info(State(db), Path("abc".to_owned())).await.unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].cache_url, None);
        assert_eq!(reports[0].user_id.as_deref(), Some("alice@example.com"));
    }
}
