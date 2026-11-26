//! `routine_definition`: a full `sys.sql_modules.definition`, paged by
//! character.
//!
//! The shared read layer cuts every cell at 1,024 UTF-16 units, so a full body
//! cannot be returned in one cell. The definition is fetched in safe chunks and
//! concatenated into the requested character window; `next_offset` is the only
//! end-of-definition signal and `truncated` says that more text remains.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::database::{Database, DbError, ReadOutput};
use crate::statement::{Binder, Statement};

use super::model::{CatalogError, ObjectIdentity, ObjectName};
use super::sql::{
    MODULE_OBJECT_TYPES, execute_read, flag, number, object_type, optional_text, required_text,
    validate_object,
};

const DEFAULT_DEFINITION_PAGE: u16 = 8_000;
const MAX_DEFINITION_PAGE: u16 = 20_000;
/// Matches the read layer's per-cell bound, so no chunk is ever clipped.
const DEFINITION_CHUNK: u32 = 1_024;
const LIMIT_ERROR: &str = "limit must be between 1 and 20000";

/// Input for `routine_definition`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineDefinition {
    /// Procedure, function, view or trigger whose definition is read.
    pub object: ObjectName,
    /// Definition characters to skip.
    #[serde(default)]
    pub offset: u32,
    /// Definition characters per page, 1–20000; defaults to 8000.
    #[serde(default)]
    #[schemars(range(min = 1, max = 20000))]
    pub limit: Option<u16>,
}

/// A validated `routine_definition` plan. Building a plan performs no database work.
#[derive(Debug)]
pub struct RoutineDefinitionPlan {
    summary: Statement,
    chunks: Statement,
    schema: String,
    name: String,
    offset: u32,
    limit: u16,
}

impl RoutineDefinition {
    /// Validate inputs and build the catalog statements. No connection opens.
    pub fn plan(&self) -> Result<RoutineDefinitionPlan, &'static str> {
        let (schema, name) = validate_object(&self.object)?;
        let limit = definition_limit(self.limit)?;

        let mut binder = Binder::default();
        let schema_parameter = binder.bind(Value::String(schema.clone()))?;
        let name_parameter = binder.bind(Value::String(name.clone()))?;
        let summary = Statement::read(
            format!(
                "SELECT CONVERT(nvarchar(20), o.object_id) AS [object_id], \
                 CONVERT(nvarchar(128), s.name) AS [schema], \
                 CONVERT(nvarchar(128), o.name) AS [name], \
                 CONVERT(nvarchar(2), o.type) AS [type], \
                 CONVERT(nvarchar(60), o.type_desc) AS [type_desc], \
                 CONVERT(nvarchar(20), ISNULL(DATALENGTH(m.definition), 0) / 2) AS [definition_length], \
                 CONVERT(nvarchar(5), CASE WHEN m.definition IS NULL THEN 0 ELSE 1 END) AS [has_definition] \
                 FROM sys.objects AS o JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
                 LEFT JOIN sys.sql_modules AS m ON m.object_id = o.object_id \
                 WHERE o.is_ms_shipped = 0 AND s.name = {schema_parameter} \
                   AND o.name = {name_parameter} AND o.type IN ({MODULE_OBJECT_TYPES})"
            ),
            binder.parameters,
            1,
        );

        let mut binder = Binder::default();
        let object_parameter = binder.bind(Value::from(0i32))?;
        let start_parameter = binder.bind(Value::from(0u32))?;
        let chunk_parameter = binder.bind(Value::from(DEFINITION_CHUNK))?;
        let count_parameter = binder.bind(Value::from(1u32))?;
        let chunks = Statement::read(
            format!(
                "WITH [chunks] AS ( \
                   SELECT 0 AS [chunk] \
                   UNION ALL \
                   SELECT [chunk] + 1 FROM [chunks] WHERE [chunk] + 1 < {count_parameter} \
                 ) \
                 SELECT CONVERT(nvarchar(20), [chunks].[chunk]) AS [chunk], \
                        SUBSTRING([modules].definition, \
                                  {start_parameter} + ([chunks].[chunk] * {chunk_parameter}) + 1, \
                                  {chunk_parameter}) AS [definition] \
                 FROM sys.sql_modules AS [modules] CROSS JOIN [chunks] \
                 WHERE [modules].object_id = {object_parameter} \
                 ORDER BY [chunks].[chunk]"
            ),
            binder.parameters,
            1,
        );

        Ok(RoutineDefinitionPlan {
            summary,
            chunks,
            schema,
            name,
            offset: self.offset,
            limit,
        })
    }
}

impl RoutineDefinitionPlan {
    /// Resolve the module, then return one character page of its definition.
    pub async fn execute(
        &self,
        database: &Database,
    ) -> Result<RoutineDefinitionPage, RoutineDefinitionError> {
        let module = resolve_module(
            &execute_read(database, self.summary.clone())
                .await
                .map_err(RoutineDefinitionError::from)?,
            &self.schema,
            &self.name,
        )?;
        let window = definition_window(module.definition_length, self.offset, self.limit)?;
        let definition = if window.length == 0 {
            String::new()
        } else {
            let chunk_count = window.length.div_ceil(DEFINITION_CHUNK);
            let mut statement = self.chunks.clone();
            statement.parameters[0] = Value::from(module.identity.object_id);
            statement.parameters[1] = Value::from(window.start);
            statement.parameters[3] = Value::from(chunk_count);
            statement.limit = usize::try_from(chunk_count)
                .map_err(|_| RoutineDefinitionError::UnexpectedResult)?;
            absorb_definition(
                execute_read(database, statement)
                    .await
                    .map_err(RoutineDefinitionError::from)?,
                chunk_count,
                window.length,
            )?
        };
        Ok(RoutineDefinitionPage {
            object: module.identity,
            definition_length: module.definition_length,
            offset: self.offset,
            limit: self.limit,
            next_offset: window.next_offset,
            truncated: window.truncated,
            definition,
        })
    }
}

/// Failure of one `routine_definition` call after input validation succeeded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoutineDefinitionError {
    /// The database layer failed, was busy or timed out.
    Database(DbError),
    /// The requested object is missing or not visible.
    MissingObject { schema: String, name: String },
    /// The module exists, but `sys.sql_modules.definition` is NULL.
    DefinitionUnavailable { schema: String, name: String },
    /// A catalog query returned a shape this build does not understand.
    UnexpectedResult,
}

impl From<CatalogError> for RoutineDefinitionError {
    fn from(error: CatalogError) -> Self {
        match error {
            CatalogError::Database(error) => Self::Database(error),
            CatalogError::NotFound | CatalogError::UnexpectedResult => Self::UnexpectedResult,
        }
    }
}

impl std::fmt::Display for RoutineDefinitionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(error) => formatter.write_str(&error.message(false)),
            Self::MissingObject { schema, name } => write!(
                formatter,
                "object {schema}.{name} was not found or is not visible to the configured login"
            ),
            Self::DefinitionUnavailable { schema, name } => write!(
                formatter,
                "definition for {schema}.{name} is NULL; the module may be encrypted or the login may lack VIEW DEFINITION"
            ),
            Self::UnexpectedResult => {
                formatter.write_str("catalog query returned an unexpected result")
            }
        }
    }
}

/// One character page of a routine definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutineDefinitionPage {
    pub object: ObjectIdentity,
    /// Total definition length in UTF-16 units.
    pub definition_length: u64,
    /// Requested character offset.
    pub offset: u32,
    /// Requested page size in characters.
    pub limit: u16,
    /// Offset of the next character page, or `None` at the end.
    pub next_offset: Option<u32>,
    /// True when more of the definition remains; follow `next_offset`.
    pub truncated: bool,
    /// The requested definition slice; concatenating every page reproduces the
    /// stored definition exactly.
    pub definition: String,
}

#[derive(Debug, PartialEq, Eq)]
struct ResolvedModule {
    identity: ObjectIdentity,
    definition_length: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DefinitionWindow {
    start: u32,
    length: u32,
    next_offset: Option<u32>,
    truncated: bool,
}

fn definition_limit(limit: Option<u16>) -> Result<u16, &'static str> {
    let limit = limit.unwrap_or(DEFAULT_DEFINITION_PAGE);
    if !(1..=MAX_DEFINITION_PAGE).contains(&limit) {
        return Err(LIMIT_ERROR);
    }
    Ok(limit)
}

fn definition_window(
    total: u64,
    offset: u32,
    limit: u16,
) -> Result<DefinitionWindow, RoutineDefinitionError> {
    let total = u32::try_from(total).map_err(|_| RoutineDefinitionError::UnexpectedResult)?;
    let start = offset.min(total);
    let end = start.saturating_add(u32::from(limit)).min(total);
    let next_offset = (end < total).then_some(end);
    Ok(DefinitionWindow {
        start,
        length: end - start,
        next_offset,
        truncated: next_offset.is_some(),
    })
}

fn resolve_module(
    read: &ReadOutput,
    schema: &str,
    name: &str,
) -> Result<ResolvedModule, RoutineDefinitionError> {
    if read.columns.len() != 7 {
        return Err(RoutineDefinitionError::UnexpectedResult);
    }
    if read.rows.is_empty() {
        return Err(RoutineDefinitionError::MissingObject {
            schema: schema.to_owned(),
            name: name.to_owned(),
        });
    }
    if read.rows.len() != 1 {
        return Err(RoutineDefinitionError::UnexpectedResult);
    }
    let row = &read.rows[0];
    let object_id = number::<i32>(row, 0)?;
    let code = required_text(row, 3)?;
    let identity = ObjectIdentity {
        id: format!("object:{object_id}"),
        object_id,
        schema: required_text(row, 1)?,
        name: required_text(row, 2)?,
        object_type: object_type(&code).ok_or(RoutineDefinitionError::UnexpectedResult)?,
        type_desc: required_text(row, 4)?,
    };
    if !flag(row, 6) {
        return Err(RoutineDefinitionError::DefinitionUnavailable {
            schema: identity.schema,
            name: identity.name,
        });
    }
    Ok(ResolvedModule {
        identity,
        definition_length: number::<u64>(row, 5)?,
    })
}

fn absorb_definition(
    read: ReadOutput,
    expected_chunks: u32,
    expected_length: u32,
) -> Result<String, RoutineDefinitionError> {
    let expected_chunks =
        usize::try_from(expected_chunks).map_err(|_| RoutineDefinitionError::UnexpectedResult)?;
    if read.truncated || read.columns.len() != 2 || read.rows.len() != expected_chunks {
        return Err(RoutineDefinitionError::UnexpectedResult);
    }
    let mut definition = String::new();
    for (index, row) in read.rows.iter().enumerate() {
        let chunk = number::<u32>(row, 0)?;
        if chunk != u32::try_from(index).map_err(|_| RoutineDefinitionError::UnexpectedResult)? {
            return Err(RoutineDefinitionError::UnexpectedResult);
        }
        if let Some(text) = optional_text(row, 1) {
            definition.push_str(&text);
        }
    }
    let expected_length =
        usize::try_from(expected_length).map_err(|_| RoutineDefinitionError::UnexpectedResult)?;
    if definition.encode_utf16().count() != expected_length {
        return Err(RoutineDefinitionError::UnexpectedResult);
    }
    Ok(definition)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routine(value: Value) -> RoutineDefinition {
        serde_json::from_value(value).expect("routine input")
    }

    fn row(cells: &[&str]) -> Vec<Option<String>> {
        cells.iter().map(|cell| Some((*cell).to_owned())).collect()
    }

    #[test]
    fn character_pages_cover_the_definition_without_gaps() {
        let first = definition_window(40_000, 0, 20_000).expect("first page");
        assert_eq!((first.start, first.length), (0, 20_000));
        assert_eq!(first.next_offset, Some(20_000));
        assert!(first.truncated);

        let last = definition_window(40_000, 20_000, 20_000).expect("last page");
        assert_eq!((last.start, last.length), (20_000, 20_000));
        assert_eq!(last.next_offset, None);
        assert!(!last.truncated);

        let short = definition_window(2_500, 2_000, 8_000).expect("short page");
        assert_eq!((short.start, short.length), (2_000, 500));
        assert_eq!(short.next_offset, None);
        assert!(!short.truncated);

        let exact_end = definition_window(2_000, 1_000, 1_000).expect("exact page");
        assert_eq!((exact_end.start, exact_end.length), (1_000, 1_000));
        assert_eq!(exact_end.next_offset, None);

        for offset in [2_500, 3_000] {
            let empty = definition_window(2_500, offset, 8_000).expect("empty page");
            assert_eq!((empty.start, empty.length), (2_500, 0));
            assert_eq!(empty.next_offset, None);
            assert!(!empty.truncated);
        }
    }

    #[test]
    fn definition_chunks_are_reassembled_only_when_complete_and_ordered() {
        let read = |rows: Vec<Vec<Option<String>>>, truncated| ReadOutput {
            columns: vec!["chunk".into(), "definition".into()],
            rows,
            truncated,
        };
        assert_eq!(
            absorb_definition(
                read(vec![row(&["0", "abc"]), row(&["1", "def"])], false,),
                2,
                6,
            ),
            Ok("abcdef".to_owned())
        );
        for read in [
            read(vec![row(&["0", "abc"]), row(&["1", "def"])], true),
            read(vec![row(&["1", "abc"]), row(&["0", "def"])], false),
            read(vec![row(&["0", "abc"])], false),
            read(vec![row(&["0", "abc"]), row(&["1", "de"])], false),
        ] {
            assert_eq!(
                absorb_definition(read, 2, 6),
                Err(RoutineDefinitionError::UnexpectedResult)
            );
        }
    }

    #[test]
    fn unknown_objects_and_null_definitions_are_named() {
        let empty = ReadOutput {
            columns: vec!["c".into(); 7],
            rows: Vec::new(),
            truncated: false,
        };
        let error = resolve_module(&empty, "pla_monitoreo", "spConsultarModelosSegmentacion")
            .expect_err("missing module");
        assert_eq!(
            error,
            RoutineDefinitionError::MissingObject {
                schema: "pla_monitoreo".to_owned(),
                name: "spConsultarModelosSegmentacion".to_owned(),
            }
        );
        assert!(
            error
                .to_string()
                .contains("pla_monitoreo.spConsultarModelosSegmentacion")
        );

        let null_definition = ReadOutput {
            columns: vec!["c".into(); 7],
            rows: vec![row(&[
                "7",
                "dbo",
                "Secret",
                "P",
                "SQL_STORED_PROCEDURE",
                "0",
                "0",
            ])],
            truncated: false,
        };
        let error = resolve_module(&null_definition, "dbo", "Secret").expect_err("null definition");
        assert_eq!(
            error,
            RoutineDefinitionError::DefinitionUnavailable {
                schema: "dbo".to_owned(),
                name: "Secret".to_owned(),
            }
        );
        let message = error.to_string();
        assert!(message.contains("dbo.Secret"), "{message}");
        assert!(message.contains("NULL"), "{message}");
        assert!(message.contains("encrypted"), "{message}");
        assert!(message.contains("VIEW DEFINITION"), "{message}");
    }

    #[test]
    fn plan_binds_caller_text_and_bounds_the_page() {
        let injection = "Items'; DROP TABLE x;--";
        let plan = routine(serde_json::json!({
            "object": {"schema": "Sales", "name": injection},
            "limit": 20_000,
        }))
        .plan()
        .expect("routine plan");
        assert!(!plan.summary.sql.contains(injection));
        assert!(!plan.summary.sql.contains("DROP TABLE"));
        assert!(
            plan.summary
                .parameters
                .iter()
                .any(|value| value.as_str() == Some(injection))
        );
        assert!(plan.summary.sql.contains("sys.sql_modules"));
        assert!(plan.summary.sql.contains("AS [definition_length]"));
        assert!(plan.chunks.sql.contains("SUBSTRING"));
        assert_eq!(plan.limit, 20_000);

        for limit in [0, 20_001] {
            let input = routine(serde_json::json!({
                "object": {"name": "Items"},
                "limit": limit,
            }));
            assert_eq!(input.plan().err(), Some(LIMIT_ERROR));
        }
    }
}
