//! Screen S4 / Component: In-Terminal ERD Diagram Workspace using `flowmaid`.
//! Renders database schema relationships with interactive pan.
//!
//! The painter is ported from `examples/flowmaid_spike.rs` — flowmaid 0.25
//! only produces SVG, but its public `er::scene` / `er::glyph` API gives
//! pixel-space geometry that we can map onto a `ratatui::widgets::canvas`
//! (Marker::Braille) plus direct `Buffer` writes for the entity boxes.

use flowmaid::er::{self, ErScene};
use flowmaid::model::{Attr, Card, Document, EdgeKind, ErDiagram, Key, Relation};
use flowmaid::parser::parse_document;
use flowmaid::scene::Hit;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Circle, Line as CanvasLine};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget};

use crate::driver::{CollectionMeta, Namespace};
use crate::theme::Theme;

/// Smallest terminal the painter accepts (cols x rows).
const MIN_W: u16 = 60;
const MIN_H: u16 = 15;

/// px→cell scale. Horizontal matches flowmaid's ~7px/char text metrics so
/// table text survives the transform; vertical keeps one attribute row
/// (`er::ROW_H` = 22px) on one terminal line.
const PX_PER_COL: f64 = 7.0;
const PX_PER_ROW: f64 = 22.0;

/// Pan step (in pixel space) for hjkl / arrow keys.
const PAN_STEP_COLS: f64 = 4.0;

/// Extra margin (in terminal cells) allowed when panning past the left/top
/// edge, so content can be brought comfortably into view instead of hugging
/// the canvas border.
const OVERSCROLL_CELLS: f64 = 60.0;

/// Border accents cycled over entities (mirrors flowmaid's per-entity
/// accent idea, terminal edition).
const ACCENTS: [Color; 6] = [
    Color::Cyan,
    Color::Green,
    Color::Magenta,
    Color::Yellow,
    Color::Blue,
    Color::Red,
];

/// Viewport: world pixel coordinate shown at the top-left of the canvas,
/// plus a zoom factor (1.0 = default scale).
#[derive(Clone, Copy, Debug)]
pub struct View {
    pub ox: f64,
    pub oy: f64,
    /// Zoom multiplier applied to the px-per-cell scale. `> 1` zooms in
    /// (bigger glyphs), `< 1` zooms out (more of the diagram visible).
    pub zoom: f64,
}

impl Default for View {
    fn default() -> Self {
        Self {
            ox: 0.0,
            oy: 0.0,
            zoom: 1.0,
        }
    }
}

impl View {
    /// Effective horizontal scale (px per terminal column) at the current zoom.
    pub fn px_col(&self) -> f64 {
        PX_PER_COL * self.zoom
    }

    /// Effective vertical scale (px per terminal row) at the current zoom.
    pub fn px_row(&self) -> f64 {
        PX_PER_ROW * self.zoom
    }
}

#[derive(Clone, Debug)]
pub struct ErdTab {
    pub namespace: Namespace,
    /// Cached flowmaid layout. Computed once in `generate_from_meta`,
    /// re-used across every pan/resize. None while `is_loading` or on
    /// parse failure.
    pub scene: Option<ErScene>,
    /// Scene pixel width / height (cached so pan clamping doesn't have
    /// to borrow the optional `scene` field).
    pub scene_w: f64,
    pub scene_h: f64,
    /// Top-left world-pixel coordinate currently visible.
    pub view: View,
    /// `true` while a `generate_from_meta` is in flight or before the user
    /// has pressed `g` at least once. Drives the spinner state.
    pub is_loading: bool,
    /// Last error from `generate_from_meta`, if any. Rendered as the
    /// breadcrumb body when `scene` is None and we're not loading.
    pub last_error: Option<String>,
    /// The canvas `Rect` painted by the most recent `render_erd` call
    /// (coordinates in the terminal). Used to map a mouse click back to
    /// scene space for `node_at_mouse` — mirrors exactly what was drawn,
    /// so panning/resizing can't desync the hit-test.
    pub last_canvas_area: Option<Rect>,
    /// Index into `scene.scene.nodes` currently selected (click or Tab).
    /// Rendered with a highlighted border.
    pub selected_node: Option<usize>,
    /// Mermaid `erDiagram` source for the current diagram — the portable
    /// form used by the export, and what a reader can paste anywhere.
    pub mermaid: String,
}

/// Render an `ErDiagram` back to Mermaid `erDiagram` source.
///
/// The scene only carries geometry, so this reads the model instead — which
/// also means the export stays valid no matter how the view is panned.
fn er_to_mermaid(er: &ErDiagram) -> String {
    /// Mermaid writes cardinality from the perspective of each side, and the
    /// left marker is mirrored.
    fn card(c: Card, left: bool) -> &'static str {
        match (c, left) {
            (Card::One, _) => "||",
            (Card::ZeroOne, true) => "|o",
            (Card::ZeroOne, false) => "o|",
            (Card::ZeroMany, true) => "}o",
            (Card::ZeroMany, false) => "o{",
            (Card::OneMany, true) => "}|",
            (Card::OneMany, false) => "|{",
        }
    }
    /// Mermaid identifiers can't contain dots or spaces; quote when needed.
    fn ident(name: &str) -> String {
        if name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            name.to_string()
        } else {
            format!("\"{}\"", name.replace('"', ""))
        }
    }

    let mut out = String::from("erDiagram\n");
    for e in &er.entities {
        if e.attrs.is_empty() {
            // An entity with no attributes still has to be declared.
            out.push_str(&format!("    {}\n", ident(&e.name)));
            continue;
        }
        out.push_str(&format!("    {} {{\n", ident(&e.name)));
        for a in &e.attrs {
            let keys: Vec<&str> = a
                .keys
                .iter()
                .map(|k| match k {
                    Key::Pk => "PK",
                    Key::Fk => "FK",
                    Key::Uk => "UK",
                })
                .collect();
            // Mermaid takes multiple key markers space-separated ("PK UK");
            // a comma-joined list is rejected by the parser.
            let suffix = if keys.is_empty() {
                String::new()
            } else {
                format!(" {}", keys.join(" "))
            };
            // Mermaid types may not contain spaces (e.g. `character varying`).
            let ty = a.ty.replace(' ', "_");
            out.push_str(&format!("        {} {}{}\n", ty, a.name, suffix));
        }
        out.push_str("    }\n");
    }
    for r in &er.relations {
        let (Some(from), Some(to)) = (er.entities.get(r.from), er.entities.get(r.to)) else {
            continue;
        };
        let line = if r.identifying { "--" } else { ".." };
        out.push_str(&format!(
            "    {} {}{}{} {} : {}\n",
            ident(&from.name),
            card(r.card_from, true),
            line,
            card(r.card_to, false),
            ident(&to.name),
            r.label.clone().unwrap_or_else(|| "has".to_string())
        ));
    }
    out
}

impl ErdTab {
    pub fn new(namespace: Namespace) -> Self {
        Self {
            namespace,
            scene: None,
            scene_w: 0.0,
            scene_h: 0.0,
            view: View::default(),
            is_loading: true,
            last_error: None,
            last_canvas_area: None,
            selected_node: None,
            mermaid: String::new(),
        }
    }

    /// Build an `ErDiagram` from live schema metadata and cache its layout.
    /// We skip the Mermaid text round-trip (cleaner + no parser quirks) and
    /// feed the model straight into flowmaid's scene.
    pub fn generate_from_meta(&mut self, collections: &[CollectionMeta]) {
        let er = build_er_diagram(collections);
        // Keep the Mermaid text: it is the portable form of the diagram and
        // the layout only keeps geometry, not the model.
        self.mermaid = er_to_mermaid(&er);
        match layout(&er) {
            Ok(scene) => {
                self.scene_w = scene.scene.width;
                self.scene_h = scene.scene.height;
                self.scene = Some(scene);
                self.last_error = None;
            }
            Err(e) => {
                self.scene = None;
                self.last_error = Some(e);
            }
        }
        self.is_loading = false;
    }

    /// Re-parse an existing diagram from Mermaid text. Kept for tests +
    /// future "import Mermaid ER source" use cases. Not on the hot path
    /// (the app uses `generate_from_meta`).
    #[allow(dead_code)]
    pub fn generate_from_mermaid(&mut self, mermaid: &str) {
        match parse_document(mermaid) {
            Ok(Document::Er(er)) => {
                match layout(&er) {
                    Ok(scene) => {
                        self.scene_w = scene.scene.width;
                        self.scene_h = scene.scene.height;
                        self.scene = Some(scene);
                        self.last_error = None;
                    }
                    Err(e) => {
                        self.scene = None;
                        self.last_error = Some(e);
                    }
                }
            }
            Ok(_) => {
                self.scene = None;
                self.last_error = Some("source is not an erDiagram".to_string());
            }
            Err(e) => {
                self.scene = None;
                self.last_error = Some(format!("parse: {e}"));
            }
        }
        self.is_loading = false;
    }

    pub fn scroll_up(&mut self) {
        let p = self.view.px_row();
        self.view.oy = (self.view.oy - p).max(-OVERSCROLL_CELLS * p);
    }

    pub fn scroll_down(&mut self) {
        let p = self.view.px_row();
        self.view.oy = (self.view.oy + p).min(self.scene_h);
    }

    pub fn scroll_left(&mut self) {
        let p = self.view.px_col() * PAN_STEP_COLS;
        self.view.ox = (self.view.ox - p).max(-OVERSCROLL_CELLS * self.view.px_col());
    }

    pub fn scroll_right(&mut self) {
        let p = self.view.px_col() * PAN_STEP_COLS;
        self.view.ox = (self.view.ox + p).min(self.scene_w);
    }

    /// Scroll one viewport's worth (a "page") vertically. Uses the
    /// last-painted canvas height, falling back to a sensible default before
    /// the first draw. Clamped like the single-row scroll.
    pub fn page_down(&mut self) {
        let rows = self.page_rows();
        let p = rows * self.view.px_row();
        self.view.oy = (self.view.oy + p).min(self.scene_h);
    }

    pub fn page_up(&mut self) {
        let rows = self.page_rows();
        let p = rows * self.view.px_row();
        self.view.oy = (self.view.oy - p).max(-OVERSCROLL_CELLS * self.view.px_row());
    }

    /// Rows to skip for a page scroll — the canvas height minus one row of
    /// overlap so the previous screen's last row stays visible for context.
    fn page_rows(&self) -> f64 {
        f64::from(
            self.last_canvas_area
                .map(|r| r.height.saturating_sub(1).max(1))
                .unwrap_or(10),
        )
    }

    /// Write the diagram out as `~/dbx_erd_<schema>.svg` + `.mmd`.
    ///
    /// The SVG comes from flowmaid's own renderer (so it matches the shape on
    /// screen rather than being re-drawn), and the `.mmd` is the portable
    /// source that pastes into any Mermaid-aware tool.
    pub fn export_files(&self) -> Result<String, String> {
        let scene = self
            .scene
            .as_ref()
            .ok_or_else(|| "no diagram to export - press g to generate one first".to_string())?;

        let stem = format!("dbx_erd_{}", self.namespace.0.replace(['/', ' '], "_"));
        let svg =
            crate::export::Exporter::save_to_file(&format!("~/{stem}.svg"), &er::to_svg(scene))
                .map_err(|e| format!("{e:#}"))?;
        let mmd = crate::export::Exporter::save_to_file(&format!("~/{stem}.mmd"), &self.mermaid)
            .map_err(|e| format!("{e:#}"))?;

        Ok(format!("{} + {}", svg.display(), mmd.display()))
    }

    /// Recentre the viewport (offset = 0,0, zoom = 1.0). Bound to `0` like
    /// the spike.
    pub fn reset_view(&mut self) {
        self.view = View::default();
    }

    /// Pan by a delta in terminal cells (mouse drag). Grab-to-pan: dragging
    /// down/right moves the content with the cursor, so both axes subtract.
    /// Clamped like the arrow-key scroll so the diagram can't be flung
    /// entirely off-canvas.
    pub fn pan_by_cells(&mut self, dx: i32, dy: i32) {
        let dcol = f64::from(dx) * self.view.px_col();
        let drow = f64::from(dy) * self.view.px_row();
        self.view.ox -= dcol;
        self.view.oy -= drow;
        self.view.ox = self.view.ox.clamp(-OVERSCROLL_CELLS * self.view.px_col(), self.scene_w);
        self.view.oy = self.view.oy.clamp(-OVERSCROLL_CELLS * self.view.px_row(), self.scene_h);
    }

    /// Zoom in toward the current view origin (clamped to a sane range).
    pub fn zoom_in(&mut self) {
        self.view.zoom = (self.view.zoom * 1.25).min(3.0);
    }

    /// Zoom out (clamped so the diagram doesn't collapse to nothing).
    pub fn zoom_out(&mut self) {
        self.view.zoom = (self.view.zoom / 1.25).max(0.25);
    }

    /// Advance the keyboard selection to the next entity (wraps around).
    pub fn select_next(&mut self) {
        let n = self.scene.as_ref().map(|s| s.scene.nodes.len()).unwrap_or(0);
        if n == 0 {
            return;
        }
        let cur = self.selected_node.unwrap_or(0);
        self.selected_node = Some((cur + 1) % n);
    }

    /// Move the keyboard selection to the previous entity (wraps around).
    pub fn select_prev(&mut self) {
        let n = self.scene.as_ref().map(|s| s.scene.nodes.len()).unwrap_or(0);
        if n == 0 {
            return;
        }
        let cur = self.selected_node.unwrap_or(0);
        self.selected_node = Some((cur + n - 1) % n);
    }

    /// Map a terminal cell (mouse `column` / `row`, absolute coords) to the
    /// node under the cursor, or `None`. Requires a cached scene + a paint
    /// from `render_erd` (which records `last_canvas_area`).
    ///
    /// The transform inverts what `render_erd` does on draw:
    /// - cell → world: `x = ox + rel_x * PX_PER_COL`,
    ///   `y = oy + rel_y * PX_PER_ROW` (flowmaid's y and the screen row both
    ///   grow downward, so there is no vertical flip).
    /// - then `scene.hit_test` with a zero tolerance (node rects are big
    ///   enough to click without needing edge tolerance).
    pub fn node_at_mouse(&self, col: u16, row: u16) -> Option<usize> {
        let area = self.last_canvas_area?;
        let scene = self.scene.as_ref()?;
        if col < area.x
            || col >= area.x + area.width
            || row < area.y
            || row >= area.y + area.height
        {
            return None;
        }
        let rel_x = f64::from(col - area.x);
        let rel_y = f64::from(row - area.y);
        let wx = self.view.ox + rel_x * self.view.px_col();
        // Renderer maps world y → row as (y - oy) / px_row + area.y, so the
        // inverse is world y = oy + rel_y * px_row (no vertical mirror).
        let wy = self.view.oy + rel_y * self.view.px_row();
        match scene.scene.hit_test(wx, wy, 0.0) {
            Some(Hit::Node(i)) => Some(i),
            _ => None,
        }
    }
}

/// Run flowmaid's layout engine defensively. `er::scene` is **infallible**
/// (it panics rather than returning a `Result` on malformed input), which
/// would take down the whole TUI. Our input comes from structured schema
/// metadata so panics are unlikely, but a single layout panic shouldn't end
/// the user's session — catch it and surface it as a renderable error
/// instead.
fn layout(er: &ErDiagram) -> Result<ErScene, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| er::scene(er)))
        .map_err(|_| "diagram layout panicked inside flowmaid".to_string())
}

/// Build a `flowmaid::model::ErDiagram` from driver-level metadata.
///
/// FK relationships are walked and emitted as `Relation`s. We do not
/// introspect the referenced table for unique constraints, so a FK column
/// that is *not* UNIQUE on the child becomes `Card::OneMany` on the parent
/// side and `Card::ZeroMany` on the child side. Cross-namespace FKs are
/// skipped (no entity to anchor to on the canvas).
fn build_er_diagram(collections: &[CollectionMeta]) -> ErDiagram {
    let mut er = ErDiagram::default();

    // Entities + attributes first, so ensure_entity indexes are stable.
    for meta in collections {
        let idx = er.ensure_entity(&meta.reference.name);
        for col in &meta.columns {
            let mut keys = Vec::new();
            if col.is_primary_key {
                keys.push(Key::Pk);
            }
            if col.is_foreign_key {
                keys.push(Key::Fk);
            }
            if col.is_unique {
                keys.push(Key::Uk);
            }
            // Strip "(N)" / "(M,N)" length suffixes from the data type —
            // flowmaid uses the bare type token as a column header.
            let ty = col
                .data_type
                .split('(')
                .next()
                .unwrap_or(&col.data_type)
                .trim()
                .to_string();
            er.entities[idx].attrs.push(Attr {
                ty,
                name: col.name.clone(),
                keys,
                comment: None,
            });
        }
    }

    // Relations. All FKs are treated as identifying (`--`) — the driver's
    // metadata doesn't carry a non-identifying flag, and a TUI ERD
    // doesn't usually need to distinguish them.
    //
    // A FK may reference a table in ANOTHER namespace. Instead of dropping
    // the relationship, we draw it to an "external entity" placeholder whose
    // name is `schema.table` — it renders as a small grey box with no
    // columns, so cross-schema relationships stay visible.
    let ns = collections.first().map(|m| m.reference.namespace.clone());
    for meta in collections {
        for fk in &meta.foreign_keys {
            let ref_ns_is_this = Some(&fk.ref_namespace) == ns.as_ref();
            let parent_name = if ref_ns_is_this {
                fk.ref_table.clone()
            } else {
                format!("{}.{}", fk.ref_namespace.0, fk.ref_table)
            };
            let from = er.ensure_entity(&parent_name);
            let to = er.ensure_entity(&meta.reference.name);
            // Cardinality (crow's foot):
            // - card_from (parent side) is always exactly-one: an FK row
            //   points at a single parent row. (The parent's referenced
            //   column is unique by definition of a valid FK, so there's
            //   nothing to sniff here.)
            // - card_to (child side) is ONE when the FK column itself is
            //   PK/UNIQUE on the child (a 1:1 link — one parent has at most
            //   one child), otherwise ZERO-OR-MANY (one parent has many
            //   children).
            let child_fk_unique = meta
                .columns
                .iter()
                .find(|c| c.name == fk.column)
                .map(|c| c.is_primary_key || c.is_unique)
                .unwrap_or(false);
            let card_to = if child_fk_unique {
                Card::One
            } else {
                Card::ZeroMany
            };
            er.relations.push(Relation {
                from,
                to,
                card_from: Card::One,
                card_to,
                identifying: true,
                label: Some(fk.column.clone()),
            });
        }
    }

    er
}

// -- Painter ----------------------------------------------------------------
//
// The body below is a faithful port of examples/flowmaid_spike.rs (lines
// 218-489) with the standalone-TUI bits (terminal setup, hjkl loop) and
// the `Document` parse step stripped out. The painter takes a precomputed
// `ErScene` and a `View` and writes into the supplied `Frame`.

pub fn render_erd(
    f: &mut Frame,
    area: Rect,
    erd: &mut ErdTab,
    is_focused: bool,
    theme: &Theme,
) {
    let border_style = if is_focused { theme.accent() } else { theme.border() };
    let title = format!(
        " ERD: {} [hjkl/arrows pan, 0 reset] ",
        erd.namespace.0
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .style(theme.base())
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.width < MIN_W || inner.height < MIN_H {
        let msg = format!(
            "terminal too small: {}x{} — need at least {}x{}",
            inner.width, inner.height, MIN_W, MIN_H
        );
        f.render_widget(Paragraph::new(msg).style(theme.error()).centered(), inner);
        return;
    }

    if erd.is_loading {
        f.render_widget(
            Paragraph::new(Span::styled("Generating ERD diagram...", theme.dim()))
                .alignment(Alignment::Center),
            inner,
        );
        return;
    }

    let Some(scene) = &erd.scene else {
        // Show the last error and the Mermaid-source breadcrumb so the
        // user can see what went wrong + the raw input for debugging.
        let body = match &erd.last_error {
            Some(err) => format!("ERD scene failed: {err}"),
            None => "ERD not generated yet — press g on a database node.".to_string(),
        };
        f.render_widget(
            Paragraph::new(Span::styled(body, theme.error()))
                .alignment(Alignment::Center)
                .wrap(ratatui::widgets::Wrap { trim: true }),
            inner,
        );
        return;
    };

    // Reserve the last row for a status bar. Record the canvas rect so a
    // later mouse click can map cell coords back to scene space.
    let canvas_area = Rect {
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let status_area = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        height: 1,
        ..inner
    };
    erd.last_canvas_area = Some(canvas_area);

    let pcol = erd.view.px_col();
    let prow = erd.view.px_row();
    let vw = f64::from(canvas_area.width) * pcol;
    let vh = f64::from(canvas_area.height) * prow;
    let (vx, vy) = (erd.view.ox, erd.view.oy);
    // flowmaid is y-down (SVG); ratatui's canvas is y-up. Affine flip
    // within the current view: f(vy) = vy + vh (top), f(vy + vh) = vy.
    let flip = move |y: f64| 2.0 * vy + vh - y;

    draw_edges(f, scene, canvas_area, vx, vw, vy, vh, flip);

    let buf = f.buffer_mut();
    draw_tables(buf, scene, canvas_area, vx, vy, pcol, prow, erd.selected_node);
    draw_edge_text(buf, scene, canvas_area, vx, vy, pcol, prow);

    let status = format!(
        " hjkl/arrows pan · 0 reset · +/- zoom ({:.2}x) · click node = menu · E export svg/mmd · offset ({:.0}, {:.0})px · scene {}x{} ",
        erd.view.zoom, erd.view.ox, erd.view.oy, erd.scene_w as i64, erd.scene_h as i64
    );
    f.render_widget(Paragraph::new(status).style(theme.dim()), status_area);
}

/// Relationship curves + crow's feet on a braille canvas (2x4 subcell
/// resolution — the only sane way to draw smooth curves in a terminal).
#[allow(clippy::too_many_arguments)]
fn draw_edges(
    frame: &mut Frame,
    scene: &ErScene,
    area: Rect,
    vx: f64,
    vw: f64,
    vy: f64,
    vh: f64,
    flip: impl Fn(f64) -> f64 + Copy,
) {
    let canvas = Canvas::default()
        .x_bounds([vx, vx + vw])
        .y_bounds([vy, vy + vh])
        .marker(Marker::Braille)
        .paint(|ctx| {
            for (j, edge) in scene.scene.edges.iter().enumerate() {
                let color = Color::Gray;
                // Prefer routed waypoints (long edges threaded through
                // per-layer channels); fall back to the single cubic bezier.
                let points: Vec<(f64, f64)> = if edge.waypoints.len() >= 2 {
                    edge.waypoints.clone()
                } else {
                    sample_bezier(&edge.bezier, 32)
                };
                // Non-identifying relationships (`..`) come through as
                // EdgeKind::Dotted — skip every other segment for a dash.
                let dashed = matches!(edge.kind, EdgeKind::Dotted);
                for (i, seg) in points.windows(2).enumerate() {
                    if dashed && i % 2 == 1 {
                        continue;
                    }
                    let [a, b] = [seg[0], seg[1]];
                    ctx.draw(&CanvasLine::new(a.0, flip(a.1), b.0, flip(b.1), color));
                }

                // Crow's feet: flowmaid hands us the exact glyph geometry
                // it puts in the SVG — segments plus an optional circle.
                let (card_from, card_to) = scene.cards[j];
                for (g_endpoint, g_control, card) in [
                    (edge.bezier[0], edge.bezier[1], card_from),
                    (edge.bezier[3], edge.bezier[2], card_to),
                ] {
                    let glyph = er::glyph(g_endpoint, g_control, card);
                    for [a, b] in &glyph.segments {
                        ctx.draw(&CanvasLine::new(a.0, flip(a.1), b.0, flip(b.1), color));
                    }
                    if let Some(((cx, cy), r)) = glyph.circle {
                        ctx.draw(&Circle::new(cx, flip(cy), r, color));
                    }
                }
            }
        });
    frame.render_widget(canvas, area);
}

/// Entity tables: rounded border + title in the top border, one attribute
/// per inner line ("type name" left, key tags right-aligned).
#[allow(clippy::too_many_arguments)]
fn draw_tables(
    buf: &mut Buffer,
    scene: &ErScene,
    area: Rect,
    vx: f64,
    vy: f64,
    pcol: f64,
    prow: f64,
    selected: Option<usize>,
) {
    for (i, node) in scene.scene.nodes.iter().enumerate() {
        let table = &scene.tables[i];
        let x0 = ((node.x - node.w / 2.0 - vx) / pcol).round() as i32 + i32::from(area.x);
        let y0 = ((node.y - node.h / 2.0 - vy) / prow).round() as i32 + i32::from(area.y);
        let w = (node.w / pcol).round().max(6.0) as i32;
        let h = (node.h / prow).round().max(3.0) as i32;
        let Some(rect) = clip(x0, y0, w, h, area) else {
            continue;
        };
        if rect.width < 4 || rect.height < 3 {
            continue;
        }

        // Cross-namespace placeholder entities are named `schema.table` —
        // render them greyed-out so they read as "external, not in this
        // schema" at a glance.
        let is_external = table.name.contains('.');
        let accent = if is_external {
            Color::DarkGray
        } else {
            ACCENTS[i % ACCENTS.len()]
        };
        // Selected node: brighter, bold border so it reads as the active
        // target (e.g. for Enter→DDL / click→DDL).
        let is_selected = selected == Some(i);
        let border_color = if is_selected { Color::Yellow } else { accent };
        let border_style = if is_selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(accent)
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(border_style)
            .title_top(
                Line::from(table.name.clone())
                    .centered()
                    .style(Style::default().fg(border_color).add_modifier(Modifier::BOLD)),
            );
        let inner = block.inner(rect);
        block.render(rect, buf);

        // Attribute rows: flowmaid's layout gives one terminal line per
        // row at our scale; clip with a "+N more" indicator if the box
        // shrank below the row count.
        let max_rows = inner.height as usize;
        let visible = table.rows.len().min(max_rows);
        for (r, row) in table.rows.iter().take(visible).enumerate() {
            // On the last visible line, prefer the overflow indicator.
            if r == visible - 1 && table.rows.len() > max_rows {
                let more = format!("+{} more", table.rows.len() - max_rows + 1);
                buf.set_stringn(
                    inner.x,
                    inner.y + r as u16,
                    more,
                    inner.width as usize,
                    Style::default().fg(Color::DarkGray),
                );
                break;
            }
            let left = format!("{} {}", row.ty, row.name);
            let y = inner.y + r as u16;
            buf.set_stringn(
                inner.x,
                y,
                left,
                inner.width as usize,
                Style::default().fg(Color::White),
            );
            if !row.keys.is_empty() {
                let kw = row.keys.len() as u16;
                if inner.width > kw {
                    buf.set_stringn(
                        inner.x + inner.width - kw,
                        y,
                        row.keys.clone(),
                        kw as usize,
                        Style::default().fg(Color::Yellow),
                    );
                }
            }
        }
    }
}

/// Relationship labels (at flowmaid's label-box centre) and crow's-foot
/// cardinalities as text just off each edge endpoint — terminal users
/// read text faster than mini-glyphs.
fn draw_edge_text(
    buf: &mut Buffer,
    scene: &ErScene,
    area: Rect,
    vx: f64,
    vy: f64,
    pcol: f64,
    prow: f64,
) {
    let label_style = Style::default().fg(Color::Cyan);
    let card_style = Style::default().fg(Color::DarkGray);

    for (j, edge) in scene.scene.edges.iter().enumerate() {
        if let Some((text, (cx, cy), _w)) = &edge.label {
            let text_w = unicode_width::UnicodeWidthStr::width(text.as_str()) as f64;
            let col = ((cx - vx) / pcol - text_w / 2.0).round() as i32;
            let row = ((cy - vy) / prow).round() as i32;
            put(buf, area, col, row, text, label_style);
        }

        let (card_from, card_to) = scene.cards[j];
        for (endpoint, control, card) in [
            (edge.bezier[0], edge.bezier[1], card_from),
            (edge.bezier[3], edge.bezier[2], card_to),
        ] {
            // Step off the entity border along the edge's inward tangent.
            let (dx, dy) = (control.0 - endpoint.0, control.1 - endpoint.1);
            let len = (dx * dx + dy * dy).sqrt().max(1e-6);
            let px = endpoint.0 + dx / len * 22.0;
            let py = endpoint.1 + dy / len * 22.0;
            let text = card_text(card);
            let col = ((px - vx) / pcol - text.len() as f64 / 2.0).round() as i32;
            let row = ((py - vy) / prow).round() as i32;
            put(buf, area, col, row, text, card_style);
        }
    }
}

fn card_text(card: Card) -> &'static str {
    match card {
        Card::One => "1",
        Card::ZeroOne => "0..1",
        Card::ZeroMany => "0..N",
        Card::OneMany => "1..N",
    }
}

/// Write a string centred at cell (col, row) if it lands inside `area`.
fn put(buf: &mut Buffer, area: Rect, col: i32, row: i32, text: &str, style: Style) {
    if row < i32::from(area.y) || row >= i32::from(area.y + area.height) {
        return;
    }
    if col < i32::from(area.x) || col >= i32::from(area.x + area.width) {
        return;
    }
    let max = i32::from(area.x + area.width) - col;
    buf.set_stringn(col as u16, row as u16, text, max.max(0) as usize, style);
}

/// Intersect a signed rect with the target area; `None` if fully outside.
fn clip(x: i32, y: i32, w: i32, h: i32, area: Rect) -> Option<Rect> {
    let left = x.max(i32::from(area.x));
    let top = y.max(i32::from(area.y));
    let right = (x + w).min(i32::from(area.x + area.width));
    let bottom = (y + h).min(i32::from(area.y + area.height));
    if right <= left || bottom <= top {
        return None;
    }
    Some(Rect::new(
        left as u16,
        top as u16,
        (right - left) as u16,
        (bottom - top) as u16,
    ))
}

/// Sample a cubic bezier (start, c1, c2, end) into `n` polyline points.
fn sample_bezier(b: &[(f64, f64); 4], n: usize) -> Vec<(f64, f64)> {
    (0..=n)
        .map(|i| {
            let t = i as f64 / n as f64;
            let mt = 1.0 - t;
            let (mut x, mut y) = (0.0, 0.0);
            for (k, p) in b.iter().enumerate() {
                let coeff = match k {
                    0 => mt * mt * mt,
                    1 => 3.0 * mt * mt * t,
                    2 => 3.0 * mt * t * t,
                    _ => t * t * t,
                };
                x += coeff * p.0;
                y += coeff * p.1;
            }
            (x, y)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{CollectionRef, ColumnMeta, ForeignKeyMeta, IndexMeta};

    fn table(name: &str, columns: Vec<ColumnMeta>, fks: Vec<ForeignKeyMeta>) -> CollectionMeta {
        CollectionMeta {
            reference: CollectionRef {
                namespace: Namespace("shop".to_string()),
                name: name.to_string(),
            },
            columns,
            indexes: Vec::<IndexMeta>::new(),
            foreign_keys: fks,
        }
    }

    fn col(name: &str, ty: &str, is_pk: bool, is_fk: bool, is_unique: bool) -> ColumnMeta {
        ColumnMeta {
            name: name.to_string(),
            data_type: ty.to_string(),
            is_nullable: !is_pk,
            is_primary_key: is_pk,
            is_unique,
            is_foreign_key: is_fk,
            extra: None,
        }
    }

    #[test]
    fn test_er_diagram_builds_from_meta() {
        let users = table(
            "users",
            vec![col("id", "int", true, false, true), col("name", "string", false, false, false)],
            vec![],
        );
        let orders = table(
            "orders",
            vec![
                col("id", "int", true, false, true),
                col("user_id", "int", false, true, false),
            ],
            vec![ForeignKeyMeta {
                name: "orders_user_id_fkey".to_string(),
                column: "user_id".to_string(),
                ref_namespace: Namespace("shop".to_string()),
                ref_table: "users".to_string(),
                ref_column: "id".to_string(),
            }],
        );
        let er = build_er_diagram(&[users, orders]);
        assert_eq!(er.entities.len(), 2);
        assert_eq!(er.entities[0].name, "users");
        assert_eq!(er.entities[1].name, "orders");
        assert_eq!(er.entities[1].attrs.len(), 2);
        assert_eq!(er.relations.len(), 1);
        let rel = &er.relations[0];
        // Parent side is always exactly-one (an FK row → one parent row).
        assert_eq!(rel.card_from, Card::One);
        // user_id is NOT unique on the child, and users.id IS unique (PK) —
        // but the child side only collapses to ONE when the FK column itself
        // is unique; otherwise it stays zero-or-many (one user has many orders).
        assert_eq!(rel.card_to, Card::ZeroMany);
        assert_eq!(rel.label.as_deref(), Some("user_id"));
    }

    #[test]
    fn test_er_cardinality_one_when_child_fk_unique() {
        // profile.user_id is UNIQUE → each user has at most one profile.
        let users = table(
            "users",
            vec![col("id", "int", true, false, true)],
            vec![],
        );
        let profiles = table(
            "profiles",
            vec![
                col("id", "int", true, false, true),
                col("user_id", "int", false, true, true), // unique FK
            ],
            vec![ForeignKeyMeta {
                name: "profiles_user_id_fkey".to_string(),
                column: "user_id".to_string(),
                ref_namespace: Namespace("shop".to_string()),
                ref_table: "users".to_string(),
                ref_column: "id".to_string(),
            }],
        );
        let er = build_er_diagram(&[users, profiles]);
        assert_eq!(er.relations[0].card_to, Card::One);
        assert_eq!(er.relations[0].card_from, Card::One);
    }

    #[test]
    fn test_er_cardinality_many_when_parent_unique_but_child_fk_not() {
        // A non-unique child FK stays zero-or-many even though the parent's
        // referenced column is a PK (which is always unique) — the parent
        // side being unique says nothing about how many children fit.
        let codes = table(
            "codes",
            vec![col("code", "varchar", false, false, true)], // unique, not PK
            vec![],
        );
        let items = table(
            "items",
            vec![
                col("id", "int", true, false, true),
                col("code", "varchar", false, true, false), // non-unique FK
            ],
            vec![ForeignKeyMeta {
                name: "items_code_fkey".to_string(),
                column: "code".to_string(),
                ref_namespace: Namespace("shop".to_string()),
                ref_table: "codes".to_string(),
                ref_column: "code".to_string(),
            }],
        );
        let er = build_er_diagram(&[codes, items]);
        assert_eq!(er.relations[0].card_to, Card::ZeroMany);
        assert_eq!(er.relations[0].card_from, Card::One);
    }

    #[test]
    fn test_er_scene_layouts_built_diagram() {
        let users = table(
            "users",
            vec![col("id", "int", true, false, true)],
            vec![],
        );
        let orders = table(
            "orders",
            vec![
                col("id", "int", true, false, true),
                col("user_id", "int", false, true, false),
            ],
            vec![ForeignKeyMeta {
                name: "orders_user_id_fkey".to_string(),
                column: "user_id".to_string(),
                ref_namespace: Namespace("shop".to_string()),
                ref_table: "users".to_string(),
                ref_column: "id".to_string(),
            }],
        );
        let er = build_er_diagram(&[users, orders]);
        let scene = er::scene(&er);
        assert_eq!(scene.scene.nodes.len(), 2);
        assert_eq!(scene.scene.edges.len(), 1);
        assert_eq!(scene.tables.len(), 2);
        assert_eq!(scene.cards.len(), 1);
    }

    #[test]
    fn test_er_diagram_external_entity_for_cross_namespace_fk() {
        // A FK to another namespace must NOT be dropped — it targets an
        // "external entity" placeholder named `schema.table`.
        let users = table("users", vec![col("id", "int", true, false, true)], vec![]);
        let other = table(
            "other",
            vec![col("id", "int", true, false, true)],
            vec![ForeignKeyMeta {
                name: "fk".to_string(),
                column: "id".to_string(),
                ref_namespace: Namespace("other_ns".to_string()),
                ref_table: "users".to_string(),
                ref_column: "id".to_string(),
            }],
        );
        let er = build_er_diagram(&[users, other]);
        assert_eq!(er.relations.len(), 1, "cross-namespace FK must be kept");
        // The parent entity is the external placeholder.
        let rel = &er.relations[0];
        assert_eq!(er.entities[rel.from].name, "other_ns.users");
        assert_eq!(er.entities[rel.to].name, "other");
    }

    #[test]
    fn test_er_to_mermaid_round_trips_through_the_parser() {
        let users = table(
            "users",
            vec![col("id", "int", true, false, true)],
            vec![],
        );
        let orders = table(
            "orders",
            vec![col("user_id", "character varying", false, false, false)],
            vec![ForeignKeyMeta {
                name: "fk".to_string(),
                column: "user_id".to_string(),
                ref_namespace: Namespace("shop".to_string()),
                ref_table: "users".to_string(),
                ref_column: "id".to_string(),
            }],
        );
        let er = build_er_diagram(&[users, orders]);
        let src = er_to_mermaid(&er);

        assert!(src.starts_with("erDiagram"), "got {src}");
        assert!(src.contains("users {"), "entities missing: {src}");
        assert!(src.contains("PK"), "key markers missing: {src}");
        // Mermaid types cannot contain spaces.
        assert!(!src.contains("character varying"), "unescaped type: {src}");
        assert!(src.contains("character_varying"), "type not escaped: {src}");

        // The real contract: flowmaid must be able to parse what we emit.
        let mut tab = ErdTab::new(Namespace("shop".to_string()));
        tab.generate_from_mermaid(&src);
        assert!(
            tab.last_error.is_none(),
            "emitted source failed to parse: {:?}",
            tab.last_error
        );
        assert!(tab.scene.is_some());
    }

    #[test]
    fn test_node_at_mouse_hits_entity_center() {
        let users = table("users", vec![col("id", "int", true, false, true)], vec![]);
        let er = build_er_diagram(&[users]);
        let scene = er::scene(&er);
        let (node_x, node_y) = (scene.scene.nodes[0].x, scene.scene.nodes[0].y);

        let mut tab = ErdTab::new(Namespace("shop".to_string()));
        tab.scene = Some(scene);
        tab.view = View::default();
        // A plausible canvas: top-left of the terminal, 80x30 cells.
        tab.last_canvas_area = Some(Rect::new(0, 0, 80, 30));

        // Invert node_at_mouse's mapping: cell = (world - view) / PX_PER_*.
        // World y and screen row grow in the same direction (no y-flip).
        let col = (node_x / PX_PER_COL) as u16;
        let row = (node_y / PX_PER_ROW) as u16;
        assert_eq!(tab.node_at_mouse(col, row), Some(0));

        // A click far outside the canvas hits nothing.
        assert_eq!(tab.node_at_mouse(200, 200), None);
    }
}
