use std::collections::BTreeMap;
use std::fmt::Write as _;

pub(super) struct Node<'a> {
    pub schema: &'a str,
    pub table: &'a str,
    pub columns: Option<usize>,
}

pub(super) struct Edge<'a> {
    pub name: &'a str,
    pub child: i32,
    pub referenced: i32,
    pub columns: Vec<(&'a str, &'a str)>,
}

#[derive(Default)]
pub(super) struct Diagram<'a> {
    pub nodes: BTreeMap<i32, Node<'a>>,
    pub edges: Vec<Edge<'a>>,
    pub expressions_truncated: bool,
}

const NODE_X: usize = 28;
const NODE_WIDTH: usize = 270;
const NODE_HEIGHT: usize = 54;
const EDGE_X: usize = NODE_X + NODE_WIDTH;

const DOCUMENT: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>SQL Server schema map</title>
<style>
:root { color-scheme: light; font: 16px system-ui, sans-serif; color: #112836; background: #f4f8fa; }
body { max-width: 76rem; margin: auto; padding: 1rem; }
h1 { margin-block: .5rem; }
.canvas { overflow: auto; max-block-size: 70vh; background: #fff; border: 1px solid #9eafb9; }
svg { display: block; }
.schema-frame { fill: #edf4f7; stroke: #9eafb9; }
.node { fill: #fff; stroke: #426678; }
.external { stroke-dasharray: 5 3; }
.edge { fill: none; stroke: #0b6373; stroke-width: 1.5; }
.arrow { fill: #0b6373; }
.label { fill: #112836; font-size: 14px; font-weight: 650; }
.metadata { fill: #426678; font-size: 12px; }
li { overflow-wrap: anywhere; }
</style></head><body><main><h1>SQL Server schema map</h1>
"#;

pub(super) fn render(map: &Diagram<'_>) -> String {
    let mut html = DOCUMENT.to_owned();
    let mut schemas = BTreeMap::<&str, Vec<(i32, &Node<'_>)>>::new();
    for (id, node) in &map.nodes {
        schemas.entry(node.schema).or_default().push((*id, node));
    }
    for tables in schemas.values_mut() {
        tables.sort_by_key(|(_, node)| node.table);
    }

    let _ = writeln!(
        html,
        "<p>{} tables in view · {} foreign-key constraints. Dashed boxes are outside the selection.</p>",
        map.nodes.len(),
        map.edges.len()
    );
    if map.expressions_truncated {
        html.push_str("<p>Some catalog expressions were truncated. JSON identifies the affected fields.</p>\n");
    }
    if schemas.is_empty() {
        html.push_str("<p>No visible tables matched the selection.</p></main></body></html>\n");
        return html;
    }
    let width = EDGE_X + 64 + map.edges.len() * 11;
    let height = 26 + 64 * schemas.len() + (NODE_HEIGHT + 8) * map.nodes.len();
    let _ = writeln!(
        html,
        r#"<figure><div class="canvas" role="region" aria-label="Scrollable diagram" tabindex="0">
<svg role="img" viewBox="0 0 {width} {height}" width="{width}" height="{height}">
<title>Tables grouped by schema, with foreign keys pointing to the referenced table</title>
<defs><marker id="arrow" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto"><path class="arrow" d="M0 0 L8 4 L0 8 Z"/></marker></defs>"#
    );

    let mut positions = BTreeMap::<i32, usize>::new();
    let mut top = 12;
    for (schema, tables) in &schemas {
        let group_height = 50 + (NODE_HEIGHT + 8) * tables.len();
        let label_y = top + 24;
        let _ = writeln!(
            html,
            r#"<rect class="schema-frame" x="12" y="{top}" width="{}" height="{group_height}" rx="8"/>
<text class="label" x="{NODE_X}" y="{label_y}"><title>{}</title>{}</text>"#,
            NODE_WIDTH + 32,
            escape(schema),
            short_label(schema)
        );
        let mut y = top + 36;
        for (id, node) in tables {
            positions.insert(*id, y);
            let border_class = if node.columns.is_some() {
                "node"
            } else {
                "node external"
            };
            let details = node.columns.map_or_else(
                || "External table".to_owned(),
                |count| format!("{count} columns"),
            );
            let _ = writeln!(
                html,
                r#"<g><title>{}.{}</title><rect class="{border_class}" x="{NODE_X}" y="{y}" width="{NODE_WIDTH}" height="{NODE_HEIGHT}" rx="5"/>
<text class="label" x="40" y="{}">{}</text><text class="metadata" x="40" y="{}">{details}</text></g>"#,
                escape(node.schema),
                escape(node.table),
                y + 21,
                short_label(node.table),
                y + 40
            );
            y += NODE_HEIGHT + 8;
        }
        top += group_height + 14;
    }
    for (index, edge) in map.edges.iter().enumerate() {
        if let (Some(child), Some(target)) =
            (positions.get(&edge.child), positions.get(&edge.referenced))
        {
            let lane = EDGE_X + 38 + index * 11;
            let child_y = child + 19;
            let target_y = target + 36;
            let _ = writeln!(
                html,
                r#"<path class="edge" marker-end="url(#arrow)" d="M{EDGE_X} {child_y} H{lane} V{target_y} H{EDGE_X}"><title>{}</title></path>"#,
                escape(edge.name)
            );
        }
    }
    html.push_str("</svg></div><figcaption>Arrows point from the child table to the referenced table. Scroll the diagram to see all lanes.</figcaption></figure>\n<h2>Tables by schema</h2>\n");
    for (schema, tables) in &schemas {
        let _ = writeln!(html, "<h3>{}</h3><ul>", escape(schema));
        for (_, node) in tables {
            let details = node.columns.map_or_else(
                || "External table".to_owned(),
                |count| format!("{count} columns"),
            );
            let _ = writeln!(html, "<li>{} ({details})</li>", escape(node.table));
        }
        html.push_str("</ul>\n");
    }
    html.push_str("<h2>Foreign-key relationships</h2>\n");
    if map.edges.is_empty() {
        html.push_str("<p>No visible foreign-key constraints matched the selection.</p>\n");
    } else {
        html.push_str("<ul>\n");
        for edge in &map.edges {
            let (Some(child), Some(target)) =
                (map.nodes.get(&edge.child), map.nodes.get(&edge.referenced))
            else {
                continue;
            };
            let child = format!("{}.{}", escape(child.schema), escape(child.table));
            let target = format!("{}.{}", escape(target.schema), escape(target.table));
            let _ = write!(html, "<li>{}: ", escape(edge.name));
            if edge.columns.is_empty() {
                let _ = write!(html, "{child} → {target} (columns unavailable)");
            }
            for (index, (source, referenced)) in edge.columns.iter().enumerate() {
                if index != 0 {
                    html.push_str("; ");
                }
                let _ = write!(
                    html,
                    "{child}.{} → {target}.{}",
                    escape(source),
                    escape(referenced)
                );
            }
            html.push_str("</li>\n");
        }
        html.push_str("</ul>\n");
    }
    html.push_str("</main></body></html>\n");
    html
}

fn short_label(text: &str) -> String {
    let mut chars = text.chars();
    let mut visible: String = chars.by_ref().take(30).collect();
    if chars.next().is_some() {
        visible.push('…');
    }
    escape(&visible)
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_cross_schema_edges_with_both_column_pairs() {
        let map = Diagram {
            nodes: BTreeMap::from([
                (
                    1,
                    Node {
                        schema: "Sales",
                        table: "Orders",
                        columns: Some(5),
                    },
                ),
                (
                    2,
                    Node {
                        schema: "People",
                        table: "Customers",
                        columns: None,
                    },
                ),
            ]),
            edges: vec![Edge {
                name: "FK_Order_Customer",
                child: 1,
                referenced: 2,
                columns: vec![("Customer", "ID"), ("Region", "RegionID")],
            }],
            expressions_truncated: true,
        };
        let html = render(&map);
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<svg role=\"img\""));
        assert_eq!(html.matches("class=\"schema-frame\"").count(), 2);
        assert_eq!(html.matches("class=\"edge\"").count(), 1);
        assert!(html.contains("Orders") && html.contains("Customers"));
        assert!(html.contains("External table"));
        assert!(html.contains("FK_Order_Customer: Sales.Orders.Customer → People.Customers.ID"));
        assert!(html.contains("Sales.Orders.Region → People.Customers.RegionID"));
        assert!(html.contains("Some catalog expressions were truncated"));
    }

    #[test]
    fn treats_catalog_labels_as_text() {
        let map = Diagram {
            nodes: BTreeMap::from([(
                1,
                Node {
                    schema: "A&B",
                    table: "<orders>",
                    columns: Some(0),
                },
            )]),
            ..Default::default()
        };
        let html = render(&map);
        assert!(html.contains("A&amp;B") && html.contains("&lt;orders&gt;"));
        assert!(!html.contains("<orders>"));
        assert!(!html.contains("<script") && !html.contains("<link"));
    }
}
