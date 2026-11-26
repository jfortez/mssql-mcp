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

const NODE_WIDTH: usize = 230;
const NODE_HEIGHT: usize = 54;
const SCHEMA_COLUMNS: usize = 3;
const SCHEMA_PADDING: usize = 14;
const SCHEMA_HEADER: usize = 34;
const COLUMN_GAP: usize = 14;
const ROW_GAP: usize = 26;
const FRAME_X: usize = 12;
const FRAME_GAP: usize = 16;
const LANE_GAP: usize = 9;
/// Distance above a row gap's centre where a horizontal run feeds into a box.
const GAP_INTO_ROW: usize = 6;

/// Horizontal band between two rows of boxes, where an edge can run.
fn row_gap_y(node_y: usize) -> usize {
    node_y + NODE_HEIGHT + ROW_GAP / 2
}

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
    let mut positions = BTreeMap::<i32, (usize, usize, usize)>::new();
    let mut frames = Vec::new();
    let mut nodes = Vec::new();
    let mut top = 12;
    let mut widest = 0;
    for (schema, tables) in &schemas {
        let columns = tables.len().clamp(1, SCHEMA_COLUMNS);
        let rows = tables.len().div_ceil(columns);
        let frame_width =
            SCHEMA_PADDING * 2 + columns * NODE_WIDTH + columns.saturating_sub(1) * COLUMN_GAP;
        let frame_height = SCHEMA_HEADER + rows * (NODE_HEIGHT + ROW_GAP);
        widest = widest.max(frame_width);
        frames.push((schema, top, frame_width, frame_height));
        for (index, (id, node)) in tables.iter().enumerate() {
            let x = FRAME_X + SCHEMA_PADDING + (index % columns) * (NODE_WIDTH + COLUMN_GAP);
            let y = top + SCHEMA_HEADER + (index / columns) * (NODE_HEIGHT + ROW_GAP);
            positions.insert(*id, (x, y, row_gap_y(y)));
            nodes.push(node_markup(node, x, y));
        }
        top += frame_height + FRAME_GAP;
    }

    // One lane per table pair keeps repeated constraints readable. Lanes live in
    // a gutter right of every frame, and the horizontal runs stay inside the row
    // gaps, so no line crosses a table box.
    let mut lanes = BTreeMap::<(i32, i32), Vec<&Edge<'_>>>::new();
    for edge in &map.edges {
        if positions.contains_key(&edge.child) && positions.contains_key(&edge.referenced) {
            lanes
                .entry((edge.child, edge.referenced))
                .or_default()
                .push(edge);
        }
    }
    let mut ordered = lanes.into_iter().collect::<Vec<_>>();
    ordered.sort_by_key(|((child, referenced), _)| (*referenced, *child));
    let gutter = FRAME_X + widest + 24;
    let width = gutter + 8 + ordered.len().max(1) * LANE_GAP;
    let height = top + 4;
    let _ = writeln!(
        html,
        r#"<figure><div class="canvas" role="region" aria-label="Scrollable diagram" tabindex="0">
<svg role="img" viewBox="0 0 {width} {height}" width="{width}" height="{height}">
<title>Tables grouped by schema, with foreign keys pointing to the referenced table</title>
<defs><marker id="arrow" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto"><path class="arrow" d="M0 0 L8 4 L0 8 Z"/></marker></defs>"#
    );
    for (schema, frame_top, frame_width, frame_height) in &frames {
        let label_y = frame_top + 24;
        let _ = writeln!(
            html,
            r#"<rect class="schema-frame" x="{FRAME_X}" y="{frame_top}" width="{frame_width}" height="{frame_height}" rx="8"/>
<text class="label" x="{}" y="{label_y}"><title>{}</title>{}</text>"#,
            FRAME_X + SCHEMA_PADDING,
            escape(schema),
            short_label(schema)
        );
    }
    for markup in &nodes {
        html.push_str(markup);
    }
    for (index, ((child, referenced), edges)) in ordered.iter().enumerate() {
        let (Some((child_x, child_y, child_gap)), Some((target_x, _, target_gap))) =
            (positions.get(child), positions.get(referenced))
        else {
            continue;
        };
        let lane = gutter + index * LANE_GAP;
        let names = edges
            .iter()
            .map(|edge| escape(edge.name))
            .collect::<Vec<_>>()
            .join(", ");
        let exit_x = child_x + NODE_WIDTH / 2;
        let entry_x = target_x + NODE_WIDTH / 2;
        let _ = writeln!(
            html,
            r#"<path class="edge" marker-end="url(#arrow)" d="M{exit_x} {} V{child_gap} H{lane} V{target_gap} H{entry_x} V{}"><title>{names}</title></path>"#,
            child_y + NODE_HEIGHT,
            target_gap - GAP_INTO_ROW
        );
    }
    let _ = writeln!(
        html,
        "</svg></div><figcaption>Each arrow leaves the bottom of a child table and enters the table it references. Tables outside the selection are dashed. Scroll the diagram in both directions; {} lanes are ordered by target.</figcaption></figure>\n<h2>Tables by schema</h2>",
        ordered.len()
    );
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

fn node_markup(node: &Node<'_>, x: usize, y: usize) -> String {
    let border_class = if node.columns.is_some() {
        "node"
    } else {
        "node external"
    };
    let details = node.columns.map_or_else(
        || "External table".to_owned(),
        |count| format!("{count} columns"),
    );
    format!(
        r#"<g><title>{}.{}</title><rect class="{border_class}" x="{x}" y="{y}" width="{NODE_WIDTH}" height="{NODE_HEIGHT}" rx="5"/>
<text class="label" x="{}" y="{}">{}</text><text class="metadata" x="{}" y="{}">{details}</text></g>
"#,
        escape(node.schema),
        escape(node.table),
        x + 12,
        y + 21,
        short_label(node.table),
        x + 12,
        y + 40
    )
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
