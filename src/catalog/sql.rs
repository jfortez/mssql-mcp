//! Bounded read plumbing shared by the catalog tools.

use std::collections::BTreeMap;
use std::str::FromStr;

use serde_json::Value;

use crate::database::{Database, Outcome, ReadOutput};
use crate::statement::{Binder, Statement, validate_identifier};

use super::model::{CatalogError, DEFAULT_PAGE, MAX_PAGE, ObjectName, ObjectType};

/// Every object class the map, search and dependency tools understand.
pub(super) const ALL_OBJECT_TYPES: &str =
    "N'U', N'V', N'P', N'PC', N'FN', N'IF', N'TF', N'FS', N'FT', N'AF'";

/// A page query whose base SQL already ends with `ORDER BY`. Offset and limit
/// are appended as two extra bound parameters so no caller input becomes SQL.
#[derive(Clone, Debug)]
pub(super) struct PageQuery {
    pub kind: &'static str,
    pub sql: String,
    pub parameters: Vec<Value>,
}

impl PageQuery {
    pub fn new(kind: &'static str, sql: String, binder: Binder) -> Self {
        Self {
            kind,
            sql,
            parameters: binder.parameters,
        }
    }

    pub fn statement(&self, offset: u32, limit: u16) -> Statement {
        let offset_parameter = self.parameters.len() + 1;
        let limit_parameter = offset_parameter + 1;
        let sql = format!(
            "{} OFFSET @P{offset_parameter} ROWS FETCH NEXT @P{limit_parameter} ROWS ONLY",
            self.sql
        );
        let mut parameters = self.parameters.clone();
        parameters.push(Value::from(offset));
        parameters.push(Value::from(limit));
        Statement::read(sql, parameters, usize::from(limit))
    }
}

/// Global page window over the concatenated per-kind counts.
#[derive(Clone, Copy, Debug)]
pub(super) struct Paging {
    offset: u64,
    end: u64,
    total: u64,
}

impl Paging {
    pub fn new(offset: u32, limit: u16, total: u64) -> Self {
        let offset = u64::from(offset);
        let end = offset.saturating_add(u64::from(limit)).min(total);
        Self { offset, end, total }
    }

    /// Local `(offset, limit)` inside one kind, or `None` when the page does
    /// not intersect that kind. The caller adds the returned offset to zero for
    /// each kind; `kind_start` is the cumulative count of earlier kinds.
    pub fn range(&self, kind_start: u64, count: u64) -> Result<Option<(u32, u16)>, CatalogError> {
        let first = self.offset.max(kind_start);
        let last = self.end.min(kind_start.saturating_add(count));
        if last <= first {
            return Ok(None);
        }
        let local_offset =
            u32::try_from(first - kind_start).map_err(|_| CatalogError::UnexpectedResult)?;
        let local_limit =
            u16::try_from(last - first).map_err(|_| CatalogError::UnexpectedResult)?;
        Ok(Some((local_offset, local_limit)))
    }

    pub fn next_offset(&self) -> Result<Option<u32>, CatalogError> {
        if self.end < self.total {
            u32::try_from(self.end)
                .map(Some)
                .map_err(|_| CatalogError::UnexpectedResult)
        } else {
            Ok(None)
        }
    }
}

/// Execute one statement through the shared database layer.
pub(super) async fn execute_read(
    database: &Database,
    statement: Statement,
) -> Result<ReadOutput, CatalogError> {
    match database.execute(statement).await {
        Ok(Outcome::Read(read)) => Ok(read),
        Ok(_) => Err(CatalogError::UnexpectedResult),
        Err(error) => Err(CatalogError::Database(error)),
    }
}

/// Parse the `kind, count` result of a catalog count query. The expected names
/// are matched by value, not position, because a UNION ALL has no result order
/// guarantee. Missing, duplicate or extra counts are a broken result shape.
pub(super) fn parse_counts(
    read: ReadOutput,
    expected: &[&'static str],
) -> Result<Vec<u64>, CatalogError> {
    if read.truncated || read.columns.len() != 2 || read.rows.len() != expected.len() {
        return Err(CatalogError::UnexpectedResult);
    }
    let mut counts = BTreeMap::new();
    for row in read.rows {
        let kind = required_text(&row, 0)?;
        let count = number::<u64>(&row, 1)?;
        if counts.insert(kind, count).is_some() {
            return Err(CatalogError::UnexpectedResult);
        }
    }
    let mut ordered = Vec::with_capacity(expected.len());
    for kind in expected {
        ordered.push(counts.remove(*kind).ok_or(CatalogError::UnexpectedResult)?);
    }
    if !counts.is_empty() {
        return Err(CatalogError::UnexpectedResult);
    }
    Ok(ordered)
}

pub(super) fn page_limit(limit: Option<u16>) -> Result<u16, &'static str> {
    let limit = limit.unwrap_or(DEFAULT_PAGE);
    if !(1..=MAX_PAGE).contains(&limit) {
        return Err("limit must be between 1 and 100");
    }
    Ok(limit)
}

pub(super) fn validate_schema(schema: Option<&str>) -> Result<(), &'static str> {
    if let Some(schema) = schema {
        validate_identifier(schema)?;
    }
    Ok(())
}

pub(super) fn validate_object(object: &ObjectName) -> Result<(String, String), &'static str> {
    let schema = object.schema.as_deref().unwrap_or("dbo");
    validate_identifier(schema)?;
    validate_identifier(&object.name)?;
    Ok((schema.to_owned(), object.name.clone()))
}

pub(super) fn cell(row: &[Option<String>], index: usize) -> Option<&str> {
    row.get(index).and_then(|value| value.as_deref())
}

pub(super) fn required_text(row: &[Option<String>], index: usize) -> Result<String, CatalogError> {
    cell(row, index)
        .map(str::to_owned)
        .ok_or(CatalogError::UnexpectedResult)
}

pub(super) fn optional_text(row: &[Option<String>], index: usize) -> Option<String> {
    cell(row, index).map(str::to_owned)
}

pub(super) fn number<T: FromStr>(row: &[Option<String>], index: usize) -> Result<T, CatalogError> {
    required_text(row, index)?
        .parse()
        .map_err(|_| CatalogError::UnexpectedResult)
}

pub(super) fn optional_number<T: FromStr>(
    row: &[Option<String>],
    index: usize,
) -> Result<Option<T>, CatalogError> {
    match cell(row, index) {
        None => Ok(None),
        Some(value) => value
            .parse()
            .map(Some)
            .map_err(|_| CatalogError::UnexpectedResult),
    }
}

pub(super) fn flag(row: &[Option<String>], index: usize) -> bool {
    matches!(cell(row, index), Some("1" | "true" | "TRUE"))
}

pub(super) fn optional_flag(
    row: &[Option<String>],
    index: usize,
) -> Result<Option<bool>, CatalogError> {
    match cell(row, index) {
        None => Ok(None),
        Some("1" | "true" | "TRUE") => Ok(Some(true)),
        Some("0" | "false" | "FALSE") => Ok(Some(false)),
        Some(_) => Err(CatalogError::UnexpectedResult),
    }
}

pub(super) fn object_type(code: &str) -> Option<ObjectType> {
    ObjectType::from_code(code)
}

/// A safe SQL expression for a formatted type with length/precision/scale.
///
/// `value` is the column/parameter alias. `declared` and `base` are the joined
/// `sys.types` aliases for the declared user type and the underlying system
/// type. They are joined with `LEFT JOIN` because SQL Server hides a
/// user-defined type from a login without permission on it; the base type still
/// names the column, and the declared alias is used when it is visible.
pub(super) fn type_expression(value: &str, declared: &str, base: &str) -> String {
    format!(
        "ISNULL({declared}.name, {base}.name) + CASE \
         WHEN {base}.name IN (N'nvarchar', N'nchar') THEN \
             N'(' + CASE WHEN {value}.max_length = -1 THEN N'max' ELSE CONVERT(nvarchar(10), {value}.max_length / 2) END + N')' \
         WHEN {base}.name IN (N'varchar', N'char', N'varbinary', N'binary') THEN \
             N'(' + CASE WHEN {value}.max_length = -1 THEN N'max' ELSE CONVERT(nvarchar(10), {value}.max_length) END + N')' \
         WHEN {base}.name IN (N'decimal', N'numeric') THEN \
             N'(' + CONVERT(nvarchar(10), {value}.precision) + N',' + CONVERT(nvarchar(10), {value}.scale) + N')' \
         WHEN {base}.name IN (N'datetime2', N'time', N'datetimeoffset') THEN \
             N'(' + CONVERT(nvarchar(10), {value}.scale) + N')' \
         ELSE N'' END"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paging_walks_kinds_without_gaps_or_overlap() {
        let paging = Paging::new(120, 100, 250);
        assert_eq!(paging.range(0, 100), Ok(None));
        assert_eq!(paging.range(100, 100), Ok(Some((20, 80))));
        assert_eq!(paging.range(200, 50), Ok(Some((0, 20))));
        assert_eq!(paging.next_offset(), Ok(Some(220)));

        let first = Paging::new(0, 100, 250);
        assert_eq!(first.range(0, 100), Ok(Some((0, 100))));
        assert_eq!(first.next_offset(), Ok(Some(100)));

        let last = Paging::new(200, 100, 250);
        assert_eq!(last.next_offset(), Ok(None));

        let beyond = Paging::new(300, 100, 250);
        assert_eq!(beyond.range(0, 250), Ok(None));
        assert_eq!(beyond.next_offset(), Ok(None));
    }

    #[test]
    fn page_queries_bind_offset_and_limit_after_base_filters() {
        let mut binder = Binder::default();
        let parameter = binder.bind(Value::String("Items".to_owned())).unwrap();
        let query = PageQuery::new(
            "object",
            format!("SELECT 1 FROM sys.objects WHERE name = {parameter} ORDER BY object_id"),
            binder,
        );
        let statement = query.statement(5, 10);
        assert!(
            statement
                .sql
                .ends_with("ORDER BY object_id OFFSET @P2 ROWS FETCH NEXT @P3 ROWS ONLY")
        );
        assert_eq!(
            statement.parameters,
            vec![
                Value::String("Items".to_owned()),
                Value::from(5),
                Value::from(10)
            ]
        );
        assert!(!statement.mutation);
    }

    #[test]
    fn counts_must_match_the_expected_kind_set_exactly() {
        let read = |rows: Vec<Vec<Option<String>>>| ReadOutput {
            columns: vec!["kind".into(), "count".into()],
            rows,
            truncated: false,
        };
        assert_eq!(
            parse_counts(
                read(vec![
                    vec![Some("a".into()), Some("2".into())],
                    vec![Some("b".into()), Some("3".into())],
                ]),
                &["a", "b"],
            ),
            Ok(vec![2, 3])
        );
        for rows in [
            vec![vec![Some("a".into()), Some("2".into())]],
            vec![
                vec![Some("a".into()), Some("2".into())],
                vec![Some("a".into()), Some("3".into())],
            ],
            vec![
                vec![Some("a".into()), Some("2".into())],
                vec![Some("b".into()), Some("3".into())],
                vec![Some("c".into()), Some("4".into())],
            ],
        ] {
            assert_eq!(
                parse_counts(read(rows), &["a", "b"]),
                Err(CatalogError::UnexpectedResult)
            );
        }
    }
}
