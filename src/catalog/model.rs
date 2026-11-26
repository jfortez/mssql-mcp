//! Shared public records for the catalog tools.
//!
//! Catalog output is a normalized feed: one row per catalog entity, with stable
//! string IDs that children use to point at their parent. The IDs are built
//! from SQL Server catalog identifiers and are stable within a database:
//!
//! * `schema:<schema_id>`
//! * `object:<object_id>`
//! * `column:<object_id>:<column_id>`
//! * `key:<key_object_id>`
//! * `key_column:<key_object_id>:<column_id>`
//! * `foreign_key:<foreign_key_object_id>`
//! * `foreign_key_column:<foreign_key_object_id>:<constraint_column_id>`
//! * `index:<object_id>:<index_id>`
//! * `index_column:<object_id>:<index_id>:<index_column_id>`
//! * `parameter:<object_id>:<parameter_id>`
//!
//! A child row is never folded into a parent cell, so no per-table column,
//! key, index or parameter cap exists. Long free text (defaults, computed
//! expressions and index filters) is the one exception: it is cut at the read
//! cell bound and the record carries an explicit `*_truncated` flag. Paging is
//! separate: `next_offset` means more feed rows exist, while `truncated` means
//! a stored expression was shortened.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::database::DbError;

/// Default number of feed rows per page.
pub const DEFAULT_PAGE: u16 = 100;
/// Hard upper bound for one page, matching the bounded read.
pub const MAX_PAGE: u16 = 100;

/// Failure of one catalog operation after input validation succeeded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogError {
    /// The database layer failed, was busy or timed out.
    Database(DbError),
    /// The requested object, table or view does not exist or is not visible.
    NotFound,
    /// A catalog query returned a shape this build does not understand.
    UnexpectedResult,
}

impl From<DbError> for CatalogError {
    fn from(error: DbError) -> Self {
        Self::Database(error)
    }
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(error) => formatter.write_str(&error.message(false)),
            Self::NotFound => formatter.write_str("catalog object not found"),
            Self::UnexpectedResult => {
                formatter.write_str("catalog query returned an unexpected result")
            }
        }
    }
}

/// Schema-qualified object name supplied by the caller. `schema` defaults to
/// `dbo` exactly like the existing table tools.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObjectName {
    /// Object name without a schema prefix.
    pub name: String,
    /// Owning schema; defaults to dbo.
    pub schema: Option<String>,
}

/// Object families selected by `schema_map.include`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Include {
    Tables,
    Views,
    Procedures,
    Functions,
}

/// Target families selected by `search_objects.include`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SearchInclude {
    Objects,
    Columns,
}

/// Object class derived from the SQL Server type code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectType {
    Table,
    View,
    Procedure,
    Function,
    Trigger,
}

impl ObjectType {
    /// Map `sys.objects.type` to the public object class.
    pub fn from_code(code: &str) -> Option<Self> {
        // sys.objects.type is char(2); single-letter codes arrive padded.
        match code.trim_end() {
            "U" => Some(Self::Table),
            "V" => Some(Self::View),
            "P" | "PC" => Some(Self::Procedure),
            "FN" | "IF" | "TF" | "FS" | "FT" | "AF" => Some(Self::Function),
            "TR" => Some(Self::Trigger),
            _ => None,
        }
    }
}

/// Resolved catalog object identity shared by the dependency and stats pages.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectIdentity {
    pub id: String,
    pub object_id: i32,
    pub schema: String,
    pub name: String,
    pub object_type: ObjectType,
    pub type_desc: String,
}

/// A schema feed row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaEntry {
    pub id: String,
    pub schema_id: i32,
    pub name: String,
}

/// A table, view, procedure or function feed row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectEntry {
    pub id: String,
    pub object_id: i32,
    pub schema_id: i32,
    pub schema: String,
    pub name: String,
    pub object_type: ObjectType,
    pub type_desc: String,
    /// Scalar-function return type, or `table` for inline/table-valued functions.
    pub return_type: Option<String>,
    /// Approximate row count from base partitions (`index_id` 0 and 1) for a
    /// table; `None` for views and routines. This comes from catalog metadata,
    /// never from scanning the table.
    pub row_count: Option<u64>,
}

/// A table or view column feed row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnEntry {
    pub id: String,
    pub object_id: i32,
    pub column_id: i32,
    pub name: String,
    /// `sys.columns.column_id`, the catalog ordinal.
    pub ordinal: i32,
    /// Formatted type including length, precision, scale or max.
    pub data_type: String,
    pub nullable: bool,
    pub identity: bool,
    pub computed: bool,
    /// Whether a computed column is persisted.
    pub persisted: bool,
    /// Default constraint expression, cut at the read cell bound.
    pub default: Option<String>,
    pub default_truncated: bool,
    /// Computed-column expression, cut at the read cell bound.
    pub computed_definition: Option<String>,
    pub computed_definition_truncated: bool,
}

/// Primary-key or unique-constraint feed row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEntry {
    pub id: String,
    pub key_id: i32,
    pub object_id: i32,
    pub index_id: i32,
    pub name: String,
    pub key_type: KeyType,
    pub system_named: bool,
}

/// Kind of key constraint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyType {
    PrimaryKey,
    UniqueConstraint,
}

/// One key column in key order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyColumnEntry {
    pub id: String,
    pub key_id: i32,
    pub object_id: i32,
    pub index_id: i32,
    pub column_id: i32,
    pub name: String,
    /// One-based position inside the key.
    pub ordinal: i32,
    pub descending: bool,
}

/// Foreign-key constraint with both endpoint names.
///
/// `parent_*` is the referencing (child) table; `referenced_*` is the target
/// table. Endpoints outside a `schema` or `table` filter are still named here,
/// so a renderer can draw them as external/ghost nodes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKeyEntry {
    pub id: String,
    pub foreign_key_id: i32,
    pub name: String,
    pub parent_object_id: i32,
    pub parent_schema: String,
    pub parent_table: String,
    pub referenced_object_id: i32,
    pub referenced_schema: String,
    pub referenced_table: String,
    pub disabled: bool,
    pub not_trusted: bool,
}

/// One column pair of a foreign key. `parent_column` belongs to the referencing
/// child table; `referenced_column` belongs to the target table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKeyColumnEntry {
    pub id: String,
    pub foreign_key_id: i32,
    /// `sys.foreign_key_columns.constraint_column_id`, one-based.
    pub ordinal: i32,
    pub parent_column_id: i32,
    pub parent_column: String,
    pub referenced_column_id: i32,
    pub referenced_column: String,
}

/// Index or heap-less index definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    pub id: String,
    pub object_id: i32,
    pub index_id: i32,
    pub name: String,
    pub type_desc: String,
    pub unique: bool,
    pub primary_key: bool,
    pub unique_constraint: bool,
    pub disabled: bool,
    pub hypothetical: bool,
    pub has_filter: bool,
    pub filter_definition: Option<String>,
    pub filter_definition_truncated: bool,
    pub fill_factor: i32,
}

/// One key or included column of an index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexColumnEntry {
    pub id: String,
    pub object_id: i32,
    pub index_id: i32,
    pub index_column_id: i32,
    pub column_id: i32,
    pub name: String,
    /// Position inside the index key, or `None` for an included column.
    pub key_ordinal: Option<i32>,
    pub descending: bool,
    pub included: bool,
}

/// One procedure or function parameter. `parameter_id == 0` is the function
/// return value and has no name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParameterEntry {
    pub id: String,
    pub object_id: i32,
    pub parameter_id: i32,
    pub name: Option<String>,
    pub data_type: String,
    pub output: bool,
    pub return_value: bool,
    pub has_default: bool,
}

/// One normalized schema-map feed row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CatalogEntry {
    Schema(SchemaEntry),
    Object(ObjectEntry),
    Column(ColumnEntry),
    Key(KeyEntry),
    KeyColumn(KeyColumnEntry),
    ForeignKey(ForeignKeyEntry),
    ForeignKeyColumn(ForeignKeyColumnEntry),
    Index(IndexEntry),
    IndexColumn(IndexColumnEntry),
    Parameter(ParameterEntry),
}
