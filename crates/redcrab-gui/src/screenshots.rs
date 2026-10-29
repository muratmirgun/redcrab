//! The README's screenshots (`redcrab --screenshots <dir> <url>`, built with
//! the `screenshots` feature; scripts/screenshots.sh runs it). The app runs
//! in a window off the screen, drawn by the real Metal renderer through
//! GPUI's visual test platform and read back as an image, so taking them
//! needs neither the screen nor the focus. Each scene is shot in the dark
//! theme and the light one, then framed like a macOS window.

use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use gpui::VisualTestAppContext;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, WindowBounds, WindowOptions,
    component::TitleBar, point, px, size,
};
use image::{Rgba, RgbaImage};
use redcrab_core::{ConnectionConfig, Environment, settings::Appearance};

use crate::{
    app::{RedcrabApp, open_palette},
    assets::AppAssets,
    connect::Start,
    workspace::{View, Workspace},
};

/// The keys the keys view shows: a JSON document, and a hash under the
/// console.
const JSON: &str = "product:mon-27-4k";
const HASH: &str = "user:1042";

pub fn run(out: PathBuf, url: &str) {
    let platform = Rc::new(gpui_macos::MacPlatform::new(false));
    let cx = VisualTestAppContext::with_asset_source(platform, Arc::new(AppAssets));
    cx.executor().allow_parking();
    std::fs::create_dir_all(&out).expect("the screenshot folder");
    let mut config = ConnectionConfig::parse_url(url).expect("a Redis URL");
    config.name = "shop".into();
    config.environment = Environment::Production;
    let mut shots = Shots {
        cx,
        window: None,
        out,
        theme: "dark",
    };
    shots.cx.update(crate::setup);
    let (window, app) = shots.open(Start::Connect(config));
    shots.window = Some(window);
    let workspace = shots.until("the workspace", |cx| app.read(cx).workspace());

    // What the scenes need, once: the server's trends, an analysis, and
    // messages.
    shots.show(&workspace, View::Server);
    for _ in 0..45 {
        shots.cx.advance_clock(Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(400));
        shots.cx.run_until_parked();
    }
    shots.show(&workspace, View::Memory);
    let memory = shots.until("the memory view", |cx| workspace.read(cx).memory());
    shots
        .cx
        .update(|cx| memory.update(cx, |memory, cx| memory.analyze(cx)));
    shots.settle(Duration::from_secs(3));
    shots.show(&workspace, View::PubSub);
    let pubsub = shots.until("the Pub/Sub view", |cx| workspace.read(cx).pubsub());
    for channel in ["orders.*", "notifications"] {
        shots.with_window(|window, cx| {
            pubsub.update(cx, |pubsub, cx| {
                pubsub.subscribe(channel.as_bytes().to_vec(), window, cx)
            })
        });
    }
    shots.settle(Duration::from_secs(1));
    let client = shots.cx.read(|cx| workspace.read(cx).client().clone());
    for (channel, message) in messages() {
        futures::executor::block_on(client.publish(channel.into(), message.into()))
            .expect("a message published");
        shots.settle(Duration::from_millis(120));
    }
    shots.show(&workspace, View::Keys);
    shots.open_key(&workspace, HASH);
    shots.console(&workspace, true);
    for command in [
        "HGETALL user:1042",
        "ZREVRANGE leaderboard:weekly 0 4 WITHSCORES",
    ] {
        shots.cx.simulate_input(window, command);
        shots.cx.simulate_keystrokes(window, "enter");
        shots.settle(Duration::from_millis(500));
    }
    shots.console(&workspace, false);

    for (theme, appearance) in [("dark", Appearance::Dark), ("light", Appearance::Light)] {
        shots.theme = theme;
        shots
            .cx
            .update(|cx| crate::settings::change(cx, |settings| settings.appearance = appearance));
        // The keys, every folder closed but the one of the key shown.
        shots.show(&workspace, View::Keys);
        shots.close_folders(&workspace);
        shots.open_key(&workspace, JSON);
        shots.shot("keys");
        shots.cx.update(open_palette);
        shots.settle(Duration::from_millis(500));
        shots.shot("palette");
        shots.cx.simulate_keystrokes(window, "escape");
        shots.close_folders(&workspace);
        shots.open_key(&workspace, HASH);
        shots.console(&workspace, true);
        shots.shot("console");
        shots.console(&workspace, false);
        shots.show(&workspace, View::Server);
        shots.settle(Duration::from_secs(1));
        shots.shot("server");
        shots.show(&workspace, View::Memory);
        shots.shot("memory");
        shots.show(&workspace, View::PubSub);
        shots.shot("pubsub");
    }
}

struct Shots {
    cx: VisualTestAppContext,
    window: Option<AnyWindowHandle>,
    out: PathBuf,
    theme: &'static str,
}

impl Shots {
    fn open(&mut self, start: Start) -> (AnyWindowHandle, Entity<RedcrabApp>) {
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(-10000.), px(-10000.)),
                size: size(px(1280.), px(800.)),
            })),
            focus: false,
            show: true,
            ..TitleBar::window_options()
        };
        self.cx
            .update(|cx| {
                gpui_kit::open_window(options, cx, |window, cx| {
                    cx.new(|cx| RedcrabApp::new(start, window, cx))
                })
            })
            .expect("a window")
    }

    fn window(&self) -> AnyWindowHandle {
        self.window.expect("the window is open")
    }

    fn with_window<R>(
        &mut self,
        f: impl FnOnce(&mut gpui_kit::Window, &mut gpui_kit::App) -> R,
    ) -> R {
        let window = self.window();
        self.cx
            .update_window(window, |_, window, cx| f(window, cx))
            .expect("the window is open")
    }

    /// Runs the app for `time`, on the I/O runtime's threads too.
    fn settle(&mut self, time: Duration) {
        for _ in 0..(time.as_millis() / 10).max(1) {
            self.cx.run_until_parked();
            self.cx.advance_clock(Duration::from_millis(10));
            std::thread::sleep(Duration::from_millis(10));
        }
        self.cx.run_until_parked();
    }

    /// Runs the app until `get` gives something, for at most ten seconds.
    fn until<T>(&mut self, what: &str, get: impl Fn(&gpui_kit::App) -> Option<T>) -> T {
        for _ in 0..1000 {
            if let Some(found) = self.cx.read(&get) {
                return found;
            }
            self.settle(Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
    }

    fn show(&mut self, workspace: &Entity<Workspace>, view: View) {
        self.with_window(|window, cx| {
            workspace.update(cx, |workspace, cx| workspace.show(view, window, cx))
        });
        self.settle(Duration::from_millis(300));
    }

    fn console(&mut self, workspace: &Entity<Workspace>, show: bool) {
        self.with_window(|window, cx| {
            workspace.update(cx, |workspace, cx| workspace.set_console(show, window, cx))
        });
        self.settle(Duration::from_millis(400));
    }

    fn close_folders(&mut self, workspace: &Entity<Workspace>) {
        self.cx
            .update(|cx| workspace.update(cx, |workspace, cx| workspace.close_folders(cx)));
        self.settle(Duration::from_millis(100));
    }

    fn open_key(&mut self, workspace: &Entity<Workspace>, key: &str) {
        self.with_window(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_key(key.as_bytes().to_vec(), window, cx)
            })
        });
        self.settle(Duration::from_millis(800));
    }

    /// Draws the window, reads it back, frames it and saves it as
    /// `<name>-<theme>.png`.
    fn shot(&mut self, name: &str) {
        let window = self.window();
        // Twice: some sizes are measured in one frame and used in the next.
        for _ in 0..2 {
            self.with_window(|window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
            self.settle(Duration::from_millis(50));
        }
        let mut image = self.cx.capture_screenshot(window).expect("a screenshot");
        frame(&mut image, self.theme == "dark");
        let path = self.out.join(format!("{name}-{}.png", self.theme));
        save(&image, &path);
    }
}

/// Saves `image` as a PNG, compressed hard: it goes into the repository.
fn save(image: &RgbaImage, path: &Path) {
    use image::{
        ExtendedColorType, ImageEncoder as _,
        codecs::png::{CompressionType, FilterType, PngEncoder},
    };
    let file = std::fs::File::create(path).expect("the image file");
    PngEncoder::new_with_quality(
        std::io::BufWriter::new(file),
        CompressionType::Best,
        FilterType::Adaptive,
    )
    .write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        ExtendedColorType::Rgba8,
    )
    .expect("the image saved");
    println!("{}", path.display());
}

/// What the Pub/Sub view hears: orders as they go, and a notification.
fn messages() -> Vec<(&'static str, String)> {
    let mut messages = Vec::new();
    for (n, (event, user, total)) in [
        ("created", 1042, "129.00"),
        ("paid", 1042, "129.00"),
        ("created", 1777, "549.00"),
        ("packed", 1042, "129.00"),
        ("paid", 1777, "549.00"),
        ("created", 2051, "88.50"),
        ("shipped", 1042, "129.00"),
        ("paid", 2051, "88.50"),
        ("packed", 1777, "549.00"),
    ]
    .into_iter()
    .enumerate()
    {
        messages.push((
            [
                "orders.created",
                "orders.paid",
                "orders.packed",
                "orders.shipped",
            ]
            .into_iter()
            .find(|channel| channel.ends_with(event))
            .unwrap_or("orders.created"),
            format!(
                r#"{{"order":{},"user":"user:{user}","total":"{total}","currency":"EUR"}}"#,
                50480 + n % 3
            ),
        ));
        if n == 4 {
            messages.push((
                "notifications",
                "Stock low: mon-27-4k has 3 left".to_owned(),
            ));
        }
    }
    messages
}

/// Rounds the corners, draws the window buttons GPUI leaves to macOS, and
/// edges the window with a hairline, as a macOS window looks.
fn frame(image: &mut RgbaImage, dark: bool) {
    const RADIUS: f32 = 10.;
    let (width, height) = (image.width() as f32, image.height() as f32);
    let edge = if dark {
        Rgba([255, 255, 255, 40])
    } else {
        Rgba([0, 0, 0, 50])
    };
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let d = rounded_rect(x as f32 + 0.5, y as f32 + 0.5, width, height, RADIUS);
        // Inside the edge: the hairline, then the window itself.
        let line = (1. - (d + 0.5).abs()).clamp(0., 1.);
        blend(pixel, edge, line);
        let inside = (0.5 - d).clamp(0., 1.);
        pixel.0[3] = (f32::from(pixel.0[3]) * inside).round() as u8;
    }
    for (ix, color) in [[255, 95, 87], [254, 188, 46], [40, 200, 64]]
        .into_iter()
        .enumerate()
    {
        let (cx, cy) = (16. + 20. * ix as f32, 16.);
        for y in 8..25 {
            for x in (cx as u32 - 8)..(cx as u32 + 8) {
                let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                let cover = (6.5 - d).clamp(0., 1.);
                blend(
                    image.get_pixel_mut(x, y),
                    Rgba([color[0], color[1], color[2], 255]),
                    cover,
                );
            }
        }
    }
}

/// How far `(x, y)` is from the edge of a `width` by `height` rectangle
/// with corners of `radius`: negative inside.
fn rounded_rect(x: f32, y: f32, width: f32, height: f32, radius: f32) -> f32 {
    let qx = (x - width / 2.).abs() - (width / 2. - radius);
    let qy = (y - height / 2.).abs() - (height / 2. - radius);
    let outside = (qx.max(0.).powi(2) + qy.max(0.).powi(2)).sqrt();
    outside + qx.max(qy).min(0.) - radius
}

/// Lays `color` over `pixel`, `amount` of it.
fn blend(pixel: &mut Rgba<u8>, color: Rgba<u8>, amount: f32) {
    let amount = amount * f32::from(color.0[3]) / 255.;
    if amount <= 0. {
        return;
    }
    for channel in 0..3 {
        let under = f32::from(pixel.0[channel]);
        let over = f32::from(color.0[channel]);
        pixel.0[channel] = (under + (over - under) * amount).round() as u8;
    }
}
