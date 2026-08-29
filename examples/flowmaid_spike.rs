//! M0 spike (roadmap task 0.8): prove flowmaid's ER scene can be painted into
//! a ratatui canvas with unicode box/line drawing — before M3 depends on it.
//!
//! Contract (spike agent):
//! - Parse a small hardcoded `erDiagram` (3-4 tables with 1:N and a junction
//!   M:N relationship).
//! - Obtain layout geometry from flowmaid (scene API — positions + edges).
//! - Paint it into a ratatui buffer: nodes as rounded boxes with the entity
//!   name, edges as lines with crow's-foot-ish markers where feasible.
//! - Runs as a standalone TUI (`cargo run --example flowmaid_spike`): q quits,
//!   hjkl/arrows pan. If flowmaid's API turns out not to fit, document exactly
//!   what is missing in a comment at the top of this file — that is a
//!   successful spike outcome too.
//!
//! ---------------------------------------------------------------------------
//! SPIKE FINDINGS (flowmaid 0.25.0, ratatui 0.30.2, crossterm 0.29.0)
//!
//! VERDICT: flowmaid's scene API SUFFICES for a terminal ER painter. Nothing
//! was blocking; every piece of geometry the SVG exporter uses is reachable
//! from public, renderer-agnostic structs.
//!
//! What worked (exact API used here):
//! - `flowmaid::parser::parse_document(&str) -> Result<Document, ParseError>`
//!   with `Document::Er(ErDiagram)` — parses `erDiagram` source, incl.
//!   attribute blocks (`int id PK`), relationship labels (`: places`),
//!   identifying `--` vs non-identifying `..`, all four cardinalities.
//! - `flowmaid::er::scene(&ErDiagram) -> ErScene` — full automatic layout,
//!   no SVG involved. `ErScene { scene, tables, cards }`:
//!   - `scene.nodes: Vec<SceneNode>` — centre `(x, y)` + `(w, h)` per entity,
//!     index-parallel with `tables` (asserted inside flowmaid).
//!   - `scene.edges: Vec<SceneEdge>` — cubic `bezier: [(f64,f64); 4]`
//!     (start, c1, c2, end, clipped to entity borders), optional routed
//!     `waypoints: Vec<(f64, f64)>`, `kind: EdgeKind` (Dotted =
//!     non-identifying), `label: Option<(String, centre, width)>`.
//!   - `tables: Vec<ErTable>` — pre-formatted rows (`ErRow { ty, name, keys }`)
//!     and `ty_col_w` for column alignment.
//!   - `cards: Vec<(Card, Card)>` — crow's-foot cardinality per edge end,
//!     index-parallel with `scene.edges`.
//! - `flowmaid::er::glyph(endpoint, adjacent_control, Card) -> Glyph` —
//!   crow's foot geometry as plain segments + optional circle. Painted
//!   straight onto the braille canvas; no glyph math of our own.
//! - `er::route(&ErDiagram, &[(f64, f64)])` (not exercised here) re-routes
//!   edges for dragged entity positions — exactly what an interactive
//!   drag-to-rearrange mode in M3+ would need.
//!
//! What is missing / awkward:
//! - Coordinates are SVG pixels, y-down, font-metric-based (~7px/char,
//!   `er::HEADER_H=30`, `er::ROW_H=22`). A terminal painter must pick a
//!   px→cell scale (here 7px = 1 col, 22px = 1 row so one attribute row =
//!   one terminal line). Not a gap — just a transform — but box interiors
//!   get tight: a table with many attributes needs the vertical scale kept
//!   at ~22px/row or rows get clipped (we show "+N more").
//! - Edge curves are beziers/waypoints in continuous space; unicode
//!   box-drawing chars (─│╭╮╰╯) cannot approximate them cell-by-cell without
//!   ugly stair-stepping. Braille canvas (2x4 subcell resolution) solves it
//!   and looks good, at the cost of needing braille-capable fonts.
//! - `SceneEdge` has no per-edge index back to `Relation` beyond ordering;
//!   consumers must rely on the documented index-parallel invariant
//!   (`edges[j]` == `relations[j]`, guarded by asserts in flowmaid).
//! - No hit-testing helpers specific to ER (Scene::hit_test exists for
//!   generic scenes — fine for later).
//!
//! Effort estimate for the real M3 painter: ~1-2 days. The spike's render
//! path carries over almost verbatim; remaining work is wiring live schema
//! metadata into erDiagram text (or building `ErDiagram` directly), zoom,
//! selection/highlight, and theme integration.

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use flowmaid::er::{self, ErScene};
use flowmaid::model::{Card, Document, EdgeKind};
use flowmaid::parser::parse_document;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::Line;
use ratatui::widgets::canvas::{Canvas, Circle, Line as CanvasLine};
use ratatui::widgets::{Block, BorderType, Paragraph, Widget};
use std::io;
use std::time::Duration;

/// Smallest terminal the painter accepts (cols x rows).
const MIN_W: u16 = 60;
const MIN_H: u16 = 15;

/// px→cell scale. Horizontal matches flowmaid's ~7px/char text metrics so
/// table text survives the transform; vertical keeps one attribute row
/// (`er::ROW_H` = 22px) on one terminal line.
const PX_PER_COL: f64 = 7.0;
const PX_PER_ROW: f64 = 22.0;

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

/// The hardcoded diagram: users 1:N orders 1:N order_items, plus a
/// posts <-> tags M:N realised through the posts_tags junction table.
const DIAGRAM: &str = "\
erDiagram
    users ||--o{ orders : places
    orders ||--|{ order_items : contains
    posts ||--o{ posts_tags : tagged
    tags ||--o{ posts_tags : applied
    users {
        int id PK
        string email UK
        string name
        timestamp created_at
    }
    orders {
        int id PK
        int user_id FK
        string status
        decimal total
    }
    order_items {
        int id PK
        int order_id FK
        int product_id
        int quantity
        decimal price
    }
    posts {
        int id PK
        string title
        text body
    }
    tags {
        int id PK
        string slug UK
    }
    posts_tags {
        int post_id PK, FK
        int tag_id PK, FK
    }
";

/// Viewport: world pixel coordinate shown at the top-left of the canvas.
#[derive(Default, Clone, Copy)]
struct View {
    ox: f64,
    oy: f64,
}

fn main() -> io::Result<()> {
    // Parse + lay out before touching the terminal, so a bad diagram fails
    // on the normal screen with a readable error.
    let document = match parse_document(DIAGRAM) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("flowmaid failed to parse the hardcoded diagram: {e}");
            std::process::exit(1);
        }
    };
    let Document::Er(diagram) = document else {
        eprintln!("flowmaid parsed the input as a non-ER document");
        std::process::exit(1);
    };
    let scene = er::scene(&diagram);

    // Restore the terminal even on panic, so a crash can't strand the user
    // in raw mode / the alternate screen.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        original_hook(info);
    }));

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &scene);
    ratatui::restore();
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal, scene: &ErScene) -> io::Result<()> {
    let mut view = View::default();
    loop {
        terminal.draw(|frame| draw(frame, scene, &view))?;

        // Poll with a timeout so resizes repaint even without keypresses.
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(());
            }
            KeyCode::Char('h') | KeyCode::Left => view.ox -= PX_PER_COL * 4.0,
            KeyCode::Char('l') | KeyCode::Right => view.ox += PX_PER_COL * 4.0,
            KeyCode::Char('k') | KeyCode::Up => view.oy -= PX_PER_ROW,
            KeyCode::Char('j') | KeyCode::Down => view.oy += PX_PER_ROW,
            KeyCode::Char('0') => view = View::default(),
            _ => continue,
        }
        // Allow a little overscroll past the origin for edge breathing room.
        view.ox = view.ox.max(-2.0 * PX_PER_COL);
        view.oy = view.oy.max(-2.0 * PX_PER_ROW);
    }
}

fn draw(frame: &mut ratatui::Frame, scene: &ErScene, view: &View) {
    let area = frame.area();
    if area.width < MIN_W || area.height < MIN_H {
        let msg = format!(
            "terminal too small: {}x{} — need at least {}x{}",
            area.width, area.height, MIN_W, MIN_H
        );
        frame.render_widget(
            Paragraph::new(msg)
                .style(Style::default().fg(Color::Red))
                .centered(),
            area,
        );
        return;
    }

    // Bottom row is the status bar; everything above is the diagram canvas.
    let canvas_area = Rect {
        height: area.height - 1,
        ..area
    };
    let status_area = Rect {
        y: area.y + area.height - 1,
        height: 1,
        ..area
    };

    let vw = f64::from(canvas_area.width) * PX_PER_COL;
    let vh = f64::from(canvas_area.height) * PX_PER_ROW;
    let (vx, vy) = (view.ox, view.oy);
    // flowmaid is y-down (SVG); ratatui's canvas is y-up. Affine flip
    // within the current view: f(vy) = vy + vh (top), f(vy + vh) = vy.
    let flip = move |y: f64| 2.0 * vy + vh - y;

    draw_edges(frame, scene, canvas_area, vx, vw, vy, vh, flip);

    let buf = frame.buffer_mut();
    draw_tables(buf, scene, canvas_area, vx, vy);
    draw_edge_text(buf, scene, canvas_area, vx, vy);

    let status = format!(
        " q quit · hjkl/arrows pan · 0 reset · offset ({:.0}, {:.0})px ",
        view.ox, view.oy
    );
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(Color::DarkGray)),
        status_area,
    );
}

/// Relationship curves + crow's feet on a braille canvas (2x4 subcell
/// resolution — the only sane way to draw smooth curves in a terminal).
#[allow(clippy::too_many_arguments)]
fn draw_edges(
    frame: &mut ratatui::Frame,
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

                // Crow's feet: flowmaid hands us the exact glyph geometry it
                // puts in the SVG — segments plus an optional hollow circle.
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
fn draw_tables(buf: &mut Buffer, scene: &ErScene, area: Rect, vx: f64, vy: f64) {
    for (i, node) in scene.scene.nodes.iter().enumerate() {
        let table = &scene.tables[i];
        let x0 = ((node.x - node.w / 2.0 - vx) / PX_PER_COL).round() as i32 + i32::from(area.x);
        let y0 = ((node.y - node.h / 2.0 - vy) / PX_PER_ROW).round() as i32 + i32::from(area.y);
        let w = (node.w / PX_PER_COL).round().max(6.0) as i32;
        let h = (node.h / PX_PER_ROW).round().max(3.0) as i32;
        let Some(rect) = clip(x0, y0, w, h, area) else {
            continue;
        };
        if rect.width < 4 || rect.height < 3 {
            continue;
        }

        let accent = ACCENTS[i % ACCENTS.len()];
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(accent))
            .title_top(
                Line::from(table.name.clone())
                    .centered()
                    .style(Style::default().fg(accent).add_modifier(Modifier::BOLD)),
            );
        let inner = block.inner(rect);
        block.render(rect, buf);

        // Attribute rows: flowmaid's layout gives one terminal line per row
        // at our scale; clip with a "+N more" indicator if the box shrank.
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
/// cardinalities as text ("1", "0..1", "0..N", "1..N") just off each edge
/// endpoint — terminal users read text faster than mini-glyphs.
fn draw_edge_text(buf: &mut Buffer, scene: &ErScene, area: Rect, vx: f64, vy: f64) {
    let label_style = Style::default().fg(Color::Cyan).bg(Color::Black);
    let card_style = Style::default().fg(Color::DarkGray);

    for (j, edge) in scene.scene.edges.iter().enumerate() {
        if let Some((text, (cx, cy), _w)) = &edge.label {
            let col = ((cx - vx) / PX_PER_COL - text.len() as f64 / 2.0).round() as i32;
            let row = ((cy - vy) / PX_PER_ROW).round() as i32;
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
            let col = ((px - vx) / PX_PER_COL - text.len() as f64 / 2.0).round() as i32;
            let row = ((py - vy) / PX_PER_ROW).round() as i32;
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
