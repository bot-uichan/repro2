use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("ALTER TABLE build_reports ADD COLUMN metadata text NULL")
            .await?;
        db.execute_unprepared("ALTER TABLE build_reports ADD COLUMN artifact text NULL")
            .await?;
        db.execute_unprepared("DROP INDEX idx_build_reports_user_result")
            .await?;
        db.execute_unprepared("CREATE UNIQUE INDEX idx_build_reports_user_result ON build_reports (user_id, drv_path IS NULL, IFNULL(drv_path, ''), output_name IS NULL, IFNULL(output_name, ''), store_path_hash, store_path, nar_hash, nar_size, metadata IS NULL, IFNULL(metadata, ''))").await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Removing metadata can collapse different candidates into one identity.
        // Never silently discard reports or rewrite authenticated ownership.
        Err(DbErr::Custom("candidate metadata migration is irreversible; restore a pre-migration backup to downgrade".into()))
    }
}
