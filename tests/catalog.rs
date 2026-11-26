//! Public behaviour of the bounded catalog core.
//!
//! These tests exercise the input contract without a SQL Server: validation
//! must reject bad arguments before a connection is opened, and every execution
//! path must use the shared database layer, so a loopback peer that drops the
//! connection reports a sanitized connection failure rather than a panic.

use std::sync::Arc;

use mssql_mcp::catalog::{
    CatalogEntry, CatalogError, ForeignKeyColumnEntry, ForeignKeyEntry, ObjectDependencies,
    SchemaMap, SearchObjects, TableStats,
};
use mssql_mcp::config::{Auth, Mode, Settings, Transport};
use mssql_mcp::database::{Database, DbError};
use serde_json::{Value, json};

#[allow(
    dead_code,
    reason = "this test uses the shared socket fixture, not the command helpers"
)]
mod support;

#[test]
fn sql_server_padded_catalog_object_codes_decode() {
    use mssql_mcp::catalog::ObjectType;
    assert_eq!(ObjectType::from_code("U "), Some(ObjectType::Table));
    assert_eq!(ObjectType::from_code("V "), Some(ObjectType::View));
    assert_eq!(ObjectType::from_code("P "), Some(ObjectType::Procedure));
    assert_eq!(ObjectType::from_code("FN"), Some(ObjectType::Function));
}

const IDENTIFIER_ERROR: &str =
    "identifier must contain 1–128 UTF-16 units and no control characters";
const LIMIT_ERROR: &str = "limit must be between 1 and 100";

fn schema_map(value: Value) -> SchemaMap {
    serde_json::from_value(value).expect("schema map input")
}

fn search(value: Value) -> SearchObjects {
    serde_json::from_value(value).expect("search input")
}

fn dependencies(value: Value) -> ObjectDependencies {
    serde_json::from_value(value).expect("dependencies input")
}

fn stats(value: Value) -> TableStats {
    serde_json::from_value(value).expect("stats input")
}

#[test]
fn schema_map_validates_before_connecting() {
    assert!(schema_map(json!({})).plan().is_ok());
    // A table without its own schema inherits the enclosing schema filter.
    assert!(
        schema_map(json!({"schema": "Sales", "table": {"name": "Orders"}}))
            .plan()
            .is_ok()
    );
    for (input, message) in [
        (json!({"schema": "bad\u{7}name"}), IDENTIFIER_ERROR),
        (
            json!({"include": []}),
            "include must contain at least one of tables, views, procedures, functions",
        ),
        (json!({"limit": 0}), LIMIT_ERROR),
        (json!({"limit": 101}), LIMIT_ERROR),
        (json!({"table": {"name": "bad\u{7}name"}}), IDENTIFIER_ERROR),
        (
            json!({"schema": "sales", "table": {"schema": "dbo", "name": "Items"}}),
            "schema must match the table schema",
        ),
    ] {
        assert_eq!(schema_map(input).plan().err(), Some(message));
    }
    assert!(serde_json::from_value::<SchemaMap>(json!({"unknown": true})).is_err());
}

#[test]
fn search_validates_the_fragment_and_include_before_connecting() {
    for (input, message) in [
        (json!({"query": "   "}), "query must not be blank"),
        (
            json!({"query": "x".repeat(129)}),
            "query must contain 1–128 UTF-16 units and no control characters",
        ),
        (
            json!({"query": "bad\u{7}name"}),
            "query must contain 1–128 UTF-16 units and no control characters",
        ),
        (
            json!({"query": "id", "include": []}),
            "include must contain at least one of objects, columns",
        ),
        (json!({"query": "id", "limit": 101}), LIMIT_ERROR),
    ] {
        assert_eq!(search(input).plan().err(), Some(message));
    }
    // SQL wildcard characters stay literal because the query uses CHARINDEX.
    assert!(search(json!({"query": "100%_literal"})).plan().is_ok());
}

#[test]
fn dependencies_and_stats_validate_before_connecting() {
    assert!(
        dependencies(json!({"object": {"name": "Items"}}))
            .plan()
            .is_ok()
    );
    assert!(stats(json!({"table": {"name": "Items"}})).plan().is_ok());
    for (input, message) in [
        (
            json!({"object": {"name": "bad\u{7}name"}}),
            IDENTIFIER_ERROR,
        ),
        (
            json!({"object": {"name": "Items"}, "limit": 0}),
            LIMIT_ERROR,
        ),
    ] {
        assert_eq!(dependencies(input).plan().err(), Some(message));
    }
    for (input, message) in [
        (json!({"table": {"name": ""}}), IDENTIFIER_ERROR),
        (
            json!({"table": {"name": "Items"}, "limit": 101}),
            LIMIT_ERROR,
        ),
    ] {
        assert_eq!(stats(input).plan().err(), Some(message));
    }
}

#[tokio::test]
async fn every_core_tool_uses_the_shared_database_layer() {
    let (port, _peer) = support::dropping_peer();
    let database = Database::new(Arc::new(Settings {
        host: "127.0.0.1".to_owned(),
        port,
        database: "catalog-test".to_owned(),
        auth: Auth::Sql {
            user: "catalog".to_owned(),
            password: "catalog-test-password".to_owned(),
        },
        mode: Mode::Read,
        transport: Transport::Stdio,
        http_listen: "127.0.0.1:0".parse().expect("loopback address"),
        token: None,
        timeout_seconds: 1,
        yolo: false,
        trust_server_certificate: true,
        check_config: false,
    }));
    let expected = Some(CatalogError::Database(DbError::Connect));
    assert_eq!(
        schema_map(json!({}))
            .plan()
            .expect("map plan")
            .execute(&database)
            .await
            .err(),
        expected
    );
    assert_eq!(
        search(json!({"query": "id"}))
            .plan()
            .expect("search plan")
            .execute(&database)
            .await
            .err(),
        expected
    );
    assert_eq!(
        dependencies(json!({"object": {"name": "Items"}}))
            .plan()
            .expect("dependencies plan")
            .execute(&database)
            .await
            .err(),
        expected
    );
    assert_eq!(
        stats(json!({"table": {"name": "Items"}}))
            .plan()
            .expect("stats plan")
            .execute(&database)
            .await
            .err(),
        expected
    );
}

#[test]
fn catalog_entries_serialize_with_kind_tags_and_both_fk_endpoints() {
    let edge = CatalogEntry::ForeignKey(ForeignKeyEntry {
        id: "foreign_key:12".to_owned(),
        foreign_key_id: 12,
        name: "FK_Orders_Customers".to_owned(),
        parent_object_id: 4,
        parent_schema: "Sales".to_owned(),
        parent_table: "Orders".to_owned(),
        referenced_object_id: 9,
        referenced_schema: "Sales".to_owned(),
        referenced_table: "Customers".to_owned(),
        disabled: false,
        not_trusted: false,
    });
    let value = serde_json::to_value(&edge).expect("serialize foreign key");
    assert_eq!(value["kind"], "foreign_key");
    assert_eq!(value["parent_table"], "Orders");
    assert_eq!(value["referenced_table"], "Customers");

    let pair = CatalogEntry::ForeignKeyColumn(ForeignKeyColumnEntry {
        id: "foreign_key_column:12:1".to_owned(),
        foreign_key_id: 12,
        ordinal: 1,
        parent_column_id: 2,
        parent_column: "CustomerID".to_owned(),
        referenced_column_id: 1,
        referenced_column: "CustomerID".to_owned(),
    });
    let value = serde_json::to_value(&pair).expect("serialize foreign-key column");
    assert_eq!(value["kind"], "foreign_key_column");
    assert_eq!(value["parent_column"], "CustomerID");
    assert_eq!(value["referenced_column"], "CustomerID");
}
