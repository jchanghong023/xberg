//! Diagram recovery from flat ODF drawings (`.fodg`).
//!
//! Unlike SVG, an ODF drawing already names its own structure: every
//! `draw:connector` carries `draw:start-shape` and `draw:end-shape`
//! referencing the `draw:id` of the shapes it joins. Recovery is therefore a
//! lookup by id rather than a geometric match, and it is exact — there is no
//! proximity heuristic to fall back on, and none is needed.

use std::collections::HashMap;

use roxmltree::{Document, Node};

use crate::types::diagram::{DiagramEdge, DiagramGraph, DiagramNode, DiagramShape};

/// Maximum input byte length accepted, matching the cap the SVG recoverer
/// applies before handing input to its parser.
const MAX_INPUT_BYTES: usize = 10 * 1024 * 1024;

const DRAWING_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:drawing:1.0";
const TEXT_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:text:1.0";
const STYLE_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:style:1.0";
const SVG_COMPAT_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0";

/// Resolved fill/stroke styling for one `style:style` entry.
#[derive(Default, Clone)]
struct GraphicStyle {
    fill: Option<String>,
    stroke: Option<String>,
    stroke_width: Option<f32>,
    dashed: bool,
}

/// One shape read from the drawing, keyed by its `draw:id`.
struct ShapeCandidate {
    id: String,
    label: String,
    shape: DiagramShape,
    style: GraphicStyle,
    /// Reading-order sort key: (top edge, left edge) in source units. Shapes
    /// in a single fixture consistently share one unit, so an unconverted
    /// numeric comparison orders them correctly without a full unit parser.
    position: (f32, f32),
}

/// One connector read from the drawing, referencing two `draw:id`s.
struct ConnectorCandidate {
    start_shape: String,
    end_shape: String,
    label: Option<String>,
    style: GraphicStyle,
}

/// Recover a graph from flat ODF drawing (`.fodg`) bytes, or `None` when the
/// source is not a drawing this can read.
pub(crate) fn recover(data: &[u8]) -> Option<DiagramGraph> {
    if data.is_empty() || data.len() > MAX_INPUT_BYTES {
        return None;
    }
    let source = std::str::from_utf8(data).ok()?;
    let document = Document::parse(source).ok()?;
    let root = document.root_element();

    let styles = collect_styles(root);

    let mut shapes = Vec::new();
    let mut connectors = Vec::new();
    for page in drawing_pages(root) {
        collect_page(page, &styles, &mut shapes, &mut connectors);
    }
    if shapes.is_empty() {
        return None;
    }

    shapes.sort_by(|a, b| {
        a.position
            .0
            .total_cmp(&b.position.0)
            .then(a.position.1.total_cmp(&b.position.1))
    });

    let mut index_of: HashMap<&str, usize> = HashMap::with_capacity(shapes.len());
    let nodes = shapes
        .iter()
        .enumerate()
        .map(|(index, shape)| {
            index_of.insert(shape.id.as_str(), index);
            DiagramNode {
                id: format!("n{index}"),
                label: shape.label.clone(),
                shape: shape.shape,
                fill: shape.style.fill.clone(),
                stroke: shape.style.stroke.clone(),
                stroke_width: shape.style.stroke_width,
                dashed: shape.style.dashed,
            }
        })
        .collect();

    let mut edges: Vec<DiagramEdge> = connectors
        .into_iter()
        .filter_map(|connector| {
            let from = *index_of.get(connector.start_shape.as_str())?;
            let to = *index_of.get(connector.end_shape.as_str())?;
            Some(DiagramEdge {
                from,
                to,
                bidirectional: false,
                label: connector.label,
                stroke: connector.style.stroke,
                dashed: connector.style.dashed,
            })
        })
        .collect();
    edges.sort_by(|a, b| a.from.cmp(&b.from).then(a.to.cmp(&b.to)));

    Some(DiagramGraph {
        name: None,
        nodes,
        edges,
    })
}

/// Every `draw:page` under `office:body > office:drawing`, in document order.
fn drawing_pages<'a, 'd>(root: Node<'a, 'd>) -> impl Iterator<Item = Node<'a, 'd>> {
    root.children()
        .filter(|n| n.tag_name().name() == "body")
        .flat_map(|body| body.children().filter(|n| n.tag_name().name() == "drawing"))
        .flat_map(|drawing| drawing.children().filter(|n| n.tag_name().name() == "page"))
}

fn collect_page(
    page: Node,
    styles: &HashMap<String, GraphicStyle>,
    shapes: &mut Vec<ShapeCandidate>,
    connectors: &mut Vec<ConnectorCandidate>,
) {
    for child in page.children().filter(Node::is_element) {
        match child.tag_name().name() {
            "connector" => {
                if let Some(connector) = read_connector(child, styles) {
                    connectors.push(connector);
                }
            }
            // Every other draw:* shape with an id is a node candidate: custom
            // shapes today, with room for rect/ellipse/circle should a fixture
            // exercise them later without touching the connector matching. ~keep
            _ => {
                if let Some(shape) = read_shape(child, styles) {
                    shapes.push(shape);
                }
            }
        }
    }
}

fn read_shape(node: Node, styles: &HashMap<String, GraphicStyle>) -> Option<ShapeCandidate> {
    let id = attribute(node, DRAWING_NS, "id")?.to_string();
    let label = paragraph_text(node);
    let shape = classify_shape(node);
    let style = style_name(node)
        .and_then(|name| styles.get(name))
        .cloned()
        .unwrap_or_default();
    let position = (
        length_value(
            node.attribute((SVG_COMPAT_NS, "y"))
                .or_else(|| node.attribute("svg:y"))
                .unwrap_or_default(),
        ),
        length_value(
            node.attribute((SVG_COMPAT_NS, "x"))
                .or_else(|| node.attribute("svg:x"))
                .unwrap_or_default(),
        ),
    );
    Some(ShapeCandidate {
        id,
        label,
        shape,
        style,
        position,
    })
}

fn read_connector(node: Node, styles: &HashMap<String, GraphicStyle>) -> Option<ConnectorCandidate> {
    let start_shape = attribute(node, DRAWING_NS, "start-shape")?.to_string();
    let end_shape = attribute(node, DRAWING_NS, "end-shape")?.to_string();
    let label = {
        let text = paragraph_text(node);
        (!text.is_empty()).then_some(text)
    };
    let style = style_name(node)
        .and_then(|name| styles.get(name))
        .cloned()
        .unwrap_or_default();
    Some(ConnectorCandidate {
        start_shape,
        end_shape,
        label,
        style,
    })
}

/// `draw:id` on a shape or connector, tried under its namespace first and as
/// a literal `draw:` prefix second, matching how the sibling ODF extractors
/// (`extractors/odp.rs`) resolve attributes that a producer may or may not
/// declare with a bound namespace.
fn attribute<'a>(node: Node<'a, '_>, namespace: &str, local_name: &str) -> Option<&'a str> {
    node.attribute((namespace, local_name))
        .or_else(|| node.attribute(format!("draw:{local_name}").as_str()))
}

fn style_name<'a>(node: Node<'a, '_>) -> Option<&'a str> {
    node.attribute((DRAWING_NS, "style-name"))
        .or_else(|| node.attribute("draw:style-name"))
}

/// Concatenate every `text:p` descendant's text, one paragraph per line.
fn paragraph_text(node: Node) -> String {
    node.descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "p" && n.tag_name().namespace() == Some(TEXT_NS))
        .map(element_text)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn element_text(node: Node) -> String {
    node.descendants()
        .filter(|n| n.is_text())
        .filter_map(|n| n.text())
        .collect::<String>()
        .trim()
        .to_string()
}

fn classify_shape(node: Node) -> DiagramShape {
    let geometry_type = node
        .children()
        .find(|n| n.tag_name().name() == "enhanced-geometry")
        .and_then(|geometry| {
            geometry
                .attribute((DRAWING_NS, "type"))
                .or_else(|| geometry.attribute("draw:type"))
        });
    match geometry_type {
        Some("ellipse") | Some("circle") => DiagramShape::Ellipse,
        Some("diamond") => DiagramShape::Diamond,
        Some(_) | None => match node.tag_name().name() {
            "ellipse" | "circle" => DiagramShape::Ellipse,
            _ => DiagramShape::Box,
        },
    }
}

/// Numeric magnitude of a length attribute such as `svg:x`/`svg:y`/
/// `svg:stroke-width`, ignoring its unit suffix. Good enough to order shapes
/// drawn in one consistent unit; not a general length parser.
fn length_value(value: &str) -> f32 {
    let numeric_end = value
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
        .unwrap_or(value.len());
    value[..numeric_end].parse().unwrap_or(0.0)
}

/// Build a `style:name -> GraphicStyle` map from every graphic
/// `style:style` under `office:automatic-styles`.
fn collect_styles(root: Node) -> HashMap<String, GraphicStyle> {
    let Some(automatic_styles) = root.children().find(|n| n.tag_name().name() == "automatic-styles") else {
        return HashMap::new();
    };

    automatic_styles
        .children()
        .filter(|n| n.tag_name().name() == "style")
        .filter_map(|style| {
            let name = style
                .attribute((STYLE_NS, "name"))
                .or_else(|| style.attribute("style:name"))?;
            let properties = style.children().find(|n| n.tag_name().name() == "graphic-properties")?;
            Some((name.to_string(), read_graphic_style(properties)))
        })
        .collect()
}

fn read_graphic_style(properties: Node) -> GraphicStyle {
    let fill_mode = properties
        .attribute((DRAWING_NS, "fill"))
        .or_else(|| properties.attribute("draw:fill"));
    let fill = if fill_mode == Some("none") {
        None
    } else {
        properties
            .attribute((DRAWING_NS, "fill-color"))
            .or_else(|| properties.attribute("draw:fill-color"))
            .map(str::to_string)
    };
    let stroke = properties
        .attribute((SVG_COMPAT_NS, "stroke-color"))
        .or_else(|| properties.attribute("svg:stroke-color"))
        .map(str::to_string);
    let stroke_width = properties
        .attribute((SVG_COMPAT_NS, "stroke-width"))
        .or_else(|| properties.attribute("svg:stroke-width"))
        .map(length_value);
    let dashed = matches!(
        properties
            .attribute((DRAWING_NS, "stroke"))
            .or_else(|| properties.attribute("draw:stroke")),
        Some("dash")
    );

    GraphicStyle {
        fill,
        stroke,
        stroke_width,
        dashed,
    }
}
