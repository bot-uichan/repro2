use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // NULL deliberately preserves legacy reports without inventing an owner.
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE build_reports ADD COLUMN user_id varchar NULL")
            .await?;
        manager.get_connection().execute_unprepared(
            "CREATE UNIQUE INDEX idx_build_reports_user_result ON build_reports \
             (user_id, drv_path IS NULL, IFNULL(drv_path, ''), output_name IS NULL, IFNULL(output_name, ''), store_path_hash, store_path, nar_hash, nar_size)"
        ).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP INDEX idx_build_reports_user_result")
            .await?;
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE build_reports DROP COLUMN user_id")
            .await?;
        Ok(())
    }
}
