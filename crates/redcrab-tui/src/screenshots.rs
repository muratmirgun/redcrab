//! The README's screenshots of the terminal app, as SVG: the screen drawn
//! into a test terminal, each cell in its colors, in a window like a
//! terminal's. Lines and blocks (borders, charts, bars) are drawn as shapes,
//! so that they meet whatever font the viewer has.
//!
//! scripts/screenshots.sh runs it against its demo server:
//!
//!     REDCRAB_SCREENSHOTS=docs/images REDCRAB_SCREENSHOTS_URL=redis://... \
//!       cargo test -p redcrab-tui screenshots -- --ignored

use std::fmt::Write as _;

use crossterm::event::KeyCode;
use ratatui::{
    buffer::{Buffer, Cell},
    style::{Color, Modifier},
};
use redcrab_core::{ConnectionConfig, Environment};

use crate::{
    Start,
    app::{App, Screen},
    theme,
};

const COLUMNS: u16 = 124;
const ROWS: u16 = 34;
/// A cell, in pixels.
const CELL_W: f32 = 8.4;
const CELL_H: f32 = 18.;
const FONT_SIZE: f32 = 14.;
/// Around the screen: the title bar above, a margin on the other sides.
const BAR: f32 = 32.;
const MARGIN: f32 = 12.;

#[tokio::test]
#[ignore = "renders the README's screenshots; see scripts/screenshots.sh"]
async fn screenshots() {
    let (Ok(out), Ok(url)) = (
        std::env::var("REDCRAB_SCREENSHOTS"),
        std::env::var("REDCRAB_SCREENSHOTS_URL"),
    ) else {
        panic!("set REDCRAB_SCREENSHOTS (a folder) and REDCRAB_SCREENSHOTS_URL");
    };
    let mut config = ConnectionConfig::parse_url(&url).expect("a Redis URL");
    config.name = "shop".into();
    config.environment = Environment::Production;
    let mut app = App::new(Start::Url(Box::new(config)));
    app.until("the workspace", |app| {
        matches!(app.screen, Screen::Workspace(_))
    })
    .await;
    let shown =
        |text: &'static str| move |app: &mut App| app.screen_text(COLUMNS, ROWS).contains(text);

    // The keys: a user's hash, and the console under it.
    if let Screen::Workspace(workspace) = &mut app.screen {
        workspace.open_key(b"user:1042".to_vec().into(), &mut app.ctx);
    }
    app.until("the hash", shown("Ada Lovelace")).await;
    app.press(KeyCode::Char(':'));
    app.type_text("ZREVRANGE leaderboard:weekly 0 4 WITHSCORES");
    app.press(KeyCode::Enter);
    app.until("the reply", shown("10)")).await;
    save(&mut app, &out, "tui-keys");

    app.press(KeyCode::Esc);

    // The memory analysis.
    app.press(KeyCode::Char('3'));
    app.press(KeyCode::Char('a'));
    app.until("the analysis", shown("Measured")).await;
    save(&mut app, &out, "tui-memory");
}

fn save(app: &mut App, folder: &str, name: &str) {
    let path = std::path::Path::new(folder).join(format!("{name}.svg"));
    std::fs::write(&path, svg(&app.render(COLUMNS, ROWS), "redcrab-tui")).expect("the SVG saved");
    println!("{}", path.display());
}

/// `buffer` as an SVG picture of a terminal window titled `title`.
fn svg(buffer: &Buffer, title: &str) -> String {
    let (columns, rows) = (buffer.area.width, buffer.area.height);
    let width = n(MARGIN * 2. + f32::from(columns) * CELL_W);
    let height = n(BAR + MARGIN + f32::from(rows) * CELL_H);
    let mut out = String::new();
    let _ = write!(
        out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" font-family="ui-monospace, SFMono-Regular, Menlo, Consolas, 'DejaVu Sans Mono', monospace" font-size="{FONT_SIZE}">"#
    );
    let background = hex(theme::BACKGROUND, theme::BACKGROUND);
    let _ = write!(
        out,
        r##"<rect width="{width}" height="{height}" rx="10" fill="{background}" stroke="#3a3a42"/>"##
    );
    for (ix, color) in ["#ff5f57", "#febc2e", "#28c840"].into_iter().enumerate() {
        let _ = write!(
            out,
            r#"<circle cx="{}" cy="{}" r="6" fill="{color}"/>"#,
            n(18. + 20. * ix as f32),
            n(BAR / 2.)
        );
    }
    let _ = write!(
        out,
        r##"<text x="{}" y="{}" text-anchor="middle" fill="#8c8c96" font-size="13">{}</text>"##,
        n((MARGIN * 2. + f32::from(columns) * CELL_W) / 2.),
        n(BAR / 2. + 4.5),
        escape(title)
    );

    for y in 0..rows {
        let top = BAR + f32::from(y) * CELL_H;
        let row = n(top);
        // Backgrounds, a run of one color at a time.
        let mut x = 0;
        while x < columns {
            let color = buffer[(x, y)].bg;
            let start = x;
            while x < columns && buffer[(x, y)].bg == color {
                x += 1;
            }
            if !matches!(color, Color::Reset) && color != theme::BACKGROUND {
                let _ = write!(
                    out,
                    r#"<rect x="{}" y="{row}" width="{}" height="{}" fill="{}"/>"#,
                    n(left(start)),
                    n(f32::from(x - start) * CELL_W),
                    n(CELL_H),
                    hex(color, theme::BACKGROUND)
                );
            }
        }
        // Text, a run of one style at a time; lines and blocks as shapes.
        let mut x = 0;
        while x < columns {
            let cell = &buffer[(x, y)];
            let symbol = cell.symbol();
            if let Some(shape) = shape(symbol, left(x), top, &fg(cell)) {
                out.push_str(&shape);
                x += 1;
                continue;
            }
            if symbol.trim().is_empty() {
                x += 1;
                continue;
            }
            let start = x;
            let mut text = String::new();
            let mut cells: u16 = 0;
            while x < columns {
                let next = &buffer[(x, y)];
                let symbol = next.symbol();
                if symbol.trim().is_empty()
                    || shape(symbol, 0., 0., "").is_some()
                    || !same_style(next, cell)
                {
                    break;
                }
                text.push_str(symbol);
                let wide = symbol.chars().any(|c| c as u32 > 0x1F000);
                cells += if wide { 2 } else { 1 };
                x += if wide { 2 } else { 1 };
                if wide {
                    break;
                }
            }
            let weight = if cell.modifier.contains(Modifier::BOLD) {
                r#" font-weight="bold""#
            } else {
                ""
            };
            let fit = if text.chars().any(|c| c as u32 > 0x1F000) {
                String::new()
            } else {
                format!(
                    r#" textLength="{}" lengthAdjust="spacingAndGlyphs""#,
                    n(f32::from(cells) * CELL_W)
                )
            };
            let _ = write!(
                out,
                r#"<text x="{}" y="{}" fill="{}"{weight}{fit}>{}</text>"#,
                n(left(start)),
                n(top + CELL_H * 0.72),
                fg(cell),
                escape(&text)
            );
            if cell.modifier.contains(Modifier::UNDERLINED) {
                let color = match cell.underline_color {
                    Color::Reset => fg(cell),
                    color => hex(color, theme::TEXT),
                };
                let _ = write!(
                    out,
                    r#"<rect x="{}" y="{}" width="{}" height="1" fill="{color}"/>"#,
                    n(left(start)),
                    n(top + CELL_H - 2.),
                    n(f32::from(cells) * CELL_W)
                );
            }
        }
    }
    out.push_str("</svg>\n");
    out
}

/// A length in pixels, to the hundredth.
fn n(v: f32) -> String {
    let text = format!("{v:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

fn left(x: u16) -> f32 {
    MARGIN + f32::from(x) * CELL_W
}

fn same_style(a: &Cell, b: &Cell) -> bool {
    a.fg == b.fg && a.modifier == b.modifier && a.underline_color == b.underline_color
}

fn fg(cell: &Cell) -> String {
    hex(cell.fg, theme::TEXT)
}

fn hex(color: Color, default: Color) -> String {
    let (r, g, b) = match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Reset => return hex(default, theme::TEXT),
        Color::Black => (0, 0, 0),
        Color::White | Color::Gray => (228, 228, 231),
        Color::DarkGray => (140, 140, 150),
        Color::Red | Color::LightRed => (239, 68, 68),
        Color::Green | Color::LightGreen => (74, 222, 128),
        Color::Yellow | Color::LightYellow => (250, 204, 21),
        Color::Blue | Color::LightBlue => (96, 165, 250),
        Color::Magenta | Color::LightMagenta => (192, 132, 252),
        Color::Cyan | Color::LightCyan => (34, 211, 238),
        Color::Indexed(_) => (228, 228, 231),
    };
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A box-drawing line or a block as SVG, for the cell at `(x, y)`.
fn shape(symbol: &str, x: f32, y: f32, color: &str) -> Option<String> {
    let c = symbol.chars().next()?;
    if symbol.chars().count() != 1 {
        return None;
    }
    let (cx, cy) = (x + CELL_W / 2., y + CELL_H / 2.);
    let line = |x1: f32, y1: f32, x2: f32, y2: f32| {
        format!(
            r#"<path d="M{} {}L{} {}" stroke="{color}" stroke-width="1"/>"#,
            n(x1),
            n(y1),
            n(x2),
            n(y2)
        )
    };
    let rect = |x: f32, y: f32, w: f32, h: f32| {
        format!(
            r#"<rect x="{}" y="{}" width="{}" height="{}" fill="{color}" shape-rendering="crispEdges"/>"#,
            n(x),
            n(y),
            n(w),
            n(h)
        )
    };
    // Rounded corners: from one side to the other through a curve.
    let r = CELL_W / 2.;
    let corner = |(x1, y1): (f32, f32), (x2, y2): (f32, f32)| {
        format!(
            r#"<path d="M{} {}Q{} {} {} {}" stroke="{color}" stroke-width="1" fill="none"/>"#,
            n(x1),
            n(y1),
            n(cx),
            n(cy),
            n(x2),
            n(y2)
        )
    };
    let (right, bottom) = (x + CELL_W, y + CELL_H);
    let arms = |left: bool, right_: bool, up: bool, down: bool| {
        let mut out = String::new();
        if left {
            out.push_str(&line(x, cy, cx, cy));
        }
        if right_ {
            out.push_str(&line(cx, cy, right, cy));
        }
        if up {
            out.push_str(&line(cx, y, cx, cy));
        }
        if down {
            out.push_str(&line(cx, cy, cx, bottom));
        }
        out
    };
    Some(match c {
        '─' => arms(true, true, false, false),
        '│' => arms(false, false, true, true),
        '┌' => arms(false, true, false, true),
        '┐' => arms(true, false, false, true),
        '└' => arms(false, true, true, false),
        '┘' => arms(true, false, true, false),
        '├' => arms(false, true, true, true),
        '┤' => arms(true, false, true, true),
        '┬' => arms(true, true, false, true),
        '┴' => arms(true, true, true, false),
        '┼' => arms(true, true, true, true),
        '╭' => {
            line(cx, bottom, cx, cy + r)
                + &corner((cx, cy + r), (cx + r, cy))
                + &line(cx + r, cy, right, cy)
        }
        '╮' => {
            line(x, cy, cx - r, cy)
                + &corner((cx - r, cy), (cx, cy + r))
                + &line(cx, cy + r, cx, bottom)
        }
        '╰' => {
            line(cx, y, cx, cy - r)
                + &corner((cx, cy - r), (cx + r, cy))
                + &line(cx + r, cy, right, cy)
        }
        '╯' => {
            line(x, cy, cx - r, cy) + &corner((cx - r, cy), (cx, cy - r)) + &line(cx, cy - r, cx, y)
        }
        '█' => rect(x, y, CELL_W, CELL_H),
        // Lower eighths, as the charts are drawn.
        '▁'..='▇' => {
            let h = CELL_H * (c as u32 - 0x2580) as f32 / 8.;
            rect(x, bottom - h, CELL_W, h)
        }
        // Left eighths, as the bars are drawn.
        '▉'..='▏' => {
            let eighths = (0x2590 - c as u32) as f32;
            rect(x, y, CELL_W * eighths / 8., CELL_H)
        }
        _ => return None,
    })
}

#[test]
fn a_screen_becomes_svg() {
    let mut buffer = Buffer::empty(ratatui::layout::Rect::new(0, 0, 6, 2));
    buffer[(0, 0)].set_symbol("╭").set_fg(theme::ACCENT);
    buffer[(1, 0)].set_symbol("a").set_fg(theme::TEXT);
    buffer[(2, 0)].set_symbol("<").set_fg(theme::TEXT);
    buffer[(0, 1)].set_symbol("▄");
    buffer[(1, 1)].set_symbol("▌");
    let svg = svg(&buffer, "t & t");
    assert!(svg.starts_with("<svg"));
    assert!(svg.contains(">a&lt;</text>"), "{svg}");
    assert!(svg.contains("t &amp; t"));
    // The half blocks: half the cell high, half the cell wide, in
    // round numbers.
    assert!(svg.contains(r#"y="59" width="8.4" height="9""#), "{svg}");
    assert!(svg.contains(r#"width="4.2" height="18""#), "{svg}");
    assert!(!svg.contains("0001") && !svg.contains("9999"), "{svg}");
}
