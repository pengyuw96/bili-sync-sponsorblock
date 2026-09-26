use sea_orm::Statement;
use sea_orm_migration::prelude::*;

/// Add `page.sponsor_cut_result` and reopen completed pages whose new slot5 is unset.
///
/// PageStatus grew from 5 → 6 slots (index 5 = SponsorBlock cut). Old rows have
/// bit31 completed with slot5==0; without clearing bit31 they never re-enter
/// download. Parent videos that were fully completed also need bit31 cleared and
/// video slot4 (page-download) reset so page downloads run again.
///
/// External `you_tube_video.page_task_status` shares PageStatus encoding: mark the
/// new slot as OK (N/A for external) so expanding to 6 slots does not reopen work.
///
/// Segment API: https://github.com/hanydd/BilibiliSponsorBlock
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        add_column_if_missing(
            manager,
            "page",
            Page::Table,
            "sponsor_cut_result",
            ColumnDef::new(Page::SponsorCutResult).text().null().to_owned(),
        )
        .await?;

        let conn = manager.get_connection();
        let backend = conn.get_database_backend();

        // Clear page completed bit when slot5 (bits 15-17) is still 0.
        conn.execute(Statement::from_string(
            backend,
            "UPDATE page SET download_status = download_status & 0x7FFFFFFF \
             WHERE ((download_status >> 15) & 7) = 0"
                .to_owned(),
        ))
        .await?;

        // Re-open parent videos that were completed so page slot5 can run:
        // clear video bit31 and zero video slot4 (bits 12-14 = page download).
        conn.execute(Statement::from_string(
            backend,
            "UPDATE video SET download_status = (download_status & 0x7FFFFFFF & ~(7 << 12)) \
             WHERE (download_status & (1 << 31)) != 0 \
               AND id IN (\
                 SELECT DISTINCT video_id FROM page WHERE ((download_status >> 15) & 7) = 0\
               )"
                .to_owned(),
        ))
        .await?;

        // External sources: mark new page slot as OK (sponsor cut N/A).
        if table_has_column(manager, "you_tube_video", "page_task_status").await? {
            conn.execute(Statement::from_string(
                backend,
                "UPDATE you_tube_video SET page_task_status = \
                   ((page_task_status & ~(7 << 15)) | (7 << 15)) \
                 WHERE ((page_task_status >> 15) & 7) = 0"
                    .to_owned(),
            ))
            .await?;
        }

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        drop_column_if_exists(
            manager,
            "page",
            Page::Table,
            "sponsor_cut_result",
            Page::SponsorCutResult,
        )
        .await
    }
}

#[derive(Iden, Clone, Copy)]
enum Page {
    Table,
    SponsorCutResult,
}

async fn add_column_if_missing<T>(
    manager: &SchemaManager<'_>,
    table_name: &str,
    table: T,
    column_name: &str,
    column_def: ColumnDef,
) -> Result<(), DbErr>
where
    T: IntoIden + Clone + 'static,
{
    if !table_has_column(manager, table_name, column_name).await? {
        manager
            .alter_table(Table::alter().table(table).add_column(column_def).to_owned())
            .await?;
    }
    Ok(())
}

async fn drop_column_if_exists<T, C>(
    manager: &SchemaManager<'_>,
    table_name: &str,
    table: T,
    column_name: &str,
    column: C,
) -> Result<(), DbErr>
where
    T: IntoIden + Clone + 'static,
    C: IntoIden + 'static,
{
    if table_has_column(manager, table_name, column_name).await? {
        manager
            .alter_table(Table::alter().table(table).drop_column(column).to_owned())
            .await?;
    }
    Ok(())
}

async fn table_has_column(manager: &SchemaManager<'_>, table_name: &str, column_name: &str) -> Result<bool, DbErr> {
    let backend = manager.get_connection().get_database_backend();
    let sql = format!(
        "SELECT COUNT(*) FROM pragma_table_info('{}') WHERE name = '{}'",
        table_name.replace('\'', "''"),
        column_name.replace('\'', "''")
    );
    let result = manager
        .get_connection()
        .query_one(Statement::from_string(backend, sql))
        .await?;
    Ok(result
        .and_then(|row| row.try_get_by_index::<i64>(0).ok())
        .unwrap_or(0)
        >= 1)
}
