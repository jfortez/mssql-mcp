//! `table_stats`: approximate row/storage counters and paged index sizes.
//!
//! Counters come from `sys.partitions`/`sys.allocation_units` for base
//! partitions only (`index_id` 0 and 1), so no table is scanned and no
//! `sys.dm_db_*` permission is required. Index sizes page with a bound
//! `OFFSET`/`FETCH NEXT` and expose the next offset explicitly.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::database::{Database, ReadOutput};
use crate::statement::{Binder, Statement};

use super::model::{CatalogError, ObjectIdentity, ObjectName, ObjectType};
use super::sql::{
    PageQuery, Paging, execute_read, flag, number, page_limit, required_text, validate_object,
};

/// Input for `table_stats`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TableStats {
    /// Base table whose row count, storage and indexes are read.
    pub table: ObjectName,
    /// Indexes to skip.
    #[serde(default)]
    pub offset: u32,
    /// Indexes per page, 1–100; defaults to 100.
    #[serde(default)]
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<u16>,
}

/// A validated `table_stats` plan. Building a plan performs no database work.
#[derive(Debug)]
pub struct TableStatsPlan {
    summary: Statement,
    indexes: PageQuery,
    offset: u32,
    limit: u16,
}

impl TableStats {
    /// Validate inputs and build the catalog statements. No connection opens.
    pub fn plan(&self) -> Result<TableStatsPlan, &'static str> {
        let (schema, name) = validate_object(&self.table)?;
        let limit = page_limit(self.limit)?;

        let mut binder = Binder::default();
        let schema_parameter = binder.bind(Value::String(schema.clone()))?;
        let name_parameter = binder.bind(Value::String(name.clone()))?;
        let summary = Statement::read(
            format!(
                "SELECT CONVERT(nvarchar(20), o.object_id) AS [object_id], \
                 CONVERT(nvarchar(128), s.name) AS [schema], \
                 CONVERT(nvarchar(128), o.name) AS [name], \
                 CONVERT(nvarchar(60), o.type_desc) AS [type_desc], \
                 CONVERT(nvarchar(20), ISNULL(ps.row_count, 0)) AS [row_count], \
                 CONVERT(nvarchar(20), ISNULL(sz.reserved_bytes, 0)) AS [reserved_bytes], \
                 CONVERT(nvarchar(20), ISNULL(sz.used_bytes, 0)) AS [used_bytes], \
                 CONVERT(nvarchar(20), ISNULL(sz.data_bytes, 0)) AS [data_bytes], \
                 CONVERT(nvarchar(20), ISNULL(ix.index_count, 0)) AS [index_count] \
                 FROM sys.objects AS o \
                 JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
                 OUTER APPLY (SELECT SUM(p.rows) AS row_count FROM sys.partitions AS p \
                              WHERE p.object_id = o.object_id AND p.index_id IN (0, 1)) AS ps \
                 OUTER APPLY (SELECT \
                     SUM(CONVERT(bigint, au.total_pages)) * 8192 AS reserved_bytes, \
                     SUM(CONVERT(bigint, au.used_pages)) * 8192 AS used_bytes, \
                     SUM(CONVERT(bigint, au.data_pages)) * 8192 AS data_bytes \
                   FROM sys.partitions AS p \
                   LEFT JOIN sys.allocation_units AS au \
                     ON (au.type IN (1, 3) AND au.container_id = p.hobt_id) \
                     OR (au.type = 2 AND au.container_id = p.partition_id) \
                   WHERE p.object_id = o.object_id AND p.index_id IN (0, 1)) AS sz \
                 OUTER APPLY (SELECT COUNT_BIG(*) AS index_count FROM sys.indexes AS i \
                              WHERE i.object_id = o.object_id AND i.index_id > 0) AS ix \
                 WHERE o.is_ms_shipped = 0 AND o.type = N'U' \
                   AND s.name = {schema_parameter} AND o.name = {name_parameter}"
            ),
            binder.parameters,
            1,
        );

        let mut binder = Binder::default();
        let schema_parameter = binder.bind(Value::String(schema))?;
        let name_parameter = binder.bind(Value::String(name))?;
        let indexes = PageQuery::new(
            "index",
            format!(
                "SELECT CONVERT(nvarchar(20), i.index_id) AS [index_id], \
                 CONVERT(nvarchar(128), i.name) AS [name], \
                 CONVERT(nvarchar(60), i.type_desc) AS [type_desc], \
                 CONVERT(nvarchar(5), i.is_unique) AS [is_unique], \
                 CONVERT(nvarchar(5), i.is_primary_key) AS [is_primary_key], \
                 CONVERT(nvarchar(5), i.is_unique_constraint) AS [is_unique_constraint], \
                 CONVERT(nvarchar(5), i.is_disabled) AS [is_disabled], \
                 CONVERT(nvarchar(5), i.is_hypothetical) AS [is_hypothetical], \
                 CONVERT(nvarchar(20), i.fill_factor) AS [fill_factor], \
                 CONVERT(nvarchar(20), ISNULL(sz.reserved_bytes, 0)) AS [reserved_bytes], \
                 CONVERT(nvarchar(20), ISNULL(sz.used_bytes, 0)) AS [used_bytes], \
                 CONVERT(nvarchar(20), ISNULL(sz.data_bytes, 0)) AS [data_bytes] \
                 FROM sys.indexes AS i \
                 JOIN sys.objects AS o ON o.object_id = i.object_id \
                 JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
                 OUTER APPLY (SELECT \
                     SUM(CONVERT(bigint, au.total_pages)) * 8192 AS reserved_bytes, \
                     SUM(CONVERT(bigint, au.used_pages)) * 8192 AS used_bytes, \
                     SUM(CONVERT(bigint, au.data_pages)) * 8192 AS data_bytes \
                   FROM sys.partitions AS p \
                   LEFT JOIN sys.allocation_units AS au \
                     ON (au.type IN (1, 3) AND au.container_id = p.hobt_id) \
                     OR (au.type = 2 AND au.container_id = p.partition_id) \
                   WHERE p.object_id = i.object_id AND p.index_id = i.index_id) AS sz \
                 WHERE i.index_id > 0 AND o.is_ms_shipped = 0 AND o.type = N'U' \
                   AND s.name = {schema_parameter} AND o.name = {name_parameter} \
                 ORDER BY i.index_id"
            ),
            binder,
        );

        Ok(TableStatsPlan {
            summary,
            indexes,
            offset: self.offset,
            limit,
        })
    }
}

impl TableStatsPlan {
    /// Resolve the table, return its counters and one page of index sizes.
    pub async fn execute(&self, database: &Database) -> Result<TableStatsResult, CatalogError> {
        let table = resolve_table(&execute_read(database, self.summary.clone()).await?)?;
        let paging = Paging::new(self.offset, self.limit, table.index_count);
        let mut indexes = Vec::new();
        if let Some((offset, limit)) = paging.range(0, table.index_count)? {
            let read = execute_read(database, self.indexes.statement(offset, limit)).await?;
            absorb_indexes(read, &mut indexes)?;
        }
        Ok(TableStatsResult {
            table: table.identity,
            row_count: table.row_count,
            row_count_approximate: true,
            reserved_bytes: table.reserved_bytes,
            used_bytes: table.used_bytes,
            data_bytes: table.data_bytes,
            index_count: table.index_count,
            index_offset: self.offset,
            index_limit: self.limit,
            next_index_offset: paging.next_offset()?,
            indexes,
        })
    }
}

/// Approximate counters plus one page of index sizes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableStatsResult {
    pub table: ObjectIdentity,
    /// Approximate sum of base-partition rows (`index_id` 0 and 1).
    pub row_count: u64,
    /// Always true: the count comes from catalog partition metadata.
    pub row_count_approximate: bool,
    pub reserved_bytes: u64,
    pub used_bytes: u64,
    pub data_bytes: u64,
    /// Total number of indexes for the table.
    pub index_count: u64,
    pub index_offset: u32,
    pub index_limit: u16,
    pub next_index_offset: Option<u32>,
    pub indexes: Vec<IndexSize>,
}

/// One index and its allocated storage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexSize {
    pub index_id: i32,
    pub name: String,
    pub type_desc: String,
    pub unique: bool,
    pub primary_key: bool,
    pub unique_constraint: bool,
    pub disabled: bool,
    pub hypothetical: bool,
    pub fill_factor: i32,
    pub reserved_bytes: u64,
    pub used_bytes: u64,
    pub data_bytes: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct ResolvedTable {
    identity: ObjectIdentity,
    row_count: u64,
    reserved_bytes: u64,
    used_bytes: u64,
    data_bytes: u64,
    index_count: u64,
}

fn resolve_table(read: &ReadOutput) -> Result<ResolvedTable, CatalogError> {
    if read.columns.len() != 9 {
        return Err(CatalogError::UnexpectedResult);
    }
    if read.rows.is_empty() {
        return Err(CatalogError::NotFound);
    }
    if read.rows.len() != 1 {
        return Err(CatalogError::UnexpectedResult);
    }
    let row = &read.rows[0];
    let object_id = number::<i32>(row, 0)?;
    Ok(ResolvedTable {
        identity: ObjectIdentity {
            id: format!("object:{object_id}"),
            object_id,
            schema: required_text(row, 1)?,
            name: required_text(row, 2)?,
            object_type: ObjectType::Table,
            type_desc: required_text(row, 3)?,
        },
        row_count: number::<u64>(row, 4)?,
        reserved_bytes: number::<u64>(row, 5)?,
        used_bytes: number::<u64>(row, 6)?,
        data_bytes: number::<u64>(row, 7)?,
        index_count: number::<u64>(row, 8)?,
    })
}

fn absorb_indexes(read: ReadOutput, indexes: &mut Vec<IndexSize>) -> Result<(), CatalogError> {
    if read.columns.len() != 12 {
        return Err(CatalogError::UnexpectedResult);
    }
    for row in read.rows {
        indexes.push(IndexSize {
            index_id: number::<i32>(&row, 0)?,
            name: required_text(&row, 1)?,
            type_desc: required_text(&row, 2)?,
            unique: flag(&row, 3),
            primary_key: flag(&row, 4),
            unique_constraint: flag(&row, 5),
            disabled: flag(&row, 6),
            hypothetical: flag(&row, 7),
            fill_factor: number::<i32>(&row, 8)?,
            reserved_bytes: number::<u64>(&row, 9)?,
            used_bytes: number::<u64>(&row, 10)?,
            data_bytes: number::<u64>(&row, 11)?,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(value: Value) -> TableStats {
        serde_json::from_value(value).expect("stats input")
    }

    #[test]
    fn counters_come_from_base_partitions_without_a_table_scan() {
        let plan = stats(serde_json::json!({
            "table": {"schema": "Sales", "name": "Orders'; DROP TABLE x;--"}
        }))
        .plan()
        .expect("stats plan");
        for (sql, parameters) in [
            (plan.summary.sql.as_str(), &plan.summary.parameters),
            (plan.indexes.sql.as_str(), &plan.indexes.parameters),
        ] {
            assert!(!sql.contains("Orders"));
            assert!(!sql.contains("DROP TABLE"));
            assert!(
                parameters
                    .iter()
                    .any(|value| { value.as_str() == Some("Orders'; DROP TABLE x;--") })
            );
        }
        assert!(plan.summary.sql.contains("p.index_id IN (0, 1)"));
        assert!(plan.summary.sql.contains("sys.allocation_units"));
        assert!(plan.summary.sql.contains("AS [row_count]"));
        assert!(plan.summary.sql.contains("AS [index_count]"));
        assert!(plan.indexes.sql.contains("p.index_id = i.index_id"));
        assert!(plan.indexes.sql.contains("ORDER BY i.index_id"));
    }

    #[test]
    fn missing_tables_are_not_found_and_visible_tables_have_counters() {
        let empty = ReadOutput {
            columns: vec!["c".into(); 9],
            rows: Vec::new(),
            truncated: false,
        };
        assert_eq!(resolve_table(&empty), Err(CatalogError::NotFound));

        let read = ReadOutput {
            columns: vec!["c".into(); 9],
            rows: vec![vec![
                Some("7".into()),
                Some("Sales".into()),
                Some("Orders".into()),
                Some("USER_TABLE".into()),
                Some("42".into()),
                Some("65536".into()),
                Some("49152".into()),
                Some("32768".into()),
                Some("3".into()),
            ]],
            truncated: false,
        };
        let table = resolve_table(&read).expect("table");
        assert_eq!(table.identity.id, "object:7");
        assert_eq!(table.identity.object_type, ObjectType::Table);
        assert_eq!(table.row_count, 42);
        assert_eq!(table.reserved_bytes, 65_536);
        assert_eq!(table.used_bytes, 49_152);
        assert_eq!(table.data_bytes, 32_768);
        assert_eq!(table.index_count, 3);
    }

    #[test]
    fn index_sizes_keep_their_flags_and_bytes() {
        let read = ReadOutput {
            columns: vec!["c".into(); 12],
            rows: vec![vec![
                Some("2".into()),
                Some("IX_Orders_Customer".into()),
                Some("NONCLUSTERED".into()),
                Some("1".into()),
                Some("0".into()),
                Some("0".into()),
                Some("0".into()),
                Some("0".into()),
                Some("0".into()),
                Some("8192".into()),
                Some("4096".into()),
                Some("2048".into()),
            ]],
            truncated: false,
        };
        let mut indexes = Vec::new();
        absorb_indexes(read, &mut indexes).expect("index row");
        assert_eq!(indexes.len(), 1);
        assert_eq!(
            indexes[0],
            IndexSize {
                index_id: 2,
                name: "IX_Orders_Customer".into(),
                type_desc: "NONCLUSTERED".into(),
                unique: true,
                primary_key: false,
                unique_constraint: false,
                disabled: false,
                hypothetical: false,
                fill_factor: 0,
                reserved_bytes: 8_192,
                used_bytes: 4_096,
                data_bytes: 2_048,
            }
        );
    }
}
