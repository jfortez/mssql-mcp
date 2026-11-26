//! `object_dependencies`: outgoing and incoming dependencies of one object.
//!
//! Edges come from `sys.sql_expression_dependencies` plus foreign-key column
//! pairs. An unresolved cross-database reference still carries the catalog's
//! referenced schema/entity names, and `is_external` marks it without opening a
//! second connection.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::database::{Database, ReadOutput};
use crate::statement::{Binder, Statement};

use super::model::{CatalogError, ObjectIdentity, ObjectName, ObjectType};
use super::sql::{
    ALL_OBJECT_TYPES, PageQuery, Paging, execute_read, flag, number, object_type, optional_flag,
    optional_number, optional_text, page_limit, parse_counts, required_text, validate_object,
};

/// Input for `object_dependencies`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObjectDependencies {
    /// Table, view, procedure or function whose dependencies are read.
    pub object: ObjectName,
    /// Edges to skip.
    #[serde(default)]
    pub offset: u32,
    /// Edges per page, 1–100; defaults to 100.
    #[serde(default)]
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<u16>,
}

/// A validated `object_dependencies` plan. Building a plan performs no database work.
#[derive(Debug)]
pub struct ObjectDependenciesPlan {
    resolve: Statement,
    counts: Statement,
    pages: Vec<PageQuery>,
    offset: u32,
    limit: u16,
}

impl ObjectDependencies {
    /// Validate inputs and build the catalog statements. No connection opens.
    pub fn plan(&self) -> Result<ObjectDependenciesPlan, &'static str> {
        let (schema, name) = validate_object(&self.object)?;
        let limit = page_limit(self.limit)?;

        let mut binder = Binder::default();
        let schema_parameter = binder.bind(Value::String(schema))?;
        let name_parameter = binder.bind(Value::String(name))?;
        let resolve = Statement::read(
            format!(
                "SELECT CONVERT(nvarchar(20), o.object_id) AS [object_id], \
                 CONVERT(nvarchar(128), s.name) AS [schema], \
                 CONVERT(nvarchar(128), o.name) AS [name], \
                 CONVERT(nvarchar(2), o.type) AS [type], \
                 CONVERT(nvarchar(60), o.type_desc) AS [type_desc] \
                 FROM sys.objects AS o JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
                 WHERE o.is_ms_shipped = 0 AND s.name = {schema_parameter} \
                   AND o.name = {name_parameter} AND o.type IN ({ALL_OBJECT_TYPES})"
            ),
            binder.parameters,
            1,
        );

        let mut binder = Binder::default();
        let object_parameter = binder.bind(Value::from(0i32))?;
        let counts = Statement::read(
            format!(
                "SELECT N'depends_on' AS [direction], CONVERT(bigint, \
                   (SELECT COUNT_BIG(*) FROM sys.sql_expression_dependencies AS d \
                    WHERE d.referencing_id = {object_parameter}) \
                   + (SELECT COUNT_BIG(*) FROM sys.foreign_key_columns AS fkc \
                      WHERE fkc.parent_object_id = {object_parameter})) AS [count] \
                 UNION ALL \
                 SELECT N'depended_on_by' AS [direction], CONVERT(bigint, \
                   (SELECT COUNT_BIG(*) FROM sys.sql_expression_dependencies AS d \
                    WHERE d.referenced_id = {object_parameter}) \
                   + (SELECT COUNT_BIG(*) FROM sys.foreign_key_columns AS fkc \
                      WHERE fkc.referenced_object_id = {object_parameter})) AS [count]"
            ),
            binder.parameters,
            2,
        );

        let mut pages = Vec::with_capacity(2);
        for direction in [
            DependencyDirection::DependsOn,
            DependencyDirection::DependedOnBy,
        ] {
            let mut binder = Binder::default();
            let object_parameter = binder.bind(Value::from(0i32))?;
            let sql = dependency_sql(direction, &object_parameter);
            pages.push(PageQuery::new(direction.kind(), sql, binder));
        }

        Ok(ObjectDependenciesPlan {
            resolve,
            counts,
            pages,
            offset: self.offset,
            limit,
        })
    }
}

impl ObjectDependenciesPlan {
    /// Resolve the object, then return one page of dependency edges.
    pub async fn execute(
        &self,
        database: &Database,
    ) -> Result<ObjectDependenciesPage, CatalogError> {
        let resolve = execute_read(database, self.resolve.clone()).await?;
        let object = resolve_object(&resolve)?;
        let mut counts_statement = self.counts.clone();
        counts_statement.parameters[0] = Value::from(object.object_id);
        let counts = parse_counts(
            execute_read(database, counts_statement).await?,
            &self.pages.iter().map(|page| page.kind).collect::<Vec<_>>(),
        )?;
        let total = counts.iter().sum::<u64>();
        let paging = Paging::new(self.offset, self.limit, total);
        let mut edges = Vec::new();
        let mut truncated = false;
        let mut kind_start = 0u64;
        for (page, count) in self.pages.iter().zip(counts) {
            if let Some((offset, limit)) = paging.range(kind_start, count)? {
                let mut statement = page.statement(offset, limit);
                // The object id is the first and only base parameter; reuse
                // it instead of binding the same value again.
                statement.parameters[0] = Value::from(object.object_id);
                let read = execute_read(database, statement).await?;
                truncated |= read.truncated;
                absorb(page.kind, read, &mut edges)?;
            }
            kind_start += count;
        }
        Ok(ObjectDependenciesPage {
            object,
            offset: self.offset,
            limit: self.limit,
            total,
            next_offset: paging.next_offset()?,
            truncated,
            edges,
        })
    }
}

/// One page of dependency edges.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectDependenciesPage {
    pub object: ObjectIdentity,
    pub offset: u32,
    pub limit: u16,
    pub total: u64,
    pub next_offset: Option<u32>,
    pub truncated: bool,
    pub edges: Vec<DependencyEdge>,
}

/// Direction of an edge relative to the requested object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyDirection {
    DependsOn,
    DependedOnBy,
}

impl DependencyDirection {
    const fn kind(self) -> &'static str {
        match self {
            Self::DependsOn => "depends_on",
            Self::DependedOnBy => "depended_on_by",
        }
    }
}

/// Catalog source of the edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyType {
    SqlExpression,
    ForeignKey,
}

/// One endpoint of a dependency. Unresolved SQL references have no object id or
/// type but keep the catalog's referenced schema/entity names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyObject {
    /// `object:<object_id>` when the catalog resolved the endpoint.
    pub id: Option<String>,
    pub object_id: Option<i32>,
    pub schema: Option<String>,
    pub name: Option<String>,
    pub object_type: Option<ObjectType>,
}

/// One dependency edge. Foreign-key edges are column pairs, so a composite key
/// appears once per `ordinal` and remains complete without one wide cell.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyEdge {
    pub direction: DependencyDirection,
    pub dependency_type: DependencyType,
    pub source: DependencyObject,
    pub target: DependencyObject,
    pub source_column: Option<String>,
    pub target_column: Option<String>,
    pub constraint_name: Option<String>,
    pub ordinal: Option<i32>,
    pub is_schema_bound: Option<bool>,
    pub referenced_database: Option<String>,
    pub is_external: bool,
}

fn dependency_sql(direction: DependencyDirection, object_parameter: &str) -> String {
    let (sql_filter, fk_filter) = match direction {
        DependencyDirection::DependsOn => (
            format!("d.referencing_id = {object_parameter}"),
            format!("fkc.parent_object_id = {object_parameter}"),
        ),
        DependencyDirection::DependedOnBy => (
            format!("d.referenced_id = {object_parameter}"),
            format!("fkc.referenced_object_id = {object_parameter}"),
        ),
    };
    format!(
        "SELECT N'sql' AS [dependency_type], \
         CONVERT(nvarchar(20), src.object_id) AS [source_object_id], \
         CONVERT(nvarchar(128), ss.name) AS [source_schema], \
         CONVERT(nvarchar(128), src.name) AS [source_name], \
         CONVERT(nvarchar(2), src.type) AS [source_type], \
         CONVERT(nvarchar(20), tgt.object_id) AS [target_object_id], \
         CONVERT(nvarchar(128), COALESCE(ts.name, d.referenced_schema_name)) AS [target_schema], \
         CONVERT(nvarchar(128), COALESCE(tgt.name, d.referenced_entity_name)) AS [target_name], \
         CONVERT(nvarchar(2), tgt.type) AS [target_type], \
         CAST(NULL AS nvarchar(128)) AS [source_column], \
         CAST(NULL AS nvarchar(128)) AS [target_column], \
         CAST(NULL AS nvarchar(128)) AS [constraint_name], \
         CAST(NULL AS nvarchar(20)) AS [ordinal], \
         CONVERT(nvarchar(5), d.is_schema_bound_reference) AS [is_schema_bound], \
         CONVERT(nvarchar(128), d.referenced_database_name) AS [referenced_database], \
         CONVERT(nvarchar(5), CASE WHEN d.referenced_database_name IS NOT NULL \
             OR d.referenced_server_name IS NOT NULL THEN 1 ELSE 0 END) AS [is_external] \
         FROM sys.sql_expression_dependencies AS d \
         JOIN sys.objects AS src ON src.object_id = d.referencing_id \
         JOIN sys.schemas AS ss ON ss.schema_id = src.schema_id \
         LEFT JOIN sys.objects AS tgt ON tgt.object_id = d.referenced_id \
         LEFT JOIN sys.schemas AS ts ON ts.schema_id = tgt.schema_id \
         WHERE {sql_filter} \
         UNION ALL \
         SELECT N'foreign_key' AS [dependency_type], \
         CONVERT(nvarchar(20), fk.parent_object_id) AS [source_object_id], \
         CONVERT(nvarchar(128), ps.name) AS [source_schema], \
         CONVERT(nvarchar(128), po.name) AS [source_name], \
         CONVERT(nvarchar(2), po.type) AS [source_type], \
         CONVERT(nvarchar(20), fk.referenced_object_id) AS [target_object_id], \
         CONVERT(nvarchar(128), rs.name) AS [target_schema], \
         CONVERT(nvarchar(128), ro.name) AS [target_name], \
         CONVERT(nvarchar(2), ro.type) AS [target_type], \
         CONVERT(nvarchar(128), pc.name) AS [source_column], \
         CONVERT(nvarchar(128), rc.name) AS [target_column], \
         CONVERT(nvarchar(128), fk.name) AS [constraint_name], \
         CONVERT(nvarchar(20), fkc.constraint_column_id) AS [ordinal], \
         CAST(NULL AS nvarchar(5)) AS [is_schema_bound], \
         CAST(NULL AS nvarchar(128)) AS [referenced_database], \
         CONVERT(nvarchar(5), 0) AS [is_external] \
         FROM sys.foreign_key_columns AS fkc \
         JOIN sys.foreign_keys AS fk ON fk.object_id = fkc.constraint_object_id \
         JOIN sys.objects AS po ON po.object_id = fkc.parent_object_id \
         JOIN sys.schemas AS ps ON ps.schema_id = po.schema_id \
         JOIN sys.columns AS pc ON pc.object_id = fkc.parent_object_id \
           AND pc.column_id = fkc.parent_column_id \
         JOIN sys.objects AS ro ON ro.object_id = fkc.referenced_object_id \
         JOIN sys.schemas AS rs ON rs.schema_id = ro.schema_id \
         JOIN sys.columns AS rc ON rc.object_id = fkc.referenced_object_id \
           AND rc.column_id = fkc.referenced_column_id \
         WHERE po.is_ms_shipped = 0 AND ro.is_ms_shipped = 0 \
           AND {fk_filter} \
         ORDER BY [dependency_type], [target_schema], [target_name], [target_column], \
                  [source_schema], [source_name], [source_column], [constraint_name], [ordinal]"
    )
}

fn resolve_object(read: &ReadOutput) -> Result<ObjectIdentity, CatalogError> {
    if read.columns.len() != 5 {
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
    let code = required_text(row, 3)?;
    Ok(ObjectIdentity {
        id: format!("object:{object_id}"),
        object_id,
        schema: required_text(row, 1)?,
        name: required_text(row, 2)?,
        object_type: object_type(&code).ok_or(CatalogError::UnexpectedResult)?,
        type_desc: required_text(row, 4)?,
    })
}

fn absorb(
    kind: &str,
    read: ReadOutput,
    edges: &mut Vec<DependencyEdge>,
) -> Result<(), CatalogError> {
    if read.columns.len() != 16 {
        return Err(CatalogError::UnexpectedResult);
    }
    let direction = match kind {
        "depends_on" => DependencyDirection::DependsOn,
        "depended_on_by" => DependencyDirection::DependedOnBy,
        _ => return Err(CatalogError::UnexpectedResult),
    };
    for row in read.rows {
        let dependency_type = match required_text(&row, 0)?.as_str() {
            "sql" => DependencyType::SqlExpression,
            "foreign_key" => DependencyType::ForeignKey,
            _ => return Err(CatalogError::UnexpectedResult),
        };
        let source_id = number::<i32>(&row, 1)?;
        let source_type = required_text(&row, 4)?;
        let target_id = optional_number::<i32>(&row, 5)?;
        let target_type = optional_text(&row, 8)
            .map(|code| object_type(&code).ok_or(CatalogError::UnexpectedResult))
            .transpose()?;
        edges.push(DependencyEdge {
            direction,
            dependency_type,
            source: DependencyObject {
                id: Some(format!("object:{source_id}")),
                object_id: Some(source_id),
                schema: Some(required_text(&row, 2)?),
                name: Some(required_text(&row, 3)?),
                object_type: object_type(&source_type),
            },
            target: DependencyObject {
                id: target_id.map(|id| format!("object:{id}")),
                object_id: target_id,
                schema: optional_text(&row, 6),
                name: optional_text(&row, 7),
                object_type: target_type,
            },
            source_column: optional_text(&row, 9),
            target_column: optional_text(&row, 10),
            constraint_name: optional_text(&row, 11),
            ordinal: optional_number::<i32>(&row, 12)?,
            is_schema_bound: optional_flag(&row, 13)?,
            referenced_database: optional_text(&row, 14),
            is_external: flag(&row, 15),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dependencies(value: Value) -> ObjectDependencies {
        serde_json::from_value(value).expect("dependencies input")
    }

    #[test]
    fn both_directions_read_sql_expression_and_foreign_key_edges() {
        let plan = dependencies(serde_json::json!({"object": {"name": "Orders"}}))
            .plan()
            .expect("dependencies plan");
        assert_eq!(
            plan.pages.iter().map(|page| page.kind).collect::<Vec<_>>(),
            ["depends_on", "depended_on_by"]
        );
        for page in &plan.pages {
            assert!(page.sql.contains("sys.sql_expression_dependencies"));
            assert!(page.sql.contains("sys.foreign_key_columns"));
            assert!(page.sql.contains("AS [source_column]"));
            assert!(page.sql.contains("AS [target_column]"));
            assert!(page.sql.contains("AS [referenced_database]"));
            assert!(!page.sql.contains("Orders"));
        }
        assert!(plan.pages[0].sql.contains("d.referencing_id = @P1"));
        assert!(plan.pages[1].sql.contains("d.referenced_id = @P1"));
        assert!(plan.pages[0].sql.contains("fkc.parent_object_id = @P1"));
        assert!(plan.pages[1].sql.contains("fkc.referenced_object_id = @P1"));
        assert!(plan.counts.sql.contains("COUNT_BIG"));
    }

    #[test]
    fn absorb_keeps_unresolved_and_foreign_key_endpoints() {
        let read = ReadOutput {
            columns: vec!["c".into(); 16],
            rows: vec![vec![
                Some("sql".into()),
                Some("10".into()),
                Some("dbo".into()),
                Some("View1".into()),
                Some("V".into()),
                None,
                Some("OtherDb".into()),
                Some("RemoteThing".into()),
                None,
                None,
                None,
                None,
                None,
                Some("1".into()),
                Some("OtherDb".into()),
                Some("1".into()),
            ]],
            truncated: false,
        };
        let mut edges = Vec::new();
        absorb("depends_on", read, &mut edges).expect("sql edge");
        let edge = &edges[0];
        assert_eq!(edge.direction, DependencyDirection::DependsOn);
        assert_eq!(edge.dependency_type, DependencyType::SqlExpression);
        assert_eq!(edge.source.id.as_deref(), Some("object:10"));
        assert!(edge.target.id.is_none());
        assert_eq!(edge.target.schema.as_deref(), Some("OtherDb"));
        assert_eq!(edge.target.name.as_deref(), Some("RemoteThing"));
        assert!(edge.is_external);
    }

    #[test]
    fn missing_resolve_row_is_not_found() {
        let read = ReadOutput {
            columns: vec!["c".into(); 5],
            rows: Vec::new(),
            truncated: false,
        };
        assert_eq!(resolve_object(&read), Err(CatalogError::NotFound));
    }
}
