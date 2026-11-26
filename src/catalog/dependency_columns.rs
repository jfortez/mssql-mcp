//! `dependency_columns`: referenced objects of one module and their columns.
//!
//! The feed has two kinds: one `object` entry per object the source references
//! through `sys.sql_expression_dependencies`, and one `column` entry per column
//! that survives the optional literal `column_hint` filter. Object entries are
//! never omitted because the hint matched nothing: their `matching_column_count`
//! says whether a column entry should be expected, so absence from the column
//! part is different from absence of the object itself.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::database::{Database, ReadOutput};
use crate::statement::{Binder, Statement};

use super::dependencies::resolve_object;
use super::model::{CatalogError, ObjectIdentity, ObjectName, ObjectType};
use super::sql::{
    MODULE_OBJECT_TYPES, PageQuery, Paging, execute_read, flag, number, object_lookup_statement,
    object_type, optional_number, optional_text, page_limit, parse_counts, required_text,
    type_expression, validate_object,
};

const COLUMN_HINT_ERROR: &str =
    "column_hint must contain 1–128 UTF-16 units and no control characters";

/// Input for `dependency_columns`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DependencyColumns {
    /// Procedure, function, view or trigger whose referenced objects are read.
    pub object: ObjectName,
    /// Literal column-name fragment; empty or omitted returns every column.
    #[serde(default)]
    pub column_hint: Option<String>,
    /// Feed entries to skip.
    #[serde(default)]
    pub offset: u32,
    /// Feed entries per page, 1–100; defaults to 100.
    #[serde(default)]
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<u16>,
}

/// A validated `dependency_columns` plan. Building a plan performs no database work.
#[derive(Debug)]
pub struct DependencyColumnsPlan {
    resolve: Statement,
    counts: Statement,
    objects: PageQuery,
    columns: PageQuery,
    hint: Option<String>,
    offset: u32,
    limit: u16,
}

impl DependencyColumns {
    /// Validate inputs and build the catalog statements. No connection opens.
    pub fn plan(&self) -> Result<DependencyColumnsPlan, &'static str> {
        let (schema, name) = validate_object(&self.object)?;
        let hint = column_hint(self.column_hint.as_deref())?;
        let limit = page_limit(self.limit)?;
        let resolve = object_lookup_statement(MODULE_OBJECT_TYPES, &schema, &name)?;

        let hint_value = hint
            .as_ref()
            .map(|hint| Value::String(hint.clone()))
            .unwrap_or(Value::Null);
        let mut binder = Binder::default();
        let object_parameter = binder.bind(Value::from(0i32))?;
        let hint_parameter = binder.bind(hint_value.clone())?;
        let counts = Statement::read(
            format!(
                "SELECT N'object' AS [kind], CONVERT(bigint, \
                   (SELECT COUNT_BIG(*) FROM sys.objects AS o \
                      JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
                     WHERE o.is_ms_shipped = 0 \
                       AND EXISTS (SELECT 1 FROM sys.sql_expression_dependencies AS d \
                                   WHERE d.referencing_id = {object_parameter} \
                                     AND d.referenced_id = o.object_id)) \
                   + (SELECT COUNT_BIG(*) FROM (\
                        SELECT 1 AS [one] \
                        FROM sys.sql_expression_dependencies AS d \
                        WHERE d.referencing_id = {object_parameter} \
                          AND d.referenced_id IS NULL \
                          AND d.referenced_entity_name IS NOT NULL \
                        GROUP BY d.referenced_schema_name, d.referenced_entity_name, \
                                 d.referenced_database_name, d.referenced_server_name\
                     ) AS [unresolved])) AS [count] \
                 UNION ALL \
                 SELECT N'column' AS [kind], CONVERT(bigint, \
                   (SELECT COUNT_BIG(*) FROM sys.objects AS o \
                      JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
                      JOIN sys.columns AS c ON c.object_id = o.object_id \
                     WHERE o.is_ms_shipped = 0 \
                       AND EXISTS (SELECT 1 FROM sys.sql_expression_dependencies AS d \
                                   WHERE d.referencing_id = {object_parameter} \
                                     AND d.referenced_id = o.object_id) \
                       AND ({hint_parameter} IS NULL \
                            OR CHARINDEX({hint_parameter}, CONVERT(nvarchar(128), c.name)) > 0))) \
                   AS [count]"
            ),
            binder.parameters,
            2,
        );

        let mut binder = Binder::default();
        let object_parameter = binder.bind(Value::from(0i32))?;
        let hint_parameter = binder.bind(hint_value.clone())?;
        let objects = PageQuery::new(
            "object",
            format!(
                "WITH [refs] AS (\
                   SELECT o.object_id AS [object_id], s.name AS [schema], o.name AS [name], \
                          CONVERT(nvarchar(2), o.type) AS [type], \
                          CONVERT(nvarchar(60), o.type_desc) AS [type_desc], \
                          CONVERT(nvarchar(5), 0) AS [is_external] \
                   FROM sys.objects AS o \
                   JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
                   WHERE o.is_ms_shipped = 0 \
                     AND EXISTS (SELECT 1 FROM sys.sql_expression_dependencies AS d \
                                 WHERE d.referencing_id = {object_parameter} \
                                   AND d.referenced_id = o.object_id) \
                   UNION ALL \
                   SELECT CAST(NULL AS int) AS [object_id], \
                          d.referenced_schema_name AS [schema], \
                          d.referenced_entity_name AS [name], \
                          CAST(NULL AS nvarchar(2)) AS [type], \
                          CAST(NULL AS nvarchar(60)) AS [type_desc], \
                          CONVERT(nvarchar(5), CASE WHEN d.referenced_database_name IS NOT NULL \
                              OR d.referenced_server_name IS NOT NULL THEN 1 ELSE 0 END) \
                              AS [is_external] \
                   FROM sys.sql_expression_dependencies AS d \
                   WHERE d.referencing_id = {object_parameter} AND d.referenced_id IS NULL \
                     AND d.referenced_entity_name IS NOT NULL \
                   GROUP BY d.referenced_schema_name, d.referenced_entity_name, \
                            d.referenced_database_name, d.referenced_server_name, \
                            CASE WHEN d.referenced_database_name IS NOT NULL \
                              OR d.referenced_server_name IS NOT NULL THEN 1 ELSE 0 END\
                 ) \
                 SELECT CONVERT(nvarchar(20), r.[object_id]) AS [object_id], \
                        CONVERT(nvarchar(128), r.[schema]) AS [schema], \
                        CONVERT(nvarchar(128), r.[name]) AS [name], \
                        CONVERT(nvarchar(2), r.[type]) AS [type], \
                        CONVERT(nvarchar(60), r.[type_desc]) AS [type_desc], \
                        CONVERT(nvarchar(5), r.[is_external]) AS [is_external], \
                        CONVERT(nvarchar(20), ISNULL(stats.[column_count], 0)) AS [column_count], \
                        CONVERT(nvarchar(20), ISNULL(stats.[matching_column_count], 0)) \
                            AS [matching_column_count] \
                 FROM [refs] AS r \
                 OUTER APPLY (SELECT COUNT_BIG(*) AS [column_count], \
                     SUM(CASE WHEN {hint_parameter} IS NULL \
                         OR CHARINDEX({hint_parameter}, CONVERT(nvarchar(128), c.name)) > 0 \
                         THEN 1 ELSE 0 END) AS [matching_column_count] \
                   FROM sys.columns AS c WHERE c.object_id = r.[object_id]) AS [stats] \
                 ORDER BY r.[schema], r.[name], r.[object_id]"
            ),
            binder,
        );

        let mut binder = Binder::default();
        let object_parameter = binder.bind(Value::from(0i32))?;
        let hint_parameter = binder.bind(hint_value)?;
        let data_type = type_expression("c", "t", "b");
        let columns = PageQuery::new(
            "column",
            format!(
                "SELECT CONVERT(nvarchar(20), c.object_id) AS [object_id], \
                        CONVERT(nvarchar(128), s.name) AS [schema], \
                        CONVERT(nvarchar(128), o.name) AS [object_name], \
                        CONVERT(nvarchar(20), c.column_id) AS [column_id], \
                        CONVERT(nvarchar(128), c.name) AS [name], \
                        CONVERT(nvarchar(20), c.column_id) AS [ordinal], \
                        CONVERT(nvarchar(256), {data_type}) AS [data_type], \
                        CONVERT(nvarchar(5), c.is_nullable) AS [is_nullable], \
                        CONVERT(nvarchar(5), c.is_identity) AS [is_identity], \
                        CONVERT(nvarchar(5), c.is_computed) AS [is_computed] \
                 FROM sys.objects AS o \
                 JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
                 JOIN sys.columns AS c ON c.object_id = o.object_id \
                 LEFT JOIN sys.types AS t ON t.user_type_id = c.user_type_id \
                 LEFT JOIN sys.types AS b ON b.user_type_id = c.system_type_id \
                 WHERE o.is_ms_shipped = 0 \
                   AND EXISTS (SELECT 1 FROM sys.sql_expression_dependencies AS d \
                               WHERE d.referencing_id = {object_parameter} \
                                 AND d.referenced_id = o.object_id) \
                   AND ({hint_parameter} IS NULL \
                        OR CHARINDEX({hint_parameter}, CONVERT(nvarchar(128), c.name)) > 0) \
                 ORDER BY s.name, o.name, c.column_id"
            ),
            binder,
        );

        Ok(DependencyColumnsPlan {
            resolve,
            counts,
            objects,
            columns,
            hint,
            offset: self.offset,
            limit,
        })
    }
}

impl DependencyColumnsPlan {
    /// Resolve the source object, then return one page of object and column entries.
    pub async fn execute(
        &self,
        database: &Database,
    ) -> Result<DependencyColumnsPage, CatalogError> {
        let object = resolve_object(&execute_read(database, self.resolve.clone()).await?)?;
        let mut counts_statement = self.counts.clone();
        counts_statement.parameters[0] = Value::from(object.object_id);
        let counts = parse_counts(
            execute_read(database, counts_statement).await?,
            &["object", "column"],
        )?;
        let total = counts.iter().sum::<u64>();
        let paging = Paging::new(self.offset, self.limit, total);
        let mut entries = Vec::new();
        let mut truncated = false;
        let mut kind_start = 0u64;
        for (page, count) in [&self.objects, &self.columns].into_iter().zip(counts) {
            if let Some((offset, limit)) = paging.range(kind_start, count)? {
                let mut statement = page.statement(offset, limit);
                statement.parameters[0] = Value::from(object.object_id);
                let read = execute_read(database, statement).await?;
                truncated |= read.truncated;
                absorb(page.kind, read, &mut entries)?;
            }
            kind_start += count;
        }
        Ok(DependencyColumnsPage {
            object,
            column_hint: self.hint.clone(),
            offset: self.offset,
            limit: self.limit,
            total,
            next_offset: paging.next_offset()?,
            truncated,
            entries,
        })
    }
}

/// One page of referenced objects and their matching columns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyColumnsPage {
    pub object: ObjectIdentity,
    /// Applied literal hint, or `None` when every column is returned.
    pub column_hint: Option<String>,
    pub offset: u32,
    pub limit: u16,
    /// Total object and column feed entries.
    pub total: u64,
    pub next_offset: Option<u32>,
    /// True only when a returned value was clipped at the read cell bound.
    pub truncated: bool,
    pub entries: Vec<DependencyColumnEntry>,
}

/// One `object` or `column` feed entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DependencyColumnEntry {
    Object(ReferencedObjectEntry),
    Column(ReferencedColumnEntry),
}

/// A referenced object. `object_id` is `None` when SQL Server could not resolve
/// the reference locally; such an object still appears here but has no columns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferencedObjectEntry {
    pub id: Option<String>,
    pub object_id: Option<i32>,
    pub schema: Option<String>,
    pub name: Option<String>,
    pub object_type: Option<ObjectType>,
    pub type_desc: Option<String>,
    pub is_external: bool,
    /// Columns the object has in total, regardless of `column_hint`.
    pub column_count: u64,
    /// Columns matching `column_hint`; `0` means the object has no such column.
    pub matching_column_count: u64,
}

/// One column of a referenced object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferencedColumnEntry {
    pub id: String,
    pub object_id: i32,
    pub schema: String,
    pub object: String,
    pub column_id: i32,
    pub name: String,
    pub ordinal: i32,
    pub data_type: String,
    pub nullable: bool,
    pub identity: bool,
    pub computed: bool,
}

fn column_hint(hint: Option<&str>) -> Result<Option<String>, &'static str> {
    let Some(hint) = hint else {
        return Ok(None);
    };
    if hint.trim().is_empty() {
        return Ok(None);
    }
    if hint.encode_utf16().count() > 128 || hint.chars().any(char::is_control) {
        return Err(COLUMN_HINT_ERROR);
    }
    Ok(Some(hint.to_owned()))
}

fn absorb(
    kind: &str,
    read: ReadOutput,
    entries: &mut Vec<DependencyColumnEntry>,
) -> Result<(), CatalogError> {
    match kind {
        "object" => {
            if read.columns.len() != 8 {
                return Err(CatalogError::UnexpectedResult);
            }
            for row in read.rows {
                let object_id = optional_number::<i32>(&row, 0)?;
                let object_type = optional_text(&row, 3)
                    .map(|code| object_type(&code).ok_or(CatalogError::UnexpectedResult))
                    .transpose()?;
                entries.push(DependencyColumnEntry::Object(ReferencedObjectEntry {
                    id: object_id.map(|id| format!("object:{id}")),
                    object_id,
                    schema: optional_text(&row, 1),
                    name: optional_text(&row, 2),
                    object_type,
                    type_desc: optional_text(&row, 4),
                    is_external: flag(&row, 5),
                    column_count: number::<u64>(&row, 6)?,
                    matching_column_count: number::<u64>(&row, 7)?,
                }));
            }
        }
        "column" => {
            if read.columns.len() != 10 {
                return Err(CatalogError::UnexpectedResult);
            }
            for row in read.rows {
                let object_id = number::<i32>(&row, 0)?;
                let column_id = number::<i32>(&row, 3)?;
                entries.push(DependencyColumnEntry::Column(ReferencedColumnEntry {
                    id: format!("column:{object_id}:{column_id}"),
                    object_id,
                    schema: required_text(&row, 1)?,
                    object: required_text(&row, 2)?,
                    column_id,
                    name: required_text(&row, 4)?,
                    ordinal: number::<i32>(&row, 5)?,
                    data_type: required_text(&row, 6)?,
                    nullable: flag(&row, 7),
                    identity: flag(&row, 8),
                    computed: flag(&row, 9),
                }));
            }
        }
        _ => return Err(CatalogError::UnexpectedResult),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dependency_columns(value: Value) -> DependencyColumns {
        serde_json::from_value(value).expect("dependency columns input")
    }

    fn row(cells: &[&str]) -> Vec<Option<String>> {
        cells.iter().map(|cell| Some((*cell).to_owned())).collect()
    }

    #[test]
    fn empty_hint_means_all_columns_and_a_hint_stays_literal() {
        for input in [
            serde_json::json!({"object": {"name": "Items"}}),
            serde_json::json!({"object": {"name": "Items"}, "column_hint": ""}),
            serde_json::json!({"object": {"name": "Items"}, "column_hint": "   "}),
        ] {
            let plan = dependency_columns(input).plan().expect("plan");
            assert_eq!(plan.hint, None);
            assert_eq!(plan.counts.parameters[1], Value::Null);
            assert_eq!(plan.objects.parameters[1], Value::Null);
            assert_eq!(plan.columns.parameters[1], Value::Null);
        }

        let hint = "CreadoEn%_[";
        let plan = dependency_columns(serde_json::json!({
            "object": {"name": "Items"},
            "column_hint": hint,
        }))
        .plan()
        .expect("hinted plan");
        assert_eq!(plan.hint.as_deref(), Some(hint));
        for (sql, parameters) in [
            (plan.counts.sql.as_str(), &plan.counts.parameters),
            (plan.objects.sql.as_str(), &plan.objects.parameters),
            (plan.columns.sql.as_str(), &plan.columns.parameters),
        ] {
            assert!(!sql.contains(hint), "caller text reached SQL: {sql}");
            assert!(parameters.iter().any(|value| value.as_str() == Some(hint)));
        }

        for hint in ["bad\u{7}name".to_owned(), "x".repeat(129)] {
            let input = dependency_columns(serde_json::json!({
                "object": {"name": "Items"},
                "column_hint": hint,
            }));
            assert_eq!(input.plan().err(), Some(COLUMN_HINT_ERROR));
        }
    }

    #[test]
    fn an_object_with_no_matching_column_is_still_reported() {
        let mut entries = Vec::new();
        absorb(
            "object",
            ReadOutput {
                columns: vec!["c".into(); 8],
                rows: vec![
                    row(&["7", "dbo", "Orders", "U", "USER_TABLE", "0", "4", "0"]),
                    row(&["9", "dbo", "Customers", "U", "USER_TABLE", "0", "3", "1"]),
                ],
                truncated: false,
            },
            &mut entries,
        )
        .expect("object entries");
        absorb(
            "column",
            ReadOutput {
                columns: vec!["c".into(); 10],
                rows: vec![row(&[
                    "9",
                    "dbo",
                    "Customers",
                    "1",
                    "CreadoEn",
                    "1",
                    "datetime2(3)",
                    "0",
                    "0",
                    "0",
                ])],
                truncated: false,
            },
            &mut entries,
        )
        .expect("column entries");

        let value = serde_json::to_value(&entries).expect("serialize entries");
        let entries = value.as_array().expect("entry array");
        let orders = entries
            .iter()
            .find(|entry| entry["kind"] == "object" && entry["name"] == "Orders")
            .expect("Orders must be reported even without a matching column");
        assert_eq!(orders["matching_column_count"], 0);
        assert_eq!(orders["column_count"], 4);
        assert!(
            !entries
                .iter()
                .any(|entry| entry["kind"] == "column" && entry["object"] == "Orders"),
            "Orders must not contribute a matching column: {value}"
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry["kind"] == "column" && entry["object"] == "Customers"),
            "Customers carries the hint: {value}"
        );
        assert!(
            !entries.iter().any(|entry| entry["name"] == "Invoices"),
            "an object that is not referenced must be absent: {value}"
        );
    }

    #[test]
    fn unresolved_references_stay_visible_without_columns() {
        let mut entries = Vec::new();
        absorb(
            "object",
            ReadOutput {
                columns: vec!["c".into(); 8],
                rows: vec![vec![
                    None,
                    Some("OtherDb".into()),
                    Some("RemoteThing".into()),
                    None,
                    None,
                    Some("1".into()),
                    Some("0".into()),
                    Some("0".into()),
                ]],
                truncated: false,
            },
            &mut entries,
        )
        .expect("unresolved object");
        let DependencyColumnEntry::Object(object) = &entries[0] else {
            panic!("expected an object entry");
        };
        assert_eq!(object.id, None);
        assert_eq!(object.object_id, None);
        assert_eq!(object.schema.as_deref(), Some("OtherDb"));
        assert_eq!(object.name.as_deref(), Some("RemoteThing"));
        assert_eq!(object.object_type, None);
        assert!(object.is_external);
        assert_eq!(object.column_count, 0);
        assert_eq!(object.matching_column_count, 0);
    }

    #[test]
    fn plan_validates_the_source_before_connecting() {
        assert!(
            dependency_columns(serde_json::json!({"object": {"name": "Items"}}))
                .plan()
                .is_ok()
        );
        for (input, message) in [
            (
                serde_json::json!({"object": {"name": "bad\u{7}name"}}),
                "identifier must contain 1–128 UTF-16 units and no control characters",
            ),
            (
                serde_json::json!({"object": {"name": "Items"}, "limit": 0}),
                "limit must be between 1 and 100",
            ),
            (
                serde_json::json!({"object": {"name": "Items"}, "limit": 101}),
                "limit must be between 1 and 100",
            ),
        ] {
            assert_eq!(dependency_columns(input).plan().err(), Some(message));
        }
    }
}
