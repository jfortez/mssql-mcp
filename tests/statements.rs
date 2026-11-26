use mssql_mcp::statement::{
    DeleteRows, DescribeTable, InsertRow, ListTables, SelectRows, UpdateRows,
};
use serde_json::{Value, json};

fn select(input: Value) -> SelectRows {
    serde_json::from_value(input).unwrap()
}

#[test]
fn selects_quoted_identifiers_and_keeps_data_in_bindings() {
    let input = select(json!({
        "table": {"name": "Sales]2026", "schema": "Sales Ops"},
        "columns": ["Name", "key"],
        "filters": {"Name": "O'Brien", "key": null},
        "order_by": ["Name"],
        "offset": 10,
        "limit": 5
    }));
    let statement = input.prepare().unwrap();
    assert_eq!(
        statement.sql,
        "SELECT LEFT(CONVERT(nvarchar(max), [Name], 126), 1025) AS [Name], LEFT(CONVERT(nvarchar(max), [key], 126), 1025) AS [key] FROM [Sales Ops].[Sales]]2026] AS [t] WHERE [Name] = @P1 AND [key] IS NULL ORDER BY [t].[Name] OFFSET @P2 ROWS FETCH NEXT @P3 ROWS ONLY"
    );
    assert_eq!(
        statement.parameters,
        vec![json!("O'Brien"), json!(10), json!(5)]
    );
    assert!(!statement.mutation);
}

#[test]
fn unsorted_selects_bound_the_row_limit_with_top() {
    let statement = select(json!({"table": {"name": "Items"}, "columns": ["id"]}))
        .prepare()
        .unwrap();
    assert_eq!(
        statement.sql,
        "SELECT TOP (@P1) LEFT(CONVERT(nvarchar(max), [id], 126), 1025) AS [id] FROM [dbo].[Items] AS [t]"
    );
    assert_eq!(statement.parameters, vec![json!(25)]);
    let statement = select(
        json!({"table": {"name": "Items", "schema": "dbo"}, "columns": ["id"], "limit": 100}),
    )
    .prepare()
    .unwrap();
    assert_eq!(statement.parameters, vec![json!(100)]);
}

#[test]
fn select_rejects_invalid_identifiers_limits_and_unordered_offsets() {
    for (key, value, message) in [
        (
            "columns",
            json!([""]),
            "identifier must contain 1–128 UTF-16 units and no control characters",
        ),
        (
            "columns",
            json!(["id", "id"]),
            "columns must not repeat a name",
        ),
        ("limit", json!(101), "limit must be between 1 and 100"),
        ("offset", json!(1), "offset requires order_by"),
        (
            "filters",
            json!({"id": 18446744073709551615u64}),
            "integer exceeds signed 64-bit range; pass it as a string",
        ),
    ] {
        let mut input = json!({"table": {"name": "Items"}, "columns": ["id"]});
        input[key] = value;
        assert_eq!(select(input).prepare().unwrap_err(), message);
    }
}

#[test]
fn metadata_commands_bind_filters_and_pagination() {
    let list: ListTables =
        serde_json::from_value(json!({"schema": "archive", "offset": 4})).unwrap();
    let list = list.prepare().unwrap();
    assert_eq!(list.parameters, vec![json!(4), json!("archive")]);
    assert!(list.sql.starts_with("SELECT "));
    assert!(list.sql.contains("FROM sys.objects AS o"));
    assert!(list.sql.contains("s.name = @P2"));
    assert!(list.sql.contains("FETCH NEXT 100 ROWS ONLY"));
    let desc: DescribeTable = serde_json::from_value(json!({"table": {"name": "Items"}})).unwrap();
    let desc = desc.prepare().unwrap();
    assert_eq!(
        desc.parameters,
        vec![json!("dbo"), json!("Items"), json!(0)]
    );
    assert!(desc.sql.starts_with("SELECT "));
    assert!(desc.sql.contains("FROM sys.columns AS c"));
    assert!(desc.sql.contains("s.name = @P1 AND o.name = @P2"));
}

#[test]
fn mutations_bind_data_and_use_a_rollback_cap() {
    let input = json!({"table": {"name": "Items"}, "values": {"name": "a'b", "quantity": 8}});
    let insert: InsertRow = serde_json::from_value(input.clone()).unwrap();
    let insert = insert.prepare().unwrap();
    assert!(
        insert
            .sql
            .contains("INSERT INTO [dbo].[Items] ([name], [quantity]) VALUES (@P1, @P2)")
    );
    assert_eq!(insert.parameters, vec![json!("a'b"), json!(8)]);
    let mut input = input;
    input["filters"] = json!({"id": 42});
    let update: UpdateRows = serde_json::from_value(input).unwrap();
    let update = update.prepare().unwrap();
    assert!(update.sql.contains(
        "UPDATE TOP (101) [dbo].[Items] SET [name] = @P1, [quantity] = @P2 WHERE [id] = @P3"
    ));
    assert_eq!(update.parameters, vec![json!("a'b"), json!(8), json!(42)]);
    let delete: DeleteRows =
        serde_json::from_value(json!({"table": {"name": "Items"}, "filters": {"archived": true}}))
            .unwrap();
    let delete = delete.prepare().unwrap();
    assert!(
        delete
            .sql
            .contains("DELETE TOP (101) FROM [dbo].[Items] WHERE [archived] = @P1")
    );
    for statement in [insert, update, delete] {
        assert!(statement.mutation);
        assert!(!statement.sql.contains("a'b"));
        assert!(
            statement
                .sql
                .starts_with("SET NOCOUNT ON; SET XACT_ABORT ON; BEGIN TRANSACTION;\n")
        );
        assert_eq!(statement.sql.matches("SELECT").count(), 1);
        assert!(statement.sql.ends_with(
            "SELECT CONVERT(nvarchar(20), CASE WHEN @mcp_affected > 100 THEN -1 ELSE @mcp_affected END) AS [affected_rows];"
        ));
        assert!(
            statement
                .sql
                .contains("IF @mcp_affected > 100\n    ROLLBACK TRANSACTION")
        );
        assert!(statement.sql.contains("ELSE\n    COMMIT TRANSACTION"));
    }
}

#[test]
fn update_and_delete_reject_empty_filters() {
    let mut input = json!({"table": {"name": "Items"}, "filters": {}});
    let delete: DeleteRows = serde_json::from_value(input.clone()).unwrap();
    input["values"] = json!({"quantity": 8});
    let update: UpdateRows = serde_json::from_value(input).unwrap();
    assert_eq!(
        delete.prepare().unwrap_err(),
        "mutation requires at least one filter"
    );
    assert_eq!(
        update.prepare().unwrap_err(),
        "mutation requires at least one filter"
    );
}

#[test]
fn values_and_filters_stay_within_bounds() {
    let columns = (0..17).map(|index| format!("c{index}")).collect::<Vec<_>>();
    let input = json!({"table": {"name": "Items"}, "columns": columns});
    assert_eq!(
        select(input).prepare().unwrap_err(),
        "columns must contain between 1 and 16 names"
    );
    let input = json!({"table": {"name": "Items"}, "columns": []});
    assert_eq!(
        select(input).prepare().unwrap_err(),
        "columns must contain between 1 and 16 names"
    );
    let order_by = (0..17).map(|index| format!("c{index}")).collect::<Vec<_>>();
    let input = json!({"table": {"name": "Items"}, "columns": ["id"], "order_by": order_by});
    assert_eq!(
        select(input).prepare().unwrap_err(),
        "order_by must contain at most 16 names"
    );
    let filters = (0..17)
        .map(|index| (format!("c{index}"), json!(index)))
        .collect::<serde_json::Map<_, _>>();
    let input = json!({"table": {"name": "Items"}, "columns": ["id"], "filters": filters});
    assert_eq!(
        select(input).prepare().unwrap_err(),
        "filters must contain at most 16 entries"
    );
    let empty: InsertRow =
        serde_json::from_value(json!({"table": {"name": "Items"}, "values": {}})).unwrap();
    assert_eq!(
        empty.prepare().unwrap_err(),
        "values must contain between 1 and 16 entries"
    );
}

#[test]
fn values_reject_unbounded_compound_and_control_character_input() {
    for (values, message) in [
        (
            json!({"id": "x".repeat(16 * 1024 + 1)}),
            "string value must be at most 16384 bytes",
        ),
        (
            json!({"id": [1]}),
            "compound values are not supported; pass a scalar",
        ),
        (
            json!({"id": {"nested": 1}}),
            "compound values are not supported; pass a scalar",
        ),
        (
            json!({"bad\u{0}name": 1}),
            "identifier must contain 1–128 UTF-16 units and no control characters",
        ),
    ] {
        let insert: InsertRow =
            serde_json::from_value(json!({"table": {"name": "Items"}, "values": values})).unwrap();
        assert_eq!(insert.prepare().unwrap_err(), message);
    }
}

#[test]
fn inputs_reject_unknown_fields() {
    let input = json!({"table": {"name": "Items"}, "columns": ["id"], "query": "SELECT 1"});
    assert!(serde_json::from_value::<SelectRows>(input).is_err());
    let input = json!({"table": {"name": "Items"}, "nodescribe": true});
    assert!(serde_json::from_value::<DescribeTable>(input).is_err());
    let input = json!({"table": {"name": "Items", "database": "master"}, "columns": ["id"]});
    assert!(serde_json::from_value::<SelectRows>(input).is_err());
}

#[test]
fn tool_inputs_export_denied_schemas_with_descriptions() {
    let schema = serde_json::to_value(schemars::schema_for!(SelectRows)).unwrap();
    assert_eq!(schema["additionalProperties"], json!(false));
    assert!(schema["properties"]["columns"]["description"].is_string());
    assert!(schema["properties"]["table"]["description"].is_string());
}
