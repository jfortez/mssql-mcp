//! `schema_map`: a normalized, paged catalog feed.
//!
//! The feed has one row per catalog entity. A page is a window over a fixed
//! kind order (schema, object, column, key, key column, foreign key, foreign-key
//! column, index, index column, parameter) and then the numeric catalog IDs.
//! Counts are read from catalog metadata; each kind's page is fetched with a
//! bound `OFFSET`/`FETCH NEXT`, so a page never silently drops child rows.
//! Long default, computed and filter expressions are the only clipped values,
//! and both the record and the page expose that fact explicitly.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::database::{Database, ReadOutput};
use crate::statement::{Binder, Statement};

use super::model::{
    CatalogEntry, CatalogError, ColumnEntry, ForeignKeyColumnEntry, ForeignKeyEntry, Include,
    IndexColumnEntry, IndexEntry, KeyColumnEntry, KeyEntry, KeyType, ObjectEntry, ObjectName,
    ParameterEntry, SchemaEntry,
};
use crate::statement::validate_identifier;

use super::sql::{
    PageQuery, Paging, execute_read, flag, number, object_type, optional_number, optional_text,
    page_limit, parse_counts, required_text, type_expression, validate_schema,
};

const INCLUDE_ERROR: &str =
    "include must contain at least one of tables, views, procedures, functions";

/// Input for `schema_map`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SchemaMap {
    /// Restrict the map to this schema.
    #[serde(default)]
    pub schema: Option<String>,
    /// Object families to include; defaults to all four.
    #[serde(default)]
    pub include: Option<Vec<Include>>,
    /// Restrict the map to one table or view (and its children).
    #[serde(default)]
    pub table: Option<ObjectName>,
    /// Feed rows to skip.
    #[serde(default)]
    pub offset: u32,
    /// Feed rows per page, 1–100; defaults to 100.
    #[serde(default)]
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<u16>,
}

/// A validated `schema_map` plan. Building a plan performs no database work.
#[derive(Debug)]
pub struct SchemaMapPlan {
    counts: Statement,
    pages: Vec<PageQuery>,
    offset: u32,
    limit: u16,
}

impl SchemaMap {
    /// Validate inputs and build the catalog statements. No connection opens.
    pub fn plan(&self) -> Result<SchemaMapPlan, &'static str> {
        validate_schema(self.schema.as_deref())?;
        let table = match &self.table {
            Some(table) => {
                let table_schema = table
                    .schema
                    .as_deref()
                    .or(self.schema.as_deref())
                    .unwrap_or("dbo");
                validate_identifier(table_schema)?;
                validate_identifier(&table.name)?;
                Some((table_schema.to_owned(), table.name.clone()))
            }
            None => None,
        };
        if let Some(table) = &self.table
            && let (Some(schema), Some(table_schema)) = (&self.schema, &table.schema)
            && schema != table_schema
        {
            return Err("schema must match the table schema");
        }
        let limit = page_limit(self.limit)?;
        let filter = MapFilter {
            schema: self.schema.clone(),
            table,
            include: Includes::new(self.include.as_deref())?,
        };
        let kinds = Kind::selected(&filter);
        let kind_count = kinds.len();
        let mut count_binder = Binder::default();
        let mut count_parts = Vec::with_capacity(kind_count);
        let mut pages = Vec::with_capacity(kind_count);
        for kind in kinds {
            let count_sql = kind.build(&filter, &mut count_binder)?;
            count_parts.push(format!(
                "SELECT N'{}' AS [kind], COUNT_BIG(*) AS [count] {}",
                kind.name(),
                count_sql.from_where
            ));

            let mut binder = Binder::default();
            let page_sql = kind.build(&filter, &mut binder)?;
            pages.push(PageQuery::new(
                kind.name(),
                format!(
                    "SELECT {} {} ORDER BY {}",
                    page_sql.projection, page_sql.from_where, page_sql.order_by
                ),
                binder,
            ));
        }
        let counts = Statement::read(
            count_parts.join(" UNION ALL "),
            count_binder.parameters,
            kind_count,
        );
        Ok(SchemaMapPlan {
            counts,
            pages,
            offset: self.offset,
            limit,
        })
    }
}

impl SchemaMapPlan {
    /// Execute the planned catalog reads and return one page of the feed.
    pub async fn execute(&self, database: &Database) -> Result<SchemaMapPage, CatalogError> {
        let counts = parse_counts(
            execute_read(database, self.counts.clone()).await?,
            &self.pages.iter().map(|page| page.kind).collect::<Vec<_>>(),
        )?;
        let total = counts.iter().sum::<u64>();
        let paging = Paging::new(self.offset, self.limit, total);
        let mut entries = Vec::new();
        let mut truncated = false;
        let mut kind_start = 0u64;
        for (page, count) in self.pages.iter().zip(counts) {
            if let Some((offset, limit)) = paging.range(kind_start, count)? {
                let read = execute_read(database, page.statement(offset, limit)).await?;
                truncated |= read.truncated;
                absorb(page.kind, read, &mut entries, &mut truncated)?;
            }
            kind_start += count;
        }
        Ok(SchemaMapPage {
            offset: self.offset,
            limit: self.limit,
            total,
            next_offset: paging.next_offset()?,
            truncated,
            entries,
        })
    }
}

/// One page of the normalized schema map.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaMapPage {
    /// Requested feed offset.
    pub offset: u32,
    /// Requested feed page size.
    pub limit: u16,
    /// Total number of feed rows for the selected scope.
    pub total: u64,
    /// Offset of the next page, or `None` when this is the last page.
    pub next_offset: Option<u32>,
    /// True only when a stored expression was cut at the read cell bound.
    /// More rows are signalled by `next_offset`, never by this flag.
    pub truncated: bool,
    /// Feed rows in the fixed kind order described on [`CatalogEntry`].
    pub entries: Vec<CatalogEntry>,
}

#[derive(Clone, Copy, Debug)]
struct Includes {
    tables: bool,
    views: bool,
    procedures: bool,
    functions: bool,
}

impl Includes {
    fn new(list: Option<&[Include]>) -> Result<Self, &'static str> {
        let Some(list) = list else {
            return Ok(Self {
                tables: true,
                views: true,
                procedures: true,
                functions: true,
            });
        };
        if list.is_empty() {
            return Err(INCLUDE_ERROR);
        }
        let selected = list.iter().copied().collect::<BTreeSet<_>>();
        Ok(Self {
            tables: selected.contains(&Include::Tables),
            views: selected.contains(&Include::Views),
            procedures: selected.contains(&Include::Procedures),
            functions: selected.contains(&Include::Functions),
        })
    }

    fn object_types(self) -> String {
        let mut types = Vec::new();
        if self.tables {
            types.push("N'U'");
        }
        if self.views {
            types.push("N'V'");
        }
        types.extend(self.routine_type_list());
        types.join(", ")
    }

    fn table_types(self) -> String {
        let mut types = Vec::new();
        if self.tables {
            types.push("N'U'");
        }
        if self.views {
            types.push("N'V'");
        }
        types.join(", ")
    }

    fn routine_types(self) -> String {
        self.routine_type_list().join(", ")
    }

    fn routine_type_list(self) -> Vec<&'static str> {
        let mut types = Vec::new();
        if self.procedures {
            types.push("N'P'");
            types.push("N'PC'");
        }
        if self.functions {
            types.extend(["N'FN'", "N'IF'", "N'TF'", "N'FS'", "N'FT'", "N'AF'"]);
        }
        types
    }

    fn has_table_or_view(self) -> bool {
        self.tables || self.views
    }

    fn has_routine(self) -> bool {
        self.procedures || self.functions
    }
}

struct MapFilter {
    schema: Option<String>,
    table: Option<(String, String)>,
    include: Includes,
}

impl MapFilter {
    /// Object types the map may return: a table filter excludes routines.
    fn table_types(&self) -> String {
        if self.table.is_some() {
            self.include.table_types()
        } else {
            self.include.object_types()
        }
    }

    fn predicate(
        &self,
        binder: &mut Binder,
        schema: &str,
        name: &str,
        type_column: &str,
        types: &str,
    ) -> Result<String, &'static str> {
        let mut parts = vec![format!("{type_column} IN ({types})")];
        if let Some(schema_filter) = &self.schema {
            let parameter = binder.bind(Value::String(schema_filter.clone()))?;
            parts.push(format!("{schema} = {parameter}"));
        }
        if let Some((table_schema, table_name)) = &self.table {
            let schema_parameter = binder.bind(Value::String(table_schema.clone()))?;
            let name_parameter = binder.bind(Value::String(table_name.clone()))?;
            parts.push(format!(
                "({schema} = {schema_parameter} AND {name} = {name_parameter})"
            ));
        }
        Ok(parts.join(" AND "))
    }

    fn foreign_key_predicate(
        &self,
        binder: &mut Binder,
        parent_schema: &str,
        parent_table: &str,
        referenced_schema: &str,
        referenced_table: &str,
    ) -> Result<String, &'static str> {
        let mut parts = Vec::new();
        if let Some(schema_filter) = &self.schema {
            let parameter = binder.bind(Value::String(schema_filter.clone()))?;
            parts.push(format!(
                "({parent_schema} = {parameter} OR {referenced_schema} = {parameter})"
            ));
        }
        if let Some((table_schema, table_name)) = &self.table {
            let schema_parameter = binder.bind(Value::String(table_schema.clone()))?;
            let name_parameter = binder.bind(Value::String(table_name.clone()))?;
            parts.push(format!(
                "(({parent_schema} = {schema_parameter} AND {parent_table} = {name_parameter}) \
                 OR ({referenced_schema} = {schema_parameter} AND {referenced_table} = {name_parameter}))"
            ));
        }
        Ok(if parts.is_empty() {
            "1 = 1".to_owned()
        } else {
            parts.join(" AND ")
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Schema,
    Object,
    Column,
    Key,
    KeyColumn,
    ForeignKey,
    ForeignKeyColumn,
    Index,
    IndexColumn,
    Parameter,
}

impl Kind {
    const fn name(self) -> &'static str {
        match self {
            Self::Schema => "schema",
            Self::Object => "object",
            Self::Column => "column",
            Self::Key => "key",
            Self::KeyColumn => "key_column",
            Self::ForeignKey => "foreign_key",
            Self::ForeignKeyColumn => "foreign_key_column",
            Self::Index => "index",
            Self::IndexColumn => "index_column",
            Self::Parameter => "parameter",
        }
    }

    fn selected(filter: &MapFilter) -> Vec<Self> {
        let mut kinds = vec![Self::Schema, Self::Object];
        // A table filter names one table or view, so routine kinds cannot match.
        let routines = filter.include.has_routine() && filter.table.is_none();
        if filter.include.has_table_or_view() {
            kinds.push(Self::Column);
        }
        if filter.include.tables {
            kinds.push(Self::Key);
            kinds.push(Self::KeyColumn);
            kinds.push(Self::ForeignKey);
            kinds.push(Self::ForeignKeyColumn);
        }
        if filter.include.has_table_or_view() {
            kinds.push(Self::Index);
            kinds.push(Self::IndexColumn);
        }
        if routines {
            kinds.push(Self::Parameter);
        }
        kinds
    }

    fn build(self, filter: &MapFilter, binder: &mut Binder) -> Result<KindSql, &'static str> {
        match self {
            Self::Schema => schema_sql(filter, binder),
            Self::Object => object_sql(filter, binder),
            Self::Column => column_sql(filter, binder),
            Self::Key => key_sql(filter, binder),
            Self::KeyColumn => key_column_sql(filter, binder),
            Self::ForeignKey => foreign_key_sql(filter, binder),
            Self::ForeignKeyColumn => foreign_key_column_sql(filter, binder),
            Self::Index => index_sql(filter, binder),
            Self::IndexColumn => index_column_sql(filter, binder),
            Self::Parameter => parameter_sql(filter, binder),
        }
    }
}

struct KindSql {
    projection: String,
    from_where: String,
    order_by: String,
}

fn schema_sql(filter: &MapFilter, binder: &mut Binder) -> Result<KindSql, &'static str> {
    let schema = binder.bind(
        filter
            .schema
            .as_ref()
            .map(|schema| Value::String(schema.clone()))
            .unwrap_or(Value::Null),
    )?;
    let table_schema = binder.bind(
        filter
            .table
            .as_ref()
            .map(|(schema, _)| Value::String(schema.clone()))
            .unwrap_or(Value::Null),
    )?;
    let table_name = binder.bind(
        filter
            .table
            .as_ref()
            .map(|(_, name)| Value::String(name.clone()))
            .unwrap_or(Value::Null),
    )?;
    let types = filter.table_types();
    let mut exists = vec![
        "o.is_ms_shipped = 0".to_owned(),
        format!("o.type IN ({types})"),
        format!("({schema} IS NULL OR s.name = {schema})"),
    ];
    if filter.table.is_some() {
        exists.push(format!(
            "(s.name = {table_schema} AND o.name = {table_name})"
        ));
    }
    let from_where = format!(
        "FROM sys.schemas AS s \
         WHERE s.name NOT IN (N'sys', N'INFORMATION_SCHEMA') \
           AND (({schema} IS NULL AND {table_schema} IS NULL) \
             OR ({table_schema} IS NULL AND s.name = {schema}) \
             OR EXISTS (SELECT 1 FROM sys.objects AS o \
                        WHERE o.schema_id = s.schema_id AND {}))",
        exists.join(" AND ")
    );
    Ok(KindSql {
        projection: "CONVERT(nvarchar(20), s.schema_id) AS [schema_id], \
                     CONVERT(nvarchar(128), s.name) AS [name]"
            .to_owned(),
        from_where,
        order_by: "s.schema_id".to_owned(),
    })
}

fn object_sql(filter: &MapFilter, binder: &mut Binder) -> Result<KindSql, &'static str> {
    let types = filter.table_types();
    let predicate = filter.predicate(binder, "s.name", "o.name", "o.type", &types)?;
    let from_where = format!(
        "FROM sys.objects AS o \
         JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
         OUTER APPLY (SELECT SUM(p.rows) AS row_count FROM sys.partitions AS p \
                      WHERE p.object_id = o.object_id AND p.index_id IN (0, 1)) AS ps \
         OUTER APPLY (SELECT TOP (1) user_type_id FROM sys.parameters \
                      WHERE object_id = o.object_id AND parameter_id = 0) AS rp \
         WHERE o.is_ms_shipped = 0 AND {predicate}"
    );
    Ok(KindSql {
        projection: "CONVERT(nvarchar(20), o.object_id) AS [object_id], \
                     CONVERT(nvarchar(20), o.schema_id) AS [schema_id], \
                     CONVERT(nvarchar(128), s.name) AS [schema], \
                     CONVERT(nvarchar(128), o.name) AS [name], \
                     CONVERT(nvarchar(2), o.type) AS [type], \
                     CONVERT(nvarchar(60), o.type_desc) AS [type_desc], \
                     CONVERT(nvarchar(128), CASE WHEN o.type IN (N'IF', N'TF') THEN N'table' \
                         ELSE TYPE_NAME(rp.user_type_id) END) AS [return_type], \
                     CONVERT(nvarchar(20), CASE WHEN o.type = N'U' THEN ISNULL(ps.row_count, 0) \
                         ELSE NULL END) AS [row_count]"
            .to_owned(),
        from_where,
        order_by: "o.object_id".to_owned(),
    })
}

fn column_sql(filter: &MapFilter, binder: &mut Binder) -> Result<KindSql, &'static str> {
    let types = filter.include.table_types();
    let predicate = filter.predicate(binder, "s.name", "o.name", "o.type", &types)?;
    let data_type = type_expression("c", "t", "b");
    let from_where = format!(
        "FROM sys.columns AS c \
         JOIN sys.objects AS o ON o.object_id = c.object_id \
         JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
         LEFT JOIN sys.types AS t ON t.user_type_id = c.user_type_id \
         LEFT JOIN sys.types AS b ON b.user_type_id = c.system_type_id \
         LEFT JOIN sys.default_constraints AS dc \
           ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id \
         LEFT JOIN sys.computed_columns AS cc \
           ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
         WHERE o.is_ms_shipped = 0 AND {predicate}"
    );
    Ok(KindSql {
        projection: format!(
            "CONVERT(nvarchar(20), c.object_id) AS [object_id], \
             CONVERT(nvarchar(20), c.column_id) AS [column_id], \
             CONVERT(nvarchar(128), c.name) AS [name], \
             CONVERT(nvarchar(20), c.column_id) AS [ordinal], \
             CONVERT(nvarchar(256), {data_type}) AS [data_type], \
             CONVERT(nvarchar(5), c.is_nullable) AS [is_nullable], \
             CONVERT(nvarchar(5), c.is_identity) AS [is_identity], \
             CONVERT(nvarchar(5), c.is_computed) AS [is_computed], \
             CONVERT(nvarchar(5), ISNULL(cc.is_persisted, 0)) AS [is_persisted], \
             LEFT(CONVERT(nvarchar(max), dc.definition), 1025) AS [default_definition], \
             CONVERT(nvarchar(5), CASE WHEN dc.definition IS NOT NULL \
                 AND DATALENGTH(dc.definition) / 2 > 1024 THEN 1 ELSE 0 END) AS [default_truncated], \
             LEFT(CONVERT(nvarchar(max), cc.definition), 1025) AS [computed_definition], \
             CONVERT(nvarchar(5), CASE WHEN cc.definition IS NOT NULL \
                 AND DATALENGTH(cc.definition) / 2 > 1024 THEN 1 ELSE 0 END) AS [computed_truncated]"
        ),
        from_where,
        order_by: "c.object_id, c.column_id".to_owned(),
    })
}

fn key_sql(filter: &MapFilter, binder: &mut Binder) -> Result<KindSql, &'static str> {
    let predicate = filter.predicate(binder, "s.name", "o.name", "o.type", "N'U'")?;
    Ok(KindSql {
        projection: "CONVERT(nvarchar(20), kc.object_id) AS [key_id], \
                     CONVERT(nvarchar(20), kc.parent_object_id) AS [object_id], \
                     CONVERT(nvarchar(20), kc.unique_index_id) AS [index_id], \
                     CONVERT(nvarchar(128), kc.name) AS [name], \
                     CONVERT(nvarchar(2), kc.type) AS [type], \
                     CONVERT(nvarchar(60), kc.type_desc) AS [type_desc], \
                     CONVERT(nvarchar(5), kc.is_system_named) AS [is_system_named]"
            .to_owned(),
        from_where: format!(
            "FROM sys.key_constraints AS kc \
             JOIN sys.objects AS o ON o.object_id = kc.parent_object_id \
             JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
             WHERE o.is_ms_shipped = 0 AND {predicate}"
        ),
        order_by: "kc.object_id".to_owned(),
    })
}

fn key_column_sql(filter: &MapFilter, binder: &mut Binder) -> Result<KindSql, &'static str> {
    let predicate = filter.predicate(binder, "s.name", "o.name", "o.type", "N'U'")?;
    Ok(KindSql {
        projection: "CONVERT(nvarchar(20), kc.object_id) AS [key_id], \
                     CONVERT(nvarchar(20), kc.parent_object_id) AS [object_id], \
                     CONVERT(nvarchar(20), kc.unique_index_id) AS [index_id], \
                     CONVERT(nvarchar(20), c.column_id) AS [column_id], \
                     CONVERT(nvarchar(128), c.name) AS [name], \
                     CONVERT(nvarchar(20), ic.key_ordinal) AS [ordinal], \
                     CONVERT(nvarchar(5), ic.is_descending_key) AS [is_descending]"
            .to_owned(),
        from_where: format!(
            "FROM sys.key_constraints AS kc \
             JOIN sys.index_columns AS ic \
               ON ic.object_id = kc.parent_object_id AND ic.index_id = kc.unique_index_id \
               AND ic.key_ordinal > 0 \
             JOIN sys.columns AS c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
             JOIN sys.objects AS o ON o.object_id = kc.parent_object_id \
             JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
             WHERE o.is_ms_shipped = 0 AND {predicate}"
        ),
        order_by: "kc.object_id, ic.key_ordinal, c.column_id".to_owned(),
    })
}

fn foreign_key_sql(filter: &MapFilter, binder: &mut Binder) -> Result<KindSql, &'static str> {
    let predicate =
        filter.foreign_key_predicate(binder, "ps.name", "po.name", "rs.name", "ro.name")?;
    Ok(KindSql {
        projection: "CONVERT(nvarchar(20), fk.object_id) AS [foreign_key_id], \
                     CONVERT(nvarchar(128), fk.name) AS [name], \
                     CONVERT(nvarchar(20), fk.parent_object_id) AS [parent_object_id], \
                     CONVERT(nvarchar(128), ps.name) AS [parent_schema], \
                     CONVERT(nvarchar(128), po.name) AS [parent_table], \
                     CONVERT(nvarchar(20), fk.referenced_object_id) AS [referenced_object_id], \
                     CONVERT(nvarchar(128), rs.name) AS [referenced_schema], \
                     CONVERT(nvarchar(128), ro.name) AS [referenced_table], \
                     CONVERT(nvarchar(5), fk.is_disabled) AS [is_disabled], \
                     CONVERT(nvarchar(5), fk.is_not_trusted) AS [is_not_trusted]"
            .to_owned(),
        from_where: format!(
            "FROM sys.foreign_keys AS fk \
             JOIN sys.objects AS po ON po.object_id = fk.parent_object_id \
             JOIN sys.schemas AS ps ON ps.schema_id = po.schema_id \
             JOIN sys.objects AS ro ON ro.object_id = fk.referenced_object_id \
             JOIN sys.schemas AS rs ON rs.schema_id = ro.schema_id \
             WHERE po.is_ms_shipped = 0 AND ro.is_ms_shipped = 0 \
               AND {predicate}"
        ),
        order_by: "fk.object_id".to_owned(),
    })
}

fn foreign_key_column_sql(
    filter: &MapFilter,
    binder: &mut Binder,
) -> Result<KindSql, &'static str> {
    let predicate =
        filter.foreign_key_predicate(binder, "ps.name", "po.name", "rs.name", "ro.name")?;
    Ok(KindSql {
        projection: "CONVERT(nvarchar(20), fkc.constraint_object_id) AS [foreign_key_id], \
                     CONVERT(nvarchar(20), fkc.constraint_column_id) AS [ordinal], \
                     CONVERT(nvarchar(20), fkc.parent_object_id) AS [parent_object_id], \
                     CONVERT(nvarchar(20), fkc.parent_column_id) AS [parent_column_id], \
                     CONVERT(nvarchar(128), pc.name) AS [parent_column], \
                     CONVERT(nvarchar(20), fkc.referenced_object_id) AS [referenced_object_id], \
                     CONVERT(nvarchar(20), fkc.referenced_column_id) AS [referenced_column_id], \
                     CONVERT(nvarchar(128), rc.name) AS [referenced_column]"
            .to_owned(),
        from_where: format!(
            "FROM sys.foreign_key_columns AS fkc \
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
               AND {predicate}"
        ),
        order_by: "fkc.constraint_object_id, fkc.constraint_column_id".to_owned(),
    })
}

fn index_sql(filter: &MapFilter, binder: &mut Binder) -> Result<KindSql, &'static str> {
    let types = filter.include.table_types();
    let predicate = filter.predicate(binder, "s.name", "o.name", "o.type", &types)?;
    Ok(KindSql {
        projection: "CONVERT(nvarchar(20), i.object_id) AS [object_id], \
                     CONVERT(nvarchar(20), i.index_id) AS [index_id], \
                     CONVERT(nvarchar(128), i.name) AS [name], \
                     CONVERT(nvarchar(60), i.type_desc) AS [type_desc], \
                     CONVERT(nvarchar(5), i.is_unique) AS [is_unique], \
                     CONVERT(nvarchar(5), i.is_primary_key) AS [is_primary_key], \
                     CONVERT(nvarchar(5), i.is_unique_constraint) AS [is_unique_constraint], \
                     CONVERT(nvarchar(5), i.is_disabled) AS [is_disabled], \
                     CONVERT(nvarchar(5), i.is_hypothetical) AS [is_hypothetical], \
                     CONVERT(nvarchar(5), i.has_filter) AS [has_filter], \
                     LEFT(CONVERT(nvarchar(max), i.filter_definition), 1025) AS [filter_definition], \
                     CONVERT(nvarchar(5), CASE WHEN i.filter_definition IS NOT NULL \
                         AND DATALENGTH(i.filter_definition) / 2 > 1024 THEN 1 ELSE 0 END) \
                         AS [filter_truncated], \
                     CONVERT(nvarchar(20), i.fill_factor) AS [fill_factor]"
            .to_owned(),
        from_where: format!(
            "FROM sys.indexes AS i \
             JOIN sys.objects AS o ON o.object_id = i.object_id \
             JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
             WHERE i.index_id > 0 AND o.is_ms_shipped = 0 AND {predicate}"
        ),
        order_by: "i.object_id, i.index_id".to_owned(),
    })
}

fn index_column_sql(filter: &MapFilter, binder: &mut Binder) -> Result<KindSql, &'static str> {
    let types = filter.include.table_types();
    let predicate = filter.predicate(binder, "s.name", "o.name", "o.type", &types)?;
    Ok(KindSql {
        projection: "CONVERT(nvarchar(20), ic.object_id) AS [object_id], \
                     CONVERT(nvarchar(20), ic.index_id) AS [index_id], \
                     CONVERT(nvarchar(20), ic.index_column_id) AS [index_column_id], \
                     CONVERT(nvarchar(20), ic.column_id) AS [column_id], \
                     CONVERT(nvarchar(128), c.name) AS [name], \
                     CONVERT(nvarchar(20), CASE WHEN ic.is_included_column = 1 THEN NULL \
                         ELSE ic.key_ordinal END) AS [key_ordinal], \
                     CONVERT(nvarchar(5), ic.is_descending_key) AS [is_descending], \
                     CONVERT(nvarchar(5), ic.is_included_column) AS [is_included]"
            .to_owned(),
        from_where: format!(
            "FROM sys.index_columns AS ic \
             JOIN sys.indexes AS i ON i.object_id = ic.object_id AND i.index_id = ic.index_id \
             JOIN sys.columns AS c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
             JOIN sys.objects AS o ON o.object_id = ic.object_id \
             JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
             WHERE ic.index_id > 0 AND o.is_ms_shipped = 0 AND {predicate}"
        ),
        order_by: "ic.object_id, ic.index_id, ic.index_column_id".to_owned(),
    })
}

fn parameter_sql(filter: &MapFilter, binder: &mut Binder) -> Result<KindSql, &'static str> {
    let types = filter.include.routine_types();
    let predicate = filter.predicate(binder, "s.name", "o.name", "o.type", &types)?;
    let data_type = type_expression("p", "t", "b");
    Ok(KindSql {
        projection: format!(
            "CONVERT(nvarchar(20), p.object_id) AS [object_id], \
             CONVERT(nvarchar(20), p.parameter_id) AS [parameter_id], \
             CONVERT(nvarchar(128), p.name) AS [name], \
             CONVERT(nvarchar(256), {data_type}) AS [data_type], \
             CONVERT(nvarchar(5), p.is_output) AS [is_output], \
             CONVERT(nvarchar(5), CASE WHEN p.parameter_id = 0 THEN 1 ELSE 0 END) AS [is_return], \
             CONVERT(nvarchar(5), p.has_default_value) AS [has_default]"
        ),
        from_where: format!(
            "FROM sys.parameters AS p \
             JOIN sys.objects AS o ON o.object_id = p.object_id \
             JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
             LEFT JOIN sys.types AS t ON t.user_type_id = p.user_type_id \
             LEFT JOIN sys.types AS b ON b.user_type_id = p.system_type_id \
             WHERE o.is_ms_shipped = 0 AND {predicate}"
        ),
        order_by: "p.object_id, p.parameter_id".to_owned(),
    })
}

fn absorb(
    kind: &str,
    read: ReadOutput,
    entries: &mut Vec<CatalogEntry>,
    truncated: &mut bool,
) -> Result<(), CatalogError> {
    if read.columns.len() != expected_columns(kind) {
        return Err(CatalogError::UnexpectedResult);
    }
    for row in read.rows {
        match kind {
            "schema" => {
                let schema_id = number::<i32>(&row, 0)?;
                entries.push(CatalogEntry::Schema(SchemaEntry {
                    id: format!("schema:{schema_id}"),
                    schema_id,
                    name: required_text(&row, 1)?,
                }));
            }
            "object" => {
                let object_id = number::<i32>(&row, 0)?;
                let schema_id = number::<i32>(&row, 1)?;
                let code = required_text(&row, 4)?;
                entries.push(CatalogEntry::Object(ObjectEntry {
                    id: format!("object:{object_id}"),
                    object_id,
                    schema_id,
                    schema: required_text(&row, 2)?,
                    name: required_text(&row, 3)?,
                    object_type: object_type(&code).ok_or(CatalogError::UnexpectedResult)?,
                    type_desc: required_text(&row, 5)?,
                    return_type: optional_text(&row, 6),
                    row_count: optional_number::<u64>(&row, 7)?,
                }));
            }
            "column" => {
                let object_id = number::<i32>(&row, 0)?;
                let column_id = number::<i32>(&row, 1)?;
                let default_truncated = flag(&row, 10);
                let computed_truncated = flag(&row, 12);
                *truncated |= default_truncated || computed_truncated;
                entries.push(CatalogEntry::Column(ColumnEntry {
                    id: format!("column:{object_id}:{column_id}"),
                    object_id,
                    column_id,
                    name: required_text(&row, 2)?,
                    ordinal: number::<i32>(&row, 3)?,
                    data_type: required_text(&row, 4)?,
                    nullable: flag(&row, 5),
                    identity: flag(&row, 6),
                    computed: flag(&row, 7),
                    persisted: flag(&row, 8),
                    default: optional_text(&row, 9),
                    default_truncated,
                    computed_definition: optional_text(&row, 11),
                    computed_definition_truncated: computed_truncated,
                }));
            }
            "key" => {
                let key_id = number::<i32>(&row, 0)?;
                let code = required_text(&row, 4)?;
                let key_type = match code.as_str() {
                    "PK" => KeyType::PrimaryKey,
                    "UQ" => KeyType::UniqueConstraint,
                    _ => return Err(CatalogError::UnexpectedResult),
                };
                entries.push(CatalogEntry::Key(KeyEntry {
                    id: format!("key:{key_id}"),
                    key_id,
                    object_id: number::<i32>(&row, 1)?,
                    index_id: number::<i32>(&row, 2)?,
                    name: required_text(&row, 3)?,
                    key_type,
                    system_named: flag(&row, 6),
                }));
            }
            "key_column" => {
                let key_id = number::<i32>(&row, 0)?;
                let column_id = number::<i32>(&row, 3)?;
                entries.push(CatalogEntry::KeyColumn(KeyColumnEntry {
                    id: format!("key_column:{key_id}:{column_id}"),
                    key_id,
                    object_id: number::<i32>(&row, 1)?,
                    index_id: number::<i32>(&row, 2)?,
                    column_id,
                    name: required_text(&row, 4)?,
                    ordinal: number::<i32>(&row, 5)?,
                    descending: flag(&row, 6),
                }));
            }
            "foreign_key" => {
                let foreign_key_id = number::<i32>(&row, 0)?;
                entries.push(CatalogEntry::ForeignKey(ForeignKeyEntry {
                    id: format!("foreign_key:{foreign_key_id}"),
                    foreign_key_id,
                    name: required_text(&row, 1)?,
                    parent_object_id: number::<i32>(&row, 2)?,
                    parent_schema: required_text(&row, 3)?,
                    parent_table: required_text(&row, 4)?,
                    referenced_object_id: number::<i32>(&row, 5)?,
                    referenced_schema: required_text(&row, 6)?,
                    referenced_table: required_text(&row, 7)?,
                    disabled: flag(&row, 8),
                    not_trusted: flag(&row, 9),
                }));
            }
            "foreign_key_column" => {
                let foreign_key_id = number::<i32>(&row, 0)?;
                let ordinal = number::<i32>(&row, 1)?;
                entries.push(CatalogEntry::ForeignKeyColumn(ForeignKeyColumnEntry {
                    id: format!("foreign_key_column:{foreign_key_id}:{ordinal}"),
                    foreign_key_id,
                    ordinal,
                    parent_column_id: number::<i32>(&row, 3)?,
                    parent_column: required_text(&row, 4)?,
                    referenced_column_id: number::<i32>(&row, 6)?,
                    referenced_column: required_text(&row, 7)?,
                }));
            }
            "index" => {
                let object_id = number::<i32>(&row, 0)?;
                let index_id = number::<i32>(&row, 1)?;
                let filter_truncated = flag(&row, 11);
                *truncated |= filter_truncated;
                entries.push(CatalogEntry::Index(IndexEntry {
                    id: format!("index:{object_id}:{index_id}"),
                    object_id,
                    index_id,
                    name: required_text(&row, 2)?,
                    type_desc: required_text(&row, 3)?,
                    unique: flag(&row, 4),
                    primary_key: flag(&row, 5),
                    unique_constraint: flag(&row, 6),
                    disabled: flag(&row, 7),
                    hypothetical: flag(&row, 8),
                    has_filter: flag(&row, 9),
                    filter_definition: optional_text(&row, 10),
                    filter_definition_truncated: filter_truncated,
                    fill_factor: number::<i32>(&row, 12)?,
                }));
            }
            "index_column" => {
                let object_id = number::<i32>(&row, 0)?;
                let index_id = number::<i32>(&row, 1)?;
                let index_column_id = number::<i32>(&row, 2)?;
                entries.push(CatalogEntry::IndexColumn(IndexColumnEntry {
                    id: format!("index_column:{object_id}:{index_id}:{index_column_id}"),
                    object_id,
                    index_id,
                    index_column_id,
                    column_id: number::<i32>(&row, 3)?,
                    name: required_text(&row, 4)?,
                    key_ordinal: optional_number::<i32>(&row, 5)?,
                    descending: flag(&row, 6),
                    included: flag(&row, 7),
                }));
            }
            "parameter" => {
                let object_id = number::<i32>(&row, 0)?;
                let parameter_id = number::<i32>(&row, 1)?;
                entries.push(CatalogEntry::Parameter(ParameterEntry {
                    id: format!("parameter:{object_id}:{parameter_id}"),
                    object_id,
                    parameter_id,
                    name: optional_text(&row, 2),
                    data_type: required_text(&row, 3)?,
                    output: flag(&row, 4),
                    return_value: flag(&row, 5),
                    has_default: flag(&row, 6),
                }));
            }
            _ => return Err(CatalogError::UnexpectedResult),
        }
    }
    Ok(())
}

fn expected_columns(kind: &str) -> usize {
    match kind {
        "schema" => 2,
        "object" => 8,
        "column" => 13,
        "key" => 7,
        "key_column" => 7,
        "foreign_key" => 10,
        "foreign_key_column" => 8,
        "index" => 13,
        "index_column" => 8,
        "parameter" => 7,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(value: Value) -> SchemaMap {
        serde_json::from_value(value).expect("schema map input")
    }

    fn page_sql(input: SchemaMap, kind: &str) -> (String, Vec<Value>) {
        let plan = input.plan().expect("schema map plan");
        let page = plan
            .pages
            .iter()
            .find(|page| page.kind == kind)
            .expect("requested page");
        (page.sql.clone(), page.parameters.clone())
    }

    #[test]
    fn a_table_filter_only_lists_schemas_that_hold_that_table() {
        let (sql, parameters) = page_sql(
            map(serde_json::json!({"table": {"schema": "Sales", "name": "Orders"}})),
            "schema",
        );
        assert!(sql.contains("o.name = "), "{sql}");
        assert!(
            !sql.contains("AND s.name = ") || sql.contains("o.name = "),
            "the schema page must not match on the schema name alone: {sql}"
        );
        assert!(
            parameters.iter().any(|value| value == "Orders"),
            "{parameters:?}"
        );
    }

    #[test]
    fn the_full_map_selects_all_kinds_and_binds_the_table_filter() {
        let input = map(serde_json::json!({
            "table": {"schema": "Weird Schema'; DROP TABLE x;--", "name": "Items"},
            "limit": 7
        }));
        let plan = input.plan().expect("plan");
        assert_eq!(plan.limit, 7);
        // A table filter names one table or view, so routine kinds cannot match.
        assert_eq!(
            plan.pages.iter().map(|page| page.kind).collect::<Vec<_>>(),
            [
                "schema",
                "object",
                "column",
                "key",
                "key_column",
                "foreign_key",
                "foreign_key_column",
                "index",
                "index_column",
            ]
        );
        let object = page_sql(
            map(serde_json::json!({"table": {"name": "Items"}})),
            "object",
        )
        .0;
        assert!(object.contains("o.type IN (N'U', N'V')"), "{object}");
        let unfiltered = map(serde_json::json!({}));
        assert!(
            unfiltered
                .plan()
                .expect("plan")
                .pages
                .iter()
                .any(|page| page.kind == "parameter"),
            "an unfiltered map still lists routine parameters"
        );
        for page in &plan.pages {
            assert!(!page.sql.contains("Weird Schema"), "{}", page.sql);
            assert!(!page.sql.contains("DROP TABLE"), "{}", page.sql);
            assert!(
                page.parameters
                    .iter()
                    .any(|value| { value.as_str() == Some("Weird Schema'; DROP TABLE x;--") })
            );
        }
        assert!(plan.pages.iter().all(|page| page.sql.contains("ORDER BY")));
    }

    #[test]
    fn empty_user_schemas_are_part_of_the_full_map() {
        let (sql, _) = page_sql(map(serde_json::json!({})), "schema");
        assert!(sql.contains("sys.schemas"));
        assert!(sql.contains("N'sys', N'INFORMATION_SCHEMA'"));
        assert!(sql.contains("FROM sys.objects"));
        assert!(sql.contains("EXISTS"));
        let (sql, _) = page_sql(map(serde_json::json!({"schema": "Sales"})), "schema");
        assert!(sql.contains("s.name = @P1"));
    }

    #[test]
    fn table_entries_carry_an_approximate_partition_row_count() {
        let (sql, _) = page_sql(map(serde_json::json!({})), "object");
        assert!(sql.contains("sys.partitions"));
        assert!(sql.contains("p.index_id IN (0, 1)"));
        assert!(sql.contains("AS [row_count]"));
        let input = map(serde_json::json!({"include": ["views"]}));
        let plan = input.plan().expect("plan");
        assert!(plan.pages.iter().all(|page| !page.kind.starts_with("key")));
        assert!(
            plan.pages
                .iter()
                .all(|page| page.kind != "key" && page.kind != "foreign_key")
        );
    }

    #[test]
    fn foreign_keys_keep_both_endpoints_and_filter_on_either_side() {
        let (sql, _) = page_sql(map(serde_json::json!({"schema": "Sales"})), "foreign_key");
        assert!(sql.contains("ps.name = @P1 OR rs.name = @P1"));
        assert!(sql.contains("AS [parent_schema]"));
        assert!(sql.contains("AS [referenced_schema]"));
        let (sql, _) = page_sql(
            map(serde_json::json!({"table": {"schema": "Sales", "name": "Orders"}})),
            "foreign_key",
        );
        assert!(sql.contains("po.name = @P2"));
        assert!(sql.contains("ro.name = @P2"));
    }

    #[test]
    fn definitions_are_bounded_and_flag_their_cut() {
        let (sql, _) = page_sql(map(serde_json::json!({})), "column");
        assert!(sql.contains("LEFT(CONVERT(nvarchar(max), dc.definition), 1025)"));
        assert!(sql.contains("AS [default_truncated]"));
        assert!(sql.contains("AS [computed_truncated]"));
        let (sql, _) = page_sql(map(serde_json::json!({})), "index");
        assert!(sql.contains("AS [filter_truncated]"));
    }

    #[test]
    fn absorb_builds_stable_ids_and_marks_definition_cuts() {
        let read = ReadOutput {
            columns: (0..13).map(|index| format!("c{index}")).collect(),
            rows: vec![vec![
                Some("7".into()),
                Some("3".into()),
                Some("Total".into()),
                Some("3".into()),
                Some("decimal(10,2)".into()),
                Some("0".into()),
                Some("0".into()),
                Some("1".into()),
                Some("0".into()),
                Some("(0)".into()),
                Some("1".into()),
                Some("([a] + [b])".into()),
                Some("0".into()),
            ]],
            truncated: false,
        };
        let mut entries = Vec::new();
        let mut truncated = false;
        absorb("column", read, &mut entries, &mut truncated).expect("column row");
        assert!(truncated);
        assert_eq!(
            entries,
            vec![CatalogEntry::Column(ColumnEntry {
                id: "column:7:3".into(),
                object_id: 7,
                column_id: 3,
                name: "Total".into(),
                ordinal: 3,
                data_type: "decimal(10,2)".into(),
                nullable: false,
                identity: false,
                computed: true,
                persisted: false,
                default: Some("(0)".into()),
                default_truncated: true,
                computed_definition: Some("([a] + [b])".into()),
                computed_definition_truncated: false,
            })]
        );
    }

    #[test]
    fn included_index_columns_have_no_key_ordinal() {
        let read = ReadOutput {
            columns: vec!["c".into(); 8],
            rows: vec![vec![
                Some("7".into()),
                Some("2".into()),
                Some("3".into()),
                Some("4".into()),
                Some("Total".into()),
                None,
                Some("0".into()),
                Some("1".into()),
            ]],
            truncated: false,
        };
        let mut entries = Vec::new();
        let mut truncated = false;
        absorb("index_column", read, &mut entries, &mut truncated).expect("index column");
        assert_eq!(
            entries,
            vec![CatalogEntry::IndexColumn(IndexColumnEntry {
                id: "index_column:7:2:3".into(),
                object_id: 7,
                index_id: 2,
                index_column_id: 3,
                column_id: 4,
                name: "Total".into(),
                key_ordinal: None,
                descending: false,
                included: true,
            })]
        );
    }

    #[test]
    fn a_wrong_row_shape_is_rejected() {
        let read = ReadOutput {
            columns: vec!["only".into()],
            rows: vec![vec![Some("1".into())]],
            truncated: false,
        };
        let mut entries = Vec::new();
        let mut truncated = false;
        assert_eq!(
            absorb("object", read, &mut entries, &mut truncated),
            Err(CatalogError::UnexpectedResult)
        );
    }
}
