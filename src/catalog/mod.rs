//! Bounded, read-only SQL Server catalog tools.
//!
//! This module is the shared core for the MCP tools and the one-shot CLI:
//!
//! * [`SchemaMap`] returns a normalized, paged feed of schemas, objects,
//!   columns, keys, foreign keys, indexes and routine parameters.
//! * [`ObjectDependencies`] returns outgoing and incoming SQL-expression and
//!   foreign-key edges for one object.
//! * [`SearchObjects`] finds objects and columns by a literal name fragment.
//! * [`TableStats`] returns approximate base-partition row/storage counters and
//!   paged index sizes for one table.
//!
//! Every operation is planned first with `plan()`, which validates and binds
//! all caller data without opening a connection, then executed with
//! `execute(&Database)`. The shared database layer is used unchanged, so these
//! tools inherit its connection cap, timeout and error mapping. They never
//! create a second SQL Server connection pool or host.
//!
//! Pagination is always explicit: a page has `offset`, `limit`, `total` and
//! `next_offset`; `None` is the only end-of-feed signal. Child rows are their
//! own feed entries, so no table has a hidden per-table cap. Long default,
//! computed and index-filter expressions are the sole clipped values and carry
//! their own `*_truncated` flags plus the page-level `truncated` flag.
//! `schema_map` table objects also carry an approximate base-partition
//! `row_count`; the same counters are available from [`TableStats`] without a
//! table scan.

mod dependencies;
mod map;
mod model;
mod search;
mod sql;
mod stats;

pub use dependencies::{
    DependencyDirection, DependencyEdge, DependencyObject, DependencyType, ObjectDependencies,
    ObjectDependenciesPage, ObjectDependenciesPlan,
};
pub use map::{SchemaMap, SchemaMapPage, SchemaMapPlan};
pub use model::{
    CatalogEntry, CatalogError, ColumnEntry, DEFAULT_PAGE, ForeignKeyColumnEntry, ForeignKeyEntry,
    Include, IndexColumnEntry, IndexEntry, KeyColumnEntry, KeyEntry, KeyType, MAX_PAGE,
    ObjectEntry, ObjectIdentity, ObjectName, ObjectType, ParameterEntry, SchemaEntry,
    SearchInclude,
};
pub use search::{SearchColumn, SearchHit, SearchObject, SearchObjects, SearchPage, SearchPlan};
pub use stats::{IndexSize, TableStats, TableStatsPlan, TableStatsResult};
