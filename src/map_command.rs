use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::sync::Arc;

use mssql_mcp::catalog::{CatalogEntry, CatalogError, ObjectType, SchemaMap};
use mssql_mcp::config::{MapFormat, MapOptions, Settings};
use mssql_mcp::database::Database;
use serde::Serialize;

use crate::map_html::{self, Diagram, Edge, Node};

#[derive(Serialize)]
struct Export<'a> {
    schema: Option<&'a str>,
    truncated: bool,
    entries: Vec<CatalogEntry>,
}

pub(super) async fn export(options: &MapOptions, settings: Settings) -> Result<(), String> {
    let database = Database::new(Arc::new(settings));
    let mut request = SchemaMap {
        schema: options.schema.clone(),
        include: None,
        table: None,
        offset: 0,
        limit: None,
    };
    let mut export = Export {
        schema: options.schema.as_deref(),
        truncated: false,
        entries: Vec::new(),
    };
    let mut expected_total = None;
    loop {
        let page = request
            .plan()
            .map_err(str::to_owned)?
            .execute(&database)
            .await
            .map_err(catalog_error)?;
        if expected_total
            .replace(page.total)
            .is_some_and(|total| total != page.total)
        {
            return Err("the catalog changed during export; no file was created".to_owned());
        }
        export.truncated |= page.truncated;
        export.entries.extend(page.entries);
        match page.next_offset {
            Some(next) if next > request.offset => request.offset = next,
            None if export.entries.len() as u64 == page.total => break,
            _ => return Err("the catalog changed during export; no file was created".to_owned()),
        }
    }

    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&options.out)
        .map_err(|error| format!("cannot create output file: {error}"))?;
    let mut file = BufWriter::new(file);
    match options.format {
        MapFormat::Json => {
            serde_json::to_writer_pretty(&mut file, &export)
                .map_err(|error| format!("cannot write JSON: {error}"))?;
            file.write_all(b"\n").map_err(output_error)?;
        }
        MapFormat::Html => {
            let diagram = diagram(&export);
            file.write_all(map_html::render(&diagram).as_bytes())
                .map_err(output_error)?;
        }
    }
    file.flush().map_err(output_error)
}

fn catalog_error(error: CatalogError) -> String {
    match error {
        CatalogError::Database(error) => error.message(false).to_owned(),
        CatalogError::NotFound => "catalog object was not found".to_owned(),
        CatalogError::UnexpectedResult => {
            "database returned an unexpected catalog result".to_owned()
        }
    }
}

fn output_error(error: std::io::Error) -> String {
    format!("cannot write output file: {error}")
}

fn diagram<'a>(export: &'a Export<'_>) -> Diagram<'a> {
    let mut output = Diagram {
        expressions_truncated: export.truncated,
        ..Diagram::default()
    };
    let mut pairs = BTreeMap::<_, Vec<_>>::new();
    for entry in &export.entries {
        match entry {
            CatalogEntry::Object(object) if matches!(object.object_type, ObjectType::Table) => {
                output.nodes.insert(
                    object.object_id,
                    Node {
                        schema: &object.schema,
                        table: &object.name,
                        columns: Some(0),
                    },
                );
            }
            CatalogEntry::ForeignKeyColumn(column) => {
                pairs.entry(column.foreign_key_id).or_default().push((
                    column.ordinal,
                    column.parent_column.as_str(),
                    column.referenced_column.as_str(),
                ));
            }
            _ => {}
        }
    }
    for entry in &export.entries {
        match entry {
            CatalogEntry::Column(column) => {
                if let Some(Node {
                    columns: Some(count),
                    ..
                }) = output.nodes.get_mut(&column.object_id)
                {
                    *count += 1;
                }
            }
            CatalogEntry::ForeignKey(key) => {
                output.nodes.entry(key.parent_object_id).or_insert(Node {
                    schema: &key.parent_schema,
                    table: &key.parent_table,
                    columns: None,
                });
                output
                    .nodes
                    .entry(key.referenced_object_id)
                    .or_insert(Node {
                        schema: &key.referenced_schema,
                        table: &key.referenced_table,
                        columns: None,
                    });
                let mut columns = pairs.remove(&key.foreign_key_id).unwrap_or_default();
                columns.sort_by_key(|(ordinal, _, _)| *ordinal);
                output.edges.push(Edge {
                    name: &key.name,
                    child: key.parent_object_id,
                    referenced: key.referenced_object_id,
                    columns: columns
                        .into_iter()
                        .map(|(_, child, target)| (child, target))
                        .collect(),
                });
            }
            _ => {}
        }
    }
    output
}
