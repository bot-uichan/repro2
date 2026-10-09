pub use sea_orm_migration::prelude::*;

mod m20220101_000001_create_table;
mod m20260923_172120_create_build_reports;
mod m20260925_000001_nullable_cache_url;
mod m20261009_000001_report_identity;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20220101_000001_create_table::Migration),
            Box::new(m20260923_172120_create_build_reports::Migration),
            Box::new(m20260925_000001_nullable_cache_url::Migration),
            Box::new(m20261009_000001_report_identity::Migration),
        ]
    }
}
