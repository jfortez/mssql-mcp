//! `explore_schema` tool: one bounded, indented catalog tree.
//!
//! The tree is assembled from parameterized catalog queries that reuse the
//! statement planner, so identifiers never become SQL. Every query is bounded
//! like a read (100 rows, 16 columns, 1,024 UTF-16 units per cell), and the
//! rendered tree has its own size budget. Any cut sets `truncated` and ends the
//! output with a visible `… truncated` line. Cuts inside one table or one body
//! are also marked where they happen (`… N columns omitted`,
//! `… N definition units omitted`) so a bounded page never silently drops part
//! of an object.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::database::ReadOutput;
use crate::statement::{Binder, MAX_ROWS, Statement, validate_identifier};

const MAX_TREE_UNITS: usize = 16_384;
const TRUNCATION_LINE: &str = "… truncated";
const INCLUDE_ERROR: &str =
    "include must contain at least one of tables, views, procedures, functions";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExploreSchema {
    /// Restrict the tree to this schema.
    pub schema: Option<String>,
    /// Object kinds to include; defaults to all four.
    pub include: Option<Vec<Include>>,
    /// Inline procedure and function bodies from sys.sql_modules.
    #[serde(default)]
    pub definition: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Include {
    Tables,
    Views,
    Procedures,
    Functions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Part {
    Tables,
    Columns,
    Relations,
    Views,
    Procedures,
    Functions,
    Definitions,
}

impl ExploreSchema {
    /// Catalog queries in execution order; `None` means the plan validation
    /// failed before any connection could be opened.
    pub(crate) fn plan(&self) -> Result<Vec<(Part, Statement)>, &'static str> {
        if let Some(schema) = &self.schema {
            validate_identifier(schema)?;
        }
        let includes = match &self.include {
            Some(list) if list.is_empty() => return Err(INCLUDE_ERROR),
            Some(list) => list.iter().copied().collect::<BTreeSet<_>>(),
            None => [
                Include::Tables,
                Include::Views,
                Include::Procedures,
                Include::Functions,
            ]
            .into_iter()
            .collect(),
        };

        let mut plan = Vec::new();
        if includes.contains(&Include::Tables) {
            plan.push((Part::Tables, tables_statement(self.schema.as_deref())?));
            plan.push((Part::Columns, columns_statement(self.schema.as_deref())?));
            plan.push((
                Part::Relations,
                relations_statement(self.schema.as_deref())?,
            ));
        }
        if includes.contains(&Include::Views) {
            plan.push((Part::Views, views_statement(self.schema.as_deref())?));
        }
        if includes.contains(&Include::Procedures) {
            plan.push((
                Part::Procedures,
                procedures_statement(self.schema.as_deref())?,
            ));
        }
        if includes.contains(&Include::Functions) {
            plan.push((
                Part::Functions,
                functions_statement(self.schema.as_deref())?,
            ));
        }
        if self.definition
            && (includes.contains(&Include::Procedures) || includes.contains(&Include::Functions))
        {
            let mut types = Vec::new();
            if includes.contains(&Include::Procedures) {
                types.push("N'P', N'PC'");
            }
            if includes.contains(&Include::Functions) {
                types.push("N'FN', N'IF', N'TF', N'FS', N'FT', N'AF'");
            }
            plan.push((
                Part::Definitions,
                definitions_statement(self.schema.as_deref(), &types.join(", "))?,
            ));
        }
        Ok(plan)
    }
}

fn filter_and_offset(
    binder: &mut Binder,
    schema: Option<&str>,
) -> Result<(String, String, u16), &'static str> {
    let filter = binder.bind(match schema {
        Some(name) => Value::String(name.to_owned()),
        None => Value::Null,
    })?;
    let offset = binder.bind(Value::from(0u32))?;
    Ok((filter, offset, MAX_ROWS + 1))
}

fn statement(sql: String, binder: Binder) -> Statement {
    Statement::read(sql, binder.parameters, usize::from(MAX_ROWS))
}

fn tables_statement(schema: Option<&str>) -> Result<Statement, &'static str> {
    let mut binder = Binder::default();
    let (filter, offset, page) = filter_and_offset(&mut binder, schema)?;
    let sql = format!(
        "SELECT
             CONVERT(nvarchar(128), s.name) AS [schema],
             CONVERT(nvarchar(128), o.name) AS [name],
             CONVERT(nvarchar(20), (SELECT COUNT(*) FROM sys.columns AS c WHERE c.object_id = o.object_id)) AS [column_count]
         FROM sys.objects AS o
         JOIN sys.schemas AS s ON s.schema_id = o.schema_id
         WHERE o.is_ms_shipped = 0 AND o.type = N'U' AND ({filter} IS NULL OR s.name = {filter})
         ORDER BY s.name, o.name
         OFFSET {offset} ROWS FETCH NEXT {page} ROWS ONLY"
    );
    Ok(statement(sql, binder))
}

fn columns_statement(schema: Option<&str>) -> Result<Statement, &'static str> {
    let mut binder = Binder::default();
    let (filter, offset, page) = filter_and_offset(&mut binder, schema)?;
    let sql = format!(
        "SELECT
             CONVERT(nvarchar(128), s.name) AS [schema],
             CONVERT(nvarchar(128), o.name) AS [table_name],
             CONVERT(nvarchar(128), c.name) AS [name],
             CONVERT(nvarchar(256), t.name + CASE
                 WHEN t.name IN (N'nvarchar', N'nchar') THEN
                     N'(' + CASE WHEN c.max_length = -1 THEN N'max' ELSE CONVERT(nvarchar(10), c.max_length / 2) END + N')'
                 WHEN t.name IN (N'varchar', N'char', N'varbinary', N'binary') THEN
                     N'(' + CASE WHEN c.max_length = -1 THEN N'max' ELSE CONVERT(nvarchar(10), c.max_length) END + N')'
                 WHEN t.name IN (N'decimal', N'numeric') THEN
                     N'(' + CONVERT(nvarchar(10), c.precision) + N',' + CONVERT(nvarchar(10), c.scale) + N')'
                 WHEN t.name IN (N'datetime2', N'time', N'datetimeoffset') THEN
                     N'(' + CONVERT(nvarchar(10), c.scale) + N')'
                 ELSE N''
             END) AS [data_type],
             CONVERT(nvarchar(5), c.is_nullable) AS [is_nullable],
             CONVERT(nvarchar(5), CASE WHEN EXISTS (
                 SELECT 1
                 FROM sys.indexes AS i
                 JOIN sys.index_columns AS ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id
                 WHERE i.object_id = o.object_id AND i.is_primary_key = 1 AND ic.column_id = c.column_id
             ) THEN 1 ELSE 0 END) AS [is_primary_key]
         FROM sys.objects AS o
         JOIN sys.schemas AS s ON s.schema_id = o.schema_id
         JOIN sys.columns AS c ON c.object_id = o.object_id
         JOIN sys.types AS t ON t.user_type_id = c.user_type_id
         WHERE o.is_ms_shipped = 0 AND o.type = N'U' AND ({filter} IS NULL OR s.name = {filter})
         ORDER BY s.name, o.name, c.column_id
         OFFSET {offset} ROWS FETCH NEXT {page} ROWS ONLY"
    );
    Ok(statement(sql, binder))
}

fn relations_statement(schema: Option<&str>) -> Result<Statement, &'static str> {
    let mut binder = Binder::default();
    let (filter, offset, page) = filter_and_offset(&mut binder, schema)?;
    let sql = format!(
        "SELECT
             CONVERT(nvarchar(128), ps.name) AS [parent_schema],
             CONVERT(nvarchar(128), po.name) AS [parent_table],
             CONVERT(nvarchar(128), pc.name) AS [parent_column],
             CONVERT(nvarchar(128), rs.name) AS [referenced_schema],
             CONVERT(nvarchar(128), ro.name) AS [referenced_table],
             CONVERT(nvarchar(128), rc.name) AS [referenced_column],
             CONVERT(nvarchar(128), fk.name) AS [name],
             CONVERT(nvarchar(20), fkc.constraint_column_id) AS [ordinal]
         FROM sys.foreign_keys AS fk
         JOIN sys.foreign_key_columns AS fkc ON fkc.constraint_object_id = fk.object_id
         JOIN sys.objects AS po ON po.object_id = fk.parent_object_id
         JOIN sys.schemas AS ps ON ps.schema_id = po.schema_id
         JOIN sys.columns AS pc ON pc.object_id = po.object_id AND pc.column_id = fkc.parent_column_id
         JOIN sys.objects AS ro ON ro.object_id = fk.referenced_object_id
         JOIN sys.schemas AS rs ON rs.schema_id = ro.schema_id
         JOIN sys.columns AS rc ON rc.object_id = ro.object_id AND rc.column_id = fkc.referenced_column_id
         WHERE po.is_ms_shipped = 0 AND ro.is_ms_shipped = 0
           AND ({filter} IS NULL OR ps.name = {filter} OR rs.name = {filter})
         ORDER BY ps.name, po.name, fk.name, fkc.constraint_column_id
         OFFSET {offset} ROWS FETCH NEXT {page} ROWS ONLY"
    );
    Ok(statement(sql, binder))
}

fn views_statement(schema: Option<&str>) -> Result<Statement, &'static str> {
    let mut binder = Binder::default();
    let (filter, offset, page) = filter_and_offset(&mut binder, schema)?;
    let sql = format!(
        "SELECT
             CONVERT(nvarchar(128), s.name) AS [schema],
             CONVERT(nvarchar(128), o.name) AS [name],
             CONVERT(nvarchar(20), (SELECT COUNT(*) FROM sys.columns AS c WHERE c.object_id = o.object_id)) AS [column_count]
         FROM sys.objects AS o
         JOIN sys.schemas AS s ON s.schema_id = o.schema_id
         WHERE o.is_ms_shipped = 0 AND o.type = N'V' AND ({filter} IS NULL OR s.name = {filter})
         ORDER BY s.name, o.name
         OFFSET {offset} ROWS FETCH NEXT {page} ROWS ONLY"
    );
    Ok(statement(sql, binder))
}

fn procedures_statement(schema: Option<&str>) -> Result<Statement, &'static str> {
    let mut binder = Binder::default();
    let (filter, offset, page) = filter_and_offset(&mut binder, schema)?;
    let sql = format!(
        "SELECT
             CONVERT(nvarchar(128), s.name) AS [schema],
             CONVERT(nvarchar(128), o.name) AS [name],
             CONVERT(nvarchar(20), ISNULL(DATALENGTH(m.definition), 0) / 2) AS [definition_units],
             ISNULL(p.parameters, N'') AS [parameters]
         FROM sys.objects AS o
         JOIN sys.schemas AS s ON s.schema_id = o.schema_id
         LEFT JOIN sys.sql_modules AS m ON m.object_id = o.object_id
         OUTER APPLY (
             SELECT CONVERT(nvarchar(4000), STRING_AGG(
                        CONVERT(nvarchar(max), pr.name + N' ' + TYPE_NAME(pr.user_type_id)
                            + CASE WHEN pr.is_output = 1 THEN N' OUT' ELSE N'' END),
                        N', ')
                    WITHIN GROUP (ORDER BY pr.parameter_id)) AS [parameters]
             FROM sys.parameters AS pr
             WHERE pr.object_id = o.object_id AND pr.parameter_id > 0
         ) AS p
         WHERE o.is_ms_shipped = 0 AND o.type IN (N'P', N'PC')
           AND ({filter} IS NULL OR s.name = {filter})
         ORDER BY s.name, o.name
         OFFSET {offset} ROWS FETCH NEXT {page} ROWS ONLY"
    );
    Ok(statement(sql, binder))
}

fn functions_statement(schema: Option<&str>) -> Result<Statement, &'static str> {
    let mut binder = Binder::default();
    let (filter, offset, page) = filter_and_offset(&mut binder, schema)?;
    let sql = format!(
        "SELECT
             CONVERT(nvarchar(128), s.name) AS [schema],
             CONVERT(nvarchar(128), o.name) AS [name],
             CONVERT(nvarchar(128), CASE
                 WHEN o.type IN (N'IF', N'TF') THEN N'table'
                 ELSE ISNULL(rt.return_type, N'unknown')
             END) AS [return_type],
             CONVERT(nvarchar(20), ISNULL(DATALENGTH(m.definition), 0) / 2) AS [definition_units],
             ISNULL(p.parameters, N'') AS [parameters]
         FROM sys.objects AS o
         JOIN sys.schemas AS s ON s.schema_id = o.schema_id
         LEFT JOIN sys.sql_modules AS m ON m.object_id = o.object_id
         OUTER APPLY (
             SELECT CONVERT(nvarchar(128), TYPE_NAME(rp.user_type_id)) AS [return_type]
             FROM sys.parameters AS rp
             WHERE rp.object_id = o.object_id AND rp.parameter_id = 0
         ) AS rt
         OUTER APPLY (
             SELECT CONVERT(nvarchar(4000), STRING_AGG(
                        CONVERT(nvarchar(max), pr.name + N' ' + TYPE_NAME(pr.user_type_id)
                            + CASE WHEN pr.is_output = 1 THEN N' OUT' ELSE N'' END),
                        N', ')
                    WITHIN GROUP (ORDER BY pr.parameter_id)) AS [parameters]
             FROM sys.parameters AS pr
             WHERE pr.object_id = o.object_id AND pr.parameter_id > 0
         ) AS p
         WHERE o.is_ms_shipped = 0
           AND o.type IN (N'FN', N'IF', N'TF', N'FS', N'FT', N'AF')
           AND ({filter} IS NULL OR s.name = {filter})
         ORDER BY s.name, o.name
         OFFSET {offset} ROWS FETCH NEXT {page} ROWS ONLY"
    );
    Ok(statement(sql, binder))
}

fn definitions_statement(schema: Option<&str>, types: &str) -> Result<Statement, &'static str> {
    let mut binder = Binder::default();
    let (filter, offset, page) = filter_and_offset(&mut binder, schema)?;
    let sql = format!(
        "SELECT
             CONVERT(nvarchar(128), s.name) AS [schema],
             CONVERT(nvarchar(128), o.name) AS [name],
             -- One unit over the cell bound lets the reader detect the cut.
             LEFT(CONVERT(nvarchar(max), m.definition), 1025) AS [definition]
         FROM sys.objects AS o
         JOIN sys.schemas AS s ON s.schema_id = o.schema_id
         JOIN sys.sql_modules AS m ON m.object_id = o.object_id
         WHERE o.is_ms_shipped = 0
           AND o.type IN ({types})
           AND ({filter} IS NULL OR s.name = {filter})
         ORDER BY s.name, o.name
         OFFSET {offset} ROWS FETCH NEXT {page} ROWS ONLY"
    );
    Ok(statement(sql, binder))
}

#[derive(Default)]
struct TableRow {
    schema: String,
    name: String,
    column_count: usize,
}

struct ColumnRow {
    schema: String,
    table: String,
    name: String,
    data_type: String,
    nullable: bool,
    primary_key: bool,
}

struct RelationRow {
    parent_schema: String,
    parent_table: String,
    parent_column: String,
    referenced_schema: String,
    referenced_table: String,
    referenced_column: String,
    name: String,
    ordinal: usize,
}

/// One foreign key: the constraint identity and every column pair it covers.
struct RelationEdge<'a> {
    child_schema: &'a str,
    child_table: &'a str,
    parent_schema: &'a str,
    parent_table: &'a str,
    name: &'a str,
    /// Child → parent columns in constraint ordinal order.
    pairs: Vec<(&'a str, &'a str)>,
}

impl RelationEdge<'_> {
    /// Constraint identity plus both column lists, so two constraints to the
    /// same target stay distinguishable.
    fn label(&self) -> String {
        let (child, parent) = pair_columns(&self.pairs);
        format!("{}: {child} → {parent}", self.name)
    }
}

struct ViewRow {
    schema: String,
    name: String,
    column_count: usize,
}

struct RoutineRow {
    schema: String,
    name: String,
    parameters: String,
    definition_units: usize,
}

struct FunctionRow {
    schema: String,
    name: String,
    parameters: String,
    definition_units: usize,
    return_type: String,
}

#[derive(Default)]
pub(crate) struct Catalog {
    tables: Vec<TableRow>,
    columns: Vec<ColumnRow>,
    relations: Vec<RelationRow>,
    views: Vec<ViewRow>,
    procedures: Vec<RoutineRow>,
    functions: Vec<FunctionRow>,
    definitions: BTreeMap<(String, String), String>,
    /// Set as soon as the definitions page runs, so an absent body can be told
    /// apart from a body nobody asked for.
    definitions_read: bool,
    truncated: bool,
}

impl Catalog {
    pub(crate) fn absorb(&mut self, part: Part, read: ReadOutput) {
        self.truncated |= read.truncated;
        if part == Part::Definitions {
            self.definitions_read = true;
        }
        for row in read.rows {
            match part {
                Part::Tables => self.tables.push(TableRow {
                    schema: cell(&row, 0),
                    name: cell(&row, 1),
                    column_count: number(&row, 2),
                }),
                Part::Columns => self.columns.push(ColumnRow {
                    schema: cell(&row, 0),
                    table: cell(&row, 1),
                    name: cell(&row, 2),
                    data_type: cell(&row, 3),
                    nullable: flag(&row, 4),
                    primary_key: flag(&row, 5),
                }),
                Part::Relations => self.relations.push(RelationRow {
                    parent_schema: cell(&row, 0),
                    parent_table: cell(&row, 1),
                    parent_column: cell(&row, 2),
                    referenced_schema: cell(&row, 3),
                    referenced_table: cell(&row, 4),
                    referenced_column: cell(&row, 5),
                    name: cell(&row, 6),
                    ordinal: number(&row, 7),
                }),
                Part::Views => self.views.push(ViewRow {
                    schema: cell(&row, 0),
                    name: cell(&row, 1),
                    column_count: number(&row, 2),
                }),
                Part::Procedures => self.procedures.push(RoutineRow {
                    schema: cell(&row, 0),
                    name: cell(&row, 1),
                    definition_units: number(&row, 2),
                    parameters: cell(&row, 3),
                }),
                Part::Functions => self.functions.push(FunctionRow {
                    schema: cell(&row, 0),
                    name: cell(&row, 1),
                    return_type: cell(&row, 2),
                    definition_units: number(&row, 3),
                    parameters: cell(&row, 4),
                }),
                Part::Definitions => {
                    let body = cell(&row, 2);
                    if !body.is_empty() {
                        self.definitions
                            .insert((cell(&row, 0), cell(&row, 1)), body);
                    }
                }
            }
        }
    }

    pub(crate) fn render(&self, filter: Option<&str>) -> (String, bool) {
        let edges = self.relation_edges();
        let mut cut = self.truncated;
        let mut schemas: BTreeMap<&str, ()> = BTreeMap::new();
        for table in &self.tables {
            schemas.insert(&table.schema, ());
        }
        for column in &self.columns {
            schemas.insert(&column.schema, ());
        }
        for view in &self.views {
            schemas.insert(&view.schema, ());
        }
        for procedure in &self.procedures {
            schemas.insert(&procedure.schema, ());
        }
        for function in &self.functions {
            schemas.insert(&function.schema, ());
        }

        let mut roots = Vec::new();
        for schema in schemas.keys().copied() {
            let mut sections = Vec::new();
            if !self.tables.is_empty() || !self.columns.is_empty() {
                let (tables, omitted) = self.table_nodes(schema, &edges);
                cut |= omitted;
                if !tables.is_empty() {
                    sections.push(Node::Branch("tables".to_owned(), tables));
                }
            }
            if !self.views.is_empty() {
                let views = self.view_nodes(schema);
                if !views.is_empty() {
                    sections.push(Node::Branch("views".to_owned(), views));
                }
            }
            if !self.procedures.is_empty() {
                let (procedures, omitted) = self.procedure_nodes(schema);
                cut |= omitted;
                if !procedures.is_empty() {
                    sections.push(Node::Branch("procedures".to_owned(), procedures));
                }
            }
            if !self.functions.is_empty() {
                let (functions, omitted) = self.function_nodes(schema);
                cut |= omitted;
                if !functions.is_empty() {
                    sections.push(Node::Branch("functions".to_owned(), functions));
                }
            }
            roots.push(Node::Branch(schema.to_owned(), sections));
        }

        if roots.is_empty() {
            match filter {
                Some(schema) => roots.push(Node::Branch(
                    schema.to_owned(),
                    vec![Node::Leaf("(no objects)".to_owned())],
                )),
                None => roots.push(Node::Leaf("(no objects)".to_owned())),
            }
        }

        let mut tree = Tree::default();
        render_nodes(&roots, "", &mut tree);
        let truncated = cut || tree.truncated;
        if truncated {
            tree.finish();
        }
        (tree.text, truncated)
    }

    /// Foreign keys grouped by constraint identity and ordered by constraint
    /// ordinal, so a composite key renders as one edge with every column pair.
    fn relation_edges(&self) -> Vec<RelationEdge<'_>> {
        let mut groups: BTreeMap<(&str, &str, &str), Vec<&RelationRow>> = BTreeMap::new();
        for relation in &self.relations {
            groups
                .entry((
                    &relation.parent_schema,
                    &relation.parent_table,
                    &relation.name,
                ))
                .or_default()
                .push(relation);
        }
        groups
            .into_iter()
            .map(|((child_schema, child_table, name), mut rows)| {
                rows.sort_by_key(|row| row.ordinal);
                RelationEdge {
                    child_schema,
                    child_table,
                    parent_schema: rows[0].referenced_schema.as_str(),
                    parent_table: rows[0].referenced_table.as_str(),
                    name,
                    pairs: rows
                        .iter()
                        .map(|row| (row.parent_column.as_str(), row.referenced_column.as_str()))
                        .collect(),
                }
            })
            .collect()
    }

    /// Table nodes for one schema plus whether any table lost columns to the
    /// bounded column page.
    fn table_nodes(&self, schema: &str, edges: &[RelationEdge<'_>]) -> (Vec<Node>, bool) {
        let mut names = BTreeSet::new();
        for table in self.tables.iter().filter(|table| table.schema == schema) {
            names.insert(table.name.as_str());
        }
        for column in self.columns.iter().filter(|column| column.schema == schema) {
            names.insert(column.table.as_str());
        }
        let mut cut = false;
        let nodes = names
            .into_iter()
            .map(|name| {
                let columns = self
                    .columns
                    .iter()
                    .filter(|column| column.schema == schema && column.table == name)
                    .collect::<Vec<_>>();
                let count = self
                    .tables
                    .iter()
                    .find(|table| table.schema == schema && table.name == name)
                    .map_or(columns.len(), |table| table.column_count);
                let primary_key = columns
                    .iter()
                    .filter(|column| column.primary_key)
                    .map(|column| column.name.as_str())
                    .collect::<Vec<_>>();
                let mut label = format!("{name} ({count} {})", plural(count, "column", "columns"));
                if !primary_key.is_empty() {
                    label.push_str(&format!(" pk ({})", primary_key.join(", ")));
                }
                let mut children = columns
                    .iter()
                    .map(|column| {
                        Node::Leaf(format!(
                            "{} {} {}",
                            column.name,
                            column.data_type,
                            if column.nullable { "NULL" } else { "NOT NULL" }
                        ))
                    })
                    .collect::<Vec<_>>();
                if count > columns.len() {
                    children.push(omitted_columns(count - columns.len()));
                    cut = true;
                }
                for edge in edges
                    .iter()
                    .filter(|edge| edge.child_schema == schema && edge.child_table == name)
                {
                    children.push(Node::Leaf(format!(
                        "outgoing → {}.{} ({})",
                        edge.parent_schema,
                        edge.parent_table,
                        edge.label()
                    )));
                }
                for edge in edges
                    .iter()
                    .filter(|edge| edge.parent_schema == schema && edge.parent_table == name)
                {
                    children.push(Node::Leaf(format!(
                        "incoming ← {}.{} ({})",
                        edge.child_schema,
                        edge.child_table,
                        edge.label()
                    )));
                }
                Node::Branch(label, children)
            })
            .collect();
        (nodes, cut)
    }

    fn view_nodes(&self, schema: &str) -> Vec<Node> {
        self.views
            .iter()
            .filter(|view| view.schema == schema)
            .map(|view| {
                Node::Leaf(format!(
                    "{} ({} {})",
                    view.name,
                    view.column_count,
                    plural(view.column_count, "column", "columns")
                ))
            })
            .collect()
    }

    fn procedure_nodes(&self, schema: &str) -> (Vec<Node>, bool) {
        let mut cut = false;
        let nodes = self
            .procedures
            .iter()
            .filter(|procedure| procedure.schema == schema)
            .map(|procedure| {
                let label = format!(
                    "{}({}) definition: {} units",
                    procedure.name, procedure.parameters, procedure.definition_units
                );
                let (node, omitted) =
                    self.routine_node(schema, &procedure.name, label, procedure.definition_units);
                cut |= omitted;
                node
            })
            .collect();
        (nodes, cut)
    }

    fn function_nodes(&self, schema: &str) -> (Vec<Node>, bool) {
        let mut cut = false;
        let nodes = self
            .functions
            .iter()
            .filter(|function| function.schema == schema)
            .map(|function| {
                let label = format!(
                    "{}({}) returns {} definition: {} units",
                    function.name,
                    function.parameters,
                    function.return_type,
                    function.definition_units
                );
                let (node, omitted) =
                    self.routine_node(schema, &function.name, label, function.definition_units);
                cut |= omitted;
                node
            })
            .collect();
        (nodes, cut)
    }

    /// Body branch for one routine; a body the bounded read could not deliver
    /// in full is marked with the units left out.
    fn routine_node(
        &self,
        schema: &str,
        name: &str,
        label: String,
        definition_units: usize,
    ) -> (Node, bool) {
        match self.definitions.get(&(schema.to_owned(), name.to_owned())) {
            Some(body) => {
                let omitted = definition_units.saturating_sub(body.encode_utf16().count());
                let mut children = body_nodes(body);
                if omitted > 0 {
                    children.push(omitted_definition_units(omitted));
                }
                (Node::Branch(label, children), omitted > 0)
            }
            None if self.definitions_read && definition_units > 0 => (
                Node::Branch(label, vec![omitted_definition_units(definition_units)]),
                true,
            ),
            None => (Node::Leaf(label), false),
        }
    }
}

enum Node {
    Leaf(String),
    Branch(String, Vec<Node>),
}

impl Node {
    fn label(&self) -> &str {
        match self {
            Self::Leaf(label) | Self::Branch(label, _) => label,
        }
    }
}

#[derive(Default)]
struct Tree {
    text: String,
    units: usize,
    truncated: bool,
}

impl Tree {
    fn line(&mut self, line: &str) -> bool {
        let line_units = line.encode_utf16().count();
        let reserved = TRUNCATION_LINE.encode_utf16().count() + 1;
        let separator = usize::from(!self.text.is_empty());
        if self.units + separator + line_units + reserved > MAX_TREE_UNITS {
            self.truncated = true;
            return false;
        }
        if !self.text.is_empty() {
            self.text.push('\n');
            self.units += 1;
        }
        self.text.push_str(line);
        self.units += line_units;
        true
    }

    fn finish(&mut self) {
        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(TRUNCATION_LINE);
    }
}

fn render_nodes(nodes: &[Node], prefix: &str, tree: &mut Tree) -> bool {
    for (index, node) in nodes.iter().enumerate() {
        let last = index + 1 == nodes.len();
        let connector = if last { "└─ " } else { "├─ " };
        if !tree.line(&format!("{prefix}{connector}{}", node.label())) {
            return false;
        }
        if let Node::Branch(_, children) = node {
            let child_prefix = format!("{prefix}{}", if last { "   " } else { "│  " });
            if !render_nodes(children, &child_prefix, tree) {
                return false;
            }
        }
    }
    true
}

fn body_nodes(body: &str) -> Vec<Node> {
    let mut nodes = body
        .lines()
        .map(|line| Node::Leaf(line.to_owned()))
        .collect::<Vec<_>>();
    if nodes.is_empty() {
        nodes.push(Node::Leaf("(empty definition)".to_owned()));
    }
    nodes
}

fn omitted_columns(count: usize) -> Node {
    Node::Leaf(format!("… {count} columns omitted"))
}

fn omitted_definition_units(count: usize) -> Node {
    Node::Leaf(format!("… {count} definition units omitted"))
}

/// Child and parent column lists of one constraint; composites keep the
/// parentheses so each pair stays readable.
fn pair_columns(pairs: &[(&str, &str)]) -> (String, String) {
    let child = pairs
        .iter()
        .map(|(child, _)| *child)
        .collect::<Vec<_>>()
        .join(", ");
    let parent = pairs
        .iter()
        .map(|(_, parent)| *parent)
        .collect::<Vec<_>>()
        .join(", ");
    if pairs.len() == 1 {
        (child, parent)
    } else {
        (format!("({child})"), format!("({parent})"))
    }
}

fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

fn cell(row: &[Option<String>], index: usize) -> String {
    row.get(index).cloned().flatten().unwrap_or_default()
}

fn number(row: &[Option<String>], index: usize) -> usize {
    cell(row, index).parse().unwrap_or(0)
}

fn flag(row: &[Option<String>], index: usize) -> bool {
    matches!(cell(row, index).as_str(), "1" | "true" | "TRUE")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(rows: &[&[&str]], truncated: bool) -> ReadOutput {
        ReadOutput {
            columns: Vec::new(),
            rows: rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|cell| (!cell.is_empty()).then(|| (*cell).to_owned()))
                        .collect()
                })
                .collect(),
            truncated,
        }
    }

    fn catalog() -> Catalog {
        let mut catalog = Catalog::default();
        catalog.absorb(
            Part::Tables,
            read(&[&["dbo", "Groups", "2"], &["dbo", "Items", "3"]], false),
        );
        catalog.absorb(
            Part::Columns,
            read(
                &[
                    &["dbo", "Groups", "id", "int", "0", "1"],
                    &["dbo", "Groups", "label", "nvarchar(50)", "1", "0"],
                    &["dbo", "Items", "id", "int", "0", "1"],
                    &["dbo", "Items", "group_id", "int", "1", "0"],
                    &["dbo", "Items", "name", "nvarchar(50)", "1", "0"],
                ],
                false,
            ),
        );
        catalog.absorb(
            Part::Relations,
            read(
                &[&[
                    "dbo",
                    "Items",
                    "group_id",
                    "sales",
                    "Groups",
                    "id",
                    "FK_Items_Groups",
                    "1",
                ]],
                false,
            ),
        );
        catalog.absorb(Part::Views, read(&[&["dbo", "ActiveItems", "2"]], false));
        catalog.absorb(
            Part::Procedures,
            read(&[&["dbo", "GetItems", "25", "@id int"]], false),
        );
        catalog.absorb(
            Part::Functions,
            read(&[&["dbo", "Total", "int", "14", "@x int"]], false),
        );
        catalog.absorb(
            Part::Definitions,
            read(
                &[
                    &["dbo", "GetItems", "SELECT id FROM dbo.Items;"],
                    &["dbo", "Total", "RETURN @x + 1;"],
                ],
                false,
            ),
        );
        catalog
    }

    #[test]
    fn tree_shows_schemas_tables_columns_relations_views_and_routines() {
        let (tree, truncated) = catalog().render(None);
        assert!(!truncated, "{tree}");
        for expected in [
            "dbo",
            "├─ tables",
            "Groups (2 columns) pk (id)",
            "label nvarchar(50) NULL",
            "id int NOT NULL",
            "outgoing → sales.Groups (FK_Items_Groups: group_id → id)",
            "├─ views",
            "ActiveItems (2 columns)",
            "├─ procedures",
            "GetItems(@id int) definition: 25 units",
            "SELECT id FROM dbo.Items;",
            "└─ functions",
            "Total(@x int) returns int definition: 14 units",
            "RETURN @x + 1;",
        ] {
            assert!(tree.contains(expected), "missing {expected:?} in:\n{tree}");
        }
        assert!(
            !tree.contains("omitted"),
            "a complete page must not mark a cut:\n{tree}"
        );
    }

    #[test]
    fn incoming_relations_are_rendered_under_the_referenced_table() {
        let mut catalog = catalog();
        catalog.absorb(Part::Tables, read(&[&["sales", "Groups", "1"]], false));
        catalog.absorb(
            Part::Columns,
            read(
                &[
                    &["sales", "Groups", "id", "int", "0", "1"],
                    &["sales", "Groups", "name", "nvarchar(50)", "1", "0"],
                ],
                false,
            ),
        );
        let (tree, truncated) = catalog.render(None);
        assert!(!truncated, "{tree}");
        assert!(
            tree.contains("incoming ← dbo.Items (FK_Items_Groups: group_id → id)"),
            "incoming relation missing:\n{tree}"
        );
    }

    #[test]
    fn definition_bodies_are_inlined_and_cuts_are_marked() {
        let mut catalog = Catalog::default();
        catalog.absorb(
            Part::Procedures,
            read(&[&["dbo", "P", "9", "@id int"]], false),
        );
        catalog.absorb(
            Part::Functions,
            read(&[&["dbo", "F", "int", "10", "@x int"]], false),
        );
        catalog.absorb(
            Part::Definitions,
            read(
                &[&["dbo", "P", "SELECT 1;"], &["dbo", "F", "RETURN @x;"]],
                true,
            ),
        );
        let (tree, truncated) = catalog.render(None);
        assert!(truncated);
        for expected in [
            "P(@id int) definition: 9 units",
            "SELECT 1;",
            "F(@x int) returns int definition: 10 units",
            "RETURN @x;",
        ] {
            assert!(tree.contains(expected), "missing {expected:?} in:\n{tree}");
        }
        assert!(tree.ends_with(TRUNCATION_LINE), "{tree}");
    }

    #[test]
    fn definition_flag_adds_a_body_query_only_when_requested() {
        let request = |include, definition| ExploreSchema {
            schema: None,
            include: Some(include),
            definition,
        };
        let both = [Include::Procedures, Include::Functions];
        assert!(
            !request(both.to_vec(), false)
                .plan()
                .expect("plan")
                .iter()
                .any(|(part, _)| *part == Part::Definitions)
        );
        let definitions = request(both.to_vec(), true)
            .plan()
            .expect("plan")
            .into_iter()
            .find(|(part, _)| *part == Part::Definitions)
            .expect("definitions query");
        assert!(definitions.1.sql.contains("N'P', N'PC'"));
        assert!(definitions.1.sql.contains("N'FN'"));

        let procedures = request(vec![Include::Procedures], true)
            .plan()
            .expect("plan")
            .into_iter()
            .find(|(part, _)| *part == Part::Definitions)
            .expect("definitions query");
        assert!(procedures.1.sql.contains("N'P', N'PC'"));
        assert!(!procedures.1.sql.contains("N'FN'"));
    }

    #[test]
    fn a_truncated_read_adds_the_visible_truncation_line() {
        let mut catalog = catalog();
        catalog.truncated = true;
        let (tree, truncated) = catalog.render(None);
        assert!(truncated);
        assert!(tree.ends_with(TRUNCATION_LINE), "{tree}");
    }

    #[test]
    fn the_tree_budget_cuts_and_marks_truncation() {
        let mut catalog = Catalog::default();
        catalog.absorb(
            Part::Procedures,
            read(&[&["dbo", "Huge", "20000", "@id int"]], false),
        );
        catalog.absorb(
            Part::Definitions,
            read(&[&["dbo", "Huge", &"x".repeat(20_000)]], false),
        );
        let (tree, truncated) = catalog.render(None);
        assert!(truncated, "{tree}");
        assert!(tree.ends_with(TRUNCATION_LINE), "{tree}");
        assert!(
            tree.encode_utf16().count() <= MAX_TREE_UNITS,
            "tree exceeds its budget: {} units",
            tree.encode_utf16().count()
        );
        assert!(tree.contains("Huge(@id int) definition: 20000 units"));
    }

    #[test]
    fn an_empty_filter_renders_the_requested_schema() {
        let (tree, truncated) = Catalog::default().render(Some("dbo"));
        assert!(!truncated);
        assert!(
            tree.contains("dbo") && tree.contains("(no objects)"),
            "{tree}"
        );
    }

    #[test]
    fn a_table_whose_columns_were_cut_marks_the_omitted_columns() {
        let mut catalog = Catalog::default();
        catalog.absorb(Part::Tables, read(&[&["dbo", "Items", "22"]], false));
        let names = (1..=13)
            .map(|column| format!("c{column}"))
            .collect::<Vec<_>>();
        let rows = names
            .iter()
            .map(|name| vec!["dbo", "Items", name.as_str(), "int", "0", "0"])
            .collect::<Vec<_>>();
        let rows = rows.iter().map(Vec::as_slice).collect::<Vec<_>>();
        catalog.absorb(Part::Columns, read(&rows, false));

        let (tree, truncated) = catalog.render(None);

        assert!(tree.contains("Items (22 columns)"), "{tree}");
        assert!(tree.contains("… 9 columns omitted"), "{tree}");
        assert_eq!(
            13,
            tree.matches(" int NOT NULL").count(),
            "every received column still renders:\n{tree}"
        );
        assert!(truncated, "a cut column list must set truncated:\n{tree}");
    }

    #[test]
    fn distinct_constraints_to_one_target_keep_their_child_columns() {
        let mut catalog = Catalog::default();
        catalog.absorb(
            Part::Tables,
            read(
                &[&["Sales", "Orders", "2"], &["Sales", "Customers", "1"]],
                false,
            ),
        );
        catalog.absorb(
            Part::Columns,
            read(
                &[
                    &["Sales", "Orders", "ShipToAddressID", "int", "0", "0"],
                    &["Sales", "Orders", "BillToAddressID", "int", "0", "0"],
                    &["Sales", "Customers", "CustomerID", "int", "0", "1"],
                ],
                false,
            ),
        );
        catalog.absorb(
            Part::Relations,
            read(
                &[
                    &[
                        "Sales",
                        "Orders",
                        "ShipToAddressID",
                        "Sales",
                        "Customers",
                        "CustomerID",
                        "FK_Orders_ShipTo",
                        "1",
                    ],
                    &[
                        "Sales",
                        "Orders",
                        "BillToAddressID",
                        "Sales",
                        "Customers",
                        "CustomerID",
                        "FK_Orders_BillTo",
                        "1",
                    ],
                ],
                false,
            ),
        );

        let (tree, truncated) = catalog.render(None);

        assert!(!truncated, "{tree}");
        assert_eq!(2, tree.matches("outgoing →").count(), "{tree}");
        for expected in [
            "outgoing → Sales.Customers (FK_Orders_ShipTo: ShipToAddressID → CustomerID)",
            "outgoing → Sales.Customers (FK_Orders_BillTo: BillToAddressID → CustomerID)",
        ] {
            assert!(tree.contains(expected), "missing {expected:?} in:\n{tree}");
        }
    }

    #[test]
    fn a_composite_constraint_renders_every_pair_once_in_ordinal_order() {
        let mut catalog = Catalog::default();
        catalog.absorb(
            Part::Tables,
            read(&[&["dbo", "Child", "1"], &["dbo", "Parent", "1"]], false),
        );
        catalog.absorb(
            Part::Columns,
            read(
                &[
                    &["dbo", "Child", "part_no", "int", "0", "0"],
                    &["dbo", "Parent", "id", "int", "0", "1"],
                ],
                false,
            ),
        );
        catalog.absorb(
            Part::Relations,
            read(
                &[
                    &[
                        "dbo", "Child", "region", "dbo", "Parent", "region", "FK_Two", "2",
                    ],
                    &[
                        "dbo", "Child", "part_no", "dbo", "Parent", "id", "FK_Two", "1",
                    ],
                ],
                false,
            ),
        );

        let (tree, truncated) = catalog.render(None);

        assert!(!truncated, "{tree}");
        assert_eq!(
            1,
            tree.matches("outgoing →").count(),
            "one edge per constraint:\n{tree}"
        );
        assert!(
            tree.contains("outgoing → dbo.Parent (FK_Two: (part_no, region) → (id, region))"),
            "{tree}"
        );
    }

    #[test]
    fn an_incoming_constraint_names_the_child_table_and_both_columns() {
        let mut catalog = catalog();
        catalog.absorb(Part::Tables, read(&[&["sales", "Groups", "1"]], false));
        catalog.absorb(
            Part::Columns,
            read(&[&["sales", "Groups", "id", "int", "0", "1"]], false),
        );

        let (tree, truncated) = catalog.render(None);

        assert!(!truncated, "{tree}");
        assert!(
            tree.contains("incoming ← dbo.Items (FK_Items_Groups: group_id → id)"),
            "incoming relation missing both columns:\n{tree}"
        );
    }

    #[test]
    fn a_body_cut_by_the_bounded_read_marks_the_omitted_units() {
        let mut catalog = Catalog::default();
        catalog.absorb(
            Part::Procedures,
            read(&[&["dbo", "GetItems", "4000", "@id int"]], false),
        );
        catalog.absorb(
            Part::Definitions,
            read(&[&["dbo", "GetItems", "SELECT 1;"]], false),
        );

        let (tree, truncated) = catalog.render(None);

        assert!(
            tree.contains("GetItems(@id int) definition: 4000 units"),
            "{tree}"
        );
        assert!(tree.contains("SELECT 1;"), "{tree}");
        assert!(tree.contains("… 3991 definition units omitted"), "{tree}");
        assert!(truncated, "a cut body must set truncated:\n{tree}");
    }

    #[test]
    fn a_body_left_out_of_the_bounded_read_is_marked_omitted() {
        let mut catalog = Catalog::default();
        catalog.absorb(
            Part::Procedures,
            read(&[&["dbo", "GetItems", "400", "@id int"]], false),
        );
        catalog.absorb(Part::Definitions, read(&[], false));

        let (tree, truncated) = catalog.render(None);

        assert!(
            tree.contains("GetItems(@id int) definition: 400 units"),
            "{tree}"
        );
        assert!(tree.contains("… 400 definition units omitted"), "{tree}");
        assert!(truncated, "a missing body must set truncated:\n{tree}");
    }

    #[test]
    fn a_routine_whose_body_was_not_requested_is_not_marked_as_cut() {
        let mut catalog = Catalog::default();
        catalog.absorb(
            Part::Procedures,
            read(&[&["dbo", "GetItems", "400", "@id int"]], false),
        );

        let (tree, truncated) = catalog.render(None);

        assert!(!truncated, "{tree}");
        assert!(
            tree.contains("GetItems(@id int) definition: 400 units") && !tree.contains("omitted"),
            "{tree}"
        );
    }

    #[test]
    fn a_routine_without_a_body_is_not_marked_as_cut() {
        let mut catalog = Catalog::default();
        catalog.absorb(
            Part::Procedures,
            read(&[&["dbo", "GetItems", "0", "@id int"]], false),
        );
        catalog.absorb(Part::Definitions, read(&[], false));

        let (tree, truncated) = catalog.render(None);

        assert!(!truncated, "{tree}");
        assert!(
            tree.contains("GetItems(@id int) definition: 0 units") && !tree.contains("omitted"),
            "{tree}"
        );
    }
}
