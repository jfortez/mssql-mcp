//! `search_objects`: literal name-fragment search across objects and columns.
//!
//! The fragment is always a bound `CHARINDEX` argument, never a `LIKE` pattern,
//! so `%`, `_` and `[` stay literal and no caller text becomes SQL.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::database::{Database, ReadOutput};
use crate::statement::{Binder, Statement};

use super::model::{CatalogError, ObjectType, SearchInclude};
use super::sql::{
    ALL_OBJECT_TYPES, PageQuery, Paging, execute_read, flag, number, object_type, page_limit,
    parse_counts, required_text, type_expression, validate_schema,
};

const INCLUDE_ERROR: &str = "include must contain at least one of objects, columns";
const QUERY_ERROR: &str = "query must contain 1–128 UTF-16 units and no control characters";

/// Input for `search_objects`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchObjects {
    /// Name fragment to find; matched literally with CHARINDEX.
    pub query: String,
    /// Restrict results to this schema.
    #[serde(default)]
    pub schema: Option<String>,
    /// Result families to include; defaults to objects and columns.
    #[serde(default)]
    pub include: Option<Vec<SearchInclude>>,
    /// Hits to skip.
    #[serde(default)]
    pub offset: u32,
    /// Hits per page, 1–100; defaults to 100.
    #[serde(default)]
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<u16>,
}

/// A validated `search_objects` plan. Building a plan performs no database work.
#[derive(Debug)]
pub struct SearchPlan {
    counts: Statement,
    pages: Vec<PageQuery>,
    offset: u32,
    limit: u16,
}

impl SearchObjects {
    /// Validate inputs and build the catalog statements. No connection opens.
    pub fn plan(&self) -> Result<SearchPlan, &'static str> {
        if self.query.trim().is_empty() {
            return Err("query must not be blank");
        }
        if self.query.encode_utf16().count() > 128 || self.query.chars().any(char::is_control) {
            return Err(QUERY_ERROR);
        }
        validate_schema(self.schema.as_deref())?;
        let targets = Targets::new(self.include.as_deref())?;
        let limit = page_limit(self.limit)?;

        let mut count_binder = Binder::default();
        let query_parameter = count_binder.bind(Value::String(self.query.clone()))?;
        let schema_parameter = count_binder.bind(
            self.schema
                .as_ref()
                .map(|schema| Value::String(schema.clone()))
                .unwrap_or(Value::Null),
        )?;
        let mut count_parts = Vec::new();
        if targets.objects {
            count_parts.push(format!(
                "SELECT N'object' AS [kind], COUNT_BIG(*) AS [count] \
                 FROM sys.objects AS o JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
                 WHERE o.is_ms_shipped = 0 AND o.type IN ({ALL_OBJECT_TYPES}) \
                   AND CHARINDEX({query_parameter}, CONVERT(nvarchar(128), o.name)) > 0 \
                   AND ({schema_parameter} IS NULL OR s.name = {schema_parameter})"
            ));
        }
        if targets.columns {
            count_parts.push(format!(
                "SELECT N'column' AS [kind], COUNT_BIG(*) AS [count] \
                 FROM sys.columns AS c \
                 JOIN sys.objects AS o ON o.object_id = c.object_id \
                 JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
                 LEFT JOIN sys.types AS t ON t.user_type_id = c.user_type_id \
                 LEFT JOIN sys.types AS b ON b.user_type_id = c.system_type_id \
                 WHERE o.is_ms_shipped = 0 AND o.type IN (N'U', N'V') \
                   AND CHARINDEX({query_parameter}, CONVERT(nvarchar(128), c.name)) > 0 \
                   AND ({schema_parameter} IS NULL OR s.name = {schema_parameter})"
            ));
        }
        let count_count = count_parts.len();
        let counts = Statement::read(
            count_parts.join(" UNION ALL "),
            count_binder.parameters,
            count_count,
        );

        let mut pages = Vec::with_capacity(count_count);
        if targets.objects {
            let mut binder = Binder::default();
            let query = binder.bind(Value::String(self.query.clone()))?;
            let schema = bind_schema(&mut binder, self.schema.as_ref())?;
            pages.push(PageQuery::new(
                "object",
                format!(
                    "SELECT CONVERT(nvarchar(20), o.object_id) AS [object_id], \
                     CONVERT(nvarchar(128), s.name) AS [schema], \
                     CONVERT(nvarchar(128), o.name) AS [name], \
                     CONVERT(nvarchar(2), o.type) AS [type], \
                     CONVERT(nvarchar(60), o.type_desc) AS [type_desc] \
                     FROM sys.objects AS o JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
                     WHERE o.is_ms_shipped = 0 AND o.type IN ({ALL_OBJECT_TYPES}) \
                       AND CHARINDEX({query}, CONVERT(nvarchar(128), o.name)) > 0 \
                       AND ({schema} IS NULL OR s.name = {schema}) \
                     ORDER BY s.name, o.name, o.object_id"
                ),
                binder,
            ));
        }
        if targets.columns {
            let mut binder = Binder::default();
            let query = binder.bind(Value::String(self.query.clone()))?;
            let schema = bind_schema(&mut binder, self.schema.as_ref())?;
            let data_type = type_expression("c", "t", "b");
            pages.push(PageQuery::new(
                "column",
                format!(
                    "SELECT CONVERT(nvarchar(20), c.object_id) AS [object_id], \
                     CONVERT(nvarchar(128), s.name) AS [schema], \
                     CONVERT(nvarchar(128), o.name) AS [table], \
                     CONVERT(nvarchar(2), o.type) AS [table_type], \
                     CONVERT(nvarchar(20), c.column_id) AS [column_id], \
                     CONVERT(nvarchar(128), c.name) AS [name], \
                     CONVERT(nvarchar(256), {data_type}) AS [data_type], \
                     CONVERT(nvarchar(5), c.is_nullable) AS [is_nullable], \
                     CONVERT(nvarchar(20), c.column_id) AS [ordinal] \
                     FROM sys.columns AS c \
                     JOIN sys.objects AS o ON o.object_id = c.object_id \
                     JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
                     LEFT JOIN sys.types AS t ON t.user_type_id = c.user_type_id \
                 LEFT JOIN sys.types AS b ON b.user_type_id = c.system_type_id \
                     WHERE o.is_ms_shipped = 0 AND o.type IN (N'U', N'V') \
                       AND CHARINDEX({query}, CONVERT(nvarchar(128), c.name)) > 0 \
                       AND ({schema} IS NULL OR s.name = {schema}) \
                     ORDER BY s.name, o.name, c.column_id"
                ),
                binder,
            ));
        }
        Ok(SearchPlan {
            counts,
            pages,
            offset: self.offset,
            limit,
        })
    }
}

impl SearchPlan {
    /// Execute the planned catalog reads and return one page of hits.
    pub async fn execute(&self, database: &Database) -> Result<SearchPage, CatalogError> {
        let counts = parse_counts(
            execute_read(database, self.counts.clone()).await?,
            &self.pages.iter().map(|page| page.kind).collect::<Vec<_>>(),
        )?;
        let total = counts.iter().sum::<u64>();
        let paging = Paging::new(self.offset, self.limit, total);
        let mut hits = Vec::new();
        let mut truncated = false;
        let mut kind_start = 0u64;
        for (page, count) in self.pages.iter().zip(counts) {
            if let Some((offset, limit)) = paging.range(kind_start, count)? {
                let read = execute_read(database, page.statement(offset, limit)).await?;
                truncated |= read.truncated;
                absorb(page.kind, read, &mut hits)?;
            }
            kind_start += count;
        }
        Ok(SearchPage {
            offset: self.offset,
            limit: self.limit,
            total,
            next_offset: paging.next_offset()?,
            truncated,
            hits,
        })
    }
}

/// One page of object and column search hits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPage {
    pub offset: u32,
    pub limit: u16,
    pub total: u64,
    pub next_offset: Option<u32>,
    /// True only if a cell was cut; search result fields are short.
    pub truncated: bool,
    pub hits: Vec<SearchHit>,
}

/// A matched object or column.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SearchHit {
    Object(SearchObject),
    Column(SearchColumn),
}

/// A matched object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchObject {
    pub id: String,
    pub object_id: i32,
    pub schema: String,
    pub name: String,
    pub object_type: ObjectType,
    pub type_desc: String,
}

/// A matched column, including its owning table or view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchColumn {
    pub id: String,
    pub object_id: i32,
    pub schema: String,
    pub table: String,
    pub table_type: ObjectType,
    pub column_id: i32,
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub ordinal: i32,
}

#[derive(Clone, Copy, Debug)]
struct Targets {
    objects: bool,
    columns: bool,
}

impl Targets {
    fn new(list: Option<&[SearchInclude]>) -> Result<Self, &'static str> {
        let Some(list) = list else {
            return Ok(Self {
                objects: true,
                columns: true,
            });
        };
        if list.is_empty() {
            return Err(INCLUDE_ERROR);
        }
        Ok(Self {
            objects: list.contains(&SearchInclude::Objects),
            columns: list.contains(&SearchInclude::Columns),
        })
    }
}

fn bind_schema(binder: &mut Binder, schema: Option<&String>) -> Result<String, &'static str> {
    binder.bind(
        schema
            .map(|schema| Value::String(schema.clone()))
            .unwrap_or(Value::Null),
    )
}

fn absorb(kind: &str, read: ReadOutput, hits: &mut Vec<SearchHit>) -> Result<(), CatalogError> {
    match kind {
        "object" => {
            if read.columns.len() != 5 {
                return Err(CatalogError::UnexpectedResult);
            }
            for row in read.rows {
                let object_id = number::<i32>(&row, 0)?;
                let code = required_text(&row, 3)?;
                hits.push(SearchHit::Object(SearchObject {
                    id: format!("object:{object_id}"),
                    object_id,
                    schema: required_text(&row, 1)?,
                    name: required_text(&row, 2)?,
                    object_type: object_type(&code).ok_or(CatalogError::UnexpectedResult)?,
                    type_desc: required_text(&row, 4)?,
                }));
            }
        }
        "column" => {
            if read.columns.len() != 9 {
                return Err(CatalogError::UnexpectedResult);
            }
            for row in read.rows {
                let object_id = number::<i32>(&row, 0)?;
                let column_id = number::<i32>(&row, 4)?;
                let code = required_text(&row, 3)?;
                hits.push(SearchHit::Column(SearchColumn {
                    id: format!("column:{object_id}:{column_id}"),
                    object_id,
                    schema: required_text(&row, 1)?,
                    table: required_text(&row, 2)?,
                    table_type: object_type(&code).ok_or(CatalogError::UnexpectedResult)?,
                    column_id,
                    name: required_text(&row, 5)?,
                    data_type: required_text(&row, 6)?,
                    nullable: flag(&row, 7),
                    ordinal: number::<i32>(&row, 8)?,
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

    fn search(value: Value) -> SearchObjects {
        serde_json::from_value(value).expect("search input")
    }

    #[test]
    fn fragments_are_bound_charindex_literals_not_like_patterns() {
        let plan = search(serde_json::json!({
            "query": "100%_[odd]' OR 1=1--",
            "schema": "Sales"
        }))
        .plan()
        .expect("search plan");
        for page in &plan.pages {
            assert!(page.sql.contains("CHARINDEX(@P1"));
            assert!(!page.sql.contains("LIKE"));
            assert!(!page.sql.contains("100%"));
            assert_eq!(
                page.parameters,
                vec![
                    Value::String("100%_[odd]' OR 1=1--".to_owned()),
                    Value::String("Sales".to_owned())
                ]
            );
        }
        assert!(plan.counts.sql.contains("CHARINDEX(@P1"));
        assert!(!plan.counts.sql.contains("100%"));
    }

    #[test]
    fn object_and_column_pages_have_distinct_ordering_and_ids() {
        let plan = search(serde_json::json!({"query": "Order"}))
            .plan()
            .expect("search plan");
        assert_eq!(
            plan.pages.iter().map(|page| page.kind).collect::<Vec<_>>(),
            ["object", "column"]
        );
        assert!(
            plan.pages[0]
                .sql
                .contains("ORDER BY s.name, o.name, o.object_id")
        );
        assert!(
            plan.pages[1]
                .sql
                .contains("ORDER BY s.name, o.name, c.column_id")
        );
    }

    #[test]
    fn absorb_builds_search_ids() {
        let read = ReadOutput {
            columns: vec!["a".into(); 5],
            rows: vec![vec![
                Some("4".into()),
                Some("dbo".into()),
                Some("Orders".into()),
                Some("U".into()),
                Some("USER_TABLE".into()),
            ]],
            truncated: false,
        };
        let mut hits = Vec::new();
        absorb("object", read, &mut hits).expect("object hit");
        assert_eq!(
            hits,
            vec![SearchHit::Object(SearchObject {
                id: "object:4".into(),
                object_id: 4,
                schema: "dbo".into(),
                name: "Orders".into(),
                object_type: ObjectType::Table,
                type_desc: "USER_TABLE".into(),
            })]
        );

        let read = ReadOutput {
            columns: vec!["a".into(); 9],
            rows: vec![vec![
                Some("4".into()),
                Some("dbo".into()),
                Some("Orders".into()),
                Some("V".into()),
                Some("2".into()),
                Some("Total".into()),
                Some("decimal(10,2)".into()),
                Some("0".into()),
                Some("2".into()),
            ]],
            truncated: false,
        };
        absorb("column", read, &mut hits).expect("column hit");
        assert!(matches!(
            hits.last(),
            Some(SearchHit::Column(column)) if column.id == "column:4:2"
        ));
    }
}
