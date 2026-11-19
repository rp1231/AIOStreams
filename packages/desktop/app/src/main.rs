#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod links;
mod logging;
mod media;
mod placement;
mod platform;
mod shell;
mod updates;

use std::cell::RefCell;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use aiostreams_desktop_core::bridge::{Inbound, Outbound, PROTOCOL_VERSION, VideoFrame, origin};
use aiostreams_desktop_core::downloads::{self, Downloads};
use aiostreams_desktop_core::external::{self, External};
use aiostreams_desktop_core::playback::{self, PlayerState};
use aiostreams_desktop_core::player::Player;
use aiostreams_desktop_core::{discord, licenses, now_playing, thumbnails};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use updates::{Command, Updater};

#[derive(Debug)]
pub enum UserEvent {
    Emit(String),
    Fullscreen(Option<bool>),
    /// The page switched its TV layout on or off, or changed its width.
    TvLayout(Layout),
    Pip(Option<bool>),
    VideoFrame(Option<VideoFrame>),
    Minimize,
    Close,
    /// The player closed after the window did.
    Closed,
    Sync,
    Drag,
    Resize(Edge),
    ToggleMaximize,
    WindowState,
    WindowButtons(bool),
    #[cfg(target_os = "macos")]
    FullscreenLeft,
    Link(String),
    LinksReady,
    ChoosePlayer(external::Kind),
    ChooseDownloadFolder,
}

/// The window edges the page resizes from; the system handles the others.
#[derive(Debug, Clone, Copy)]
pub enum Edge {
    North,
    NorthEast,
    NorthWest,
}

pub struct Args {
    web: Option<String>,
    web_dir: Option<PathBuf>,
    devtools: bool,
    debug_port: Option<u16>,
    link: Option<String>,
    /// The page's TV layout, for a remote or a gamepad on a TV.
    tv: bool,
    /// The page width the TV layout is laid out at.
    tv_width: u32,
}

fn args() -> Args {
    args_from(std::env::args().skip(1))
}

fn args_from(given: impl IntoIterator<Item = String>) -> Args {
    let mut args = Args {
        web: None,
        web_dir: None,
        devtools: cfg!(debug_assertions),
        debug_port: None,
        link: None,
        tv: false,
        tv_width: TV_WIDTH,
    };
    let mut it = given.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--web" => args.web = it.next(),
            "--web-dir" => args.web_dir = it.next().map(PathBuf::from),
            "--devtools" => args.devtools = true,
            "--tv" => args.tv = true,
            "--remote-debugging-port" => args.debug_port = it.next().and_then(|p| p.parse().ok()),
            _ => match links::accept(&arg) {
                Some(link) => args.link = Some(link),
                None => log::warn!("unknown argument {arg}"),
            },
        }
    }
    args
}

fn app_dir(base: Option<PathBuf>) -> PathBuf {
    base.unwrap_or_else(std::env::temp_dir)
        .join("AIOStreams Desktop")
}

/// A portable copy's own folder. Velopack runs the app from `<root>/current`,
/// which each update replaces, and marks a portable root with `.portable`.
fn portable_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let root = exe.parent()?.parent()?;
    root.join(".portable").is_file().then(|| root.to_path_buf())
}

/// Beside the app or libmpv, or in a macOS bundle's Resources beside either.
fn licence_dirs() -> Vec<PathBuf> {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(PathBuf::from));
    let libmpv = platform::libmpv_candidates()
        .into_iter()
        .filter_map(|p| p.parent().map(PathBuf::from));
    exe.into_iter()
        .chain(libmpv)
        .flat_map(|dir| [dir.join("../Resources"), dir])
        .collect()
}

fn web_dir(args: &Args) -> PathBuf {
    let mut candidates: Vec<PathBuf> = args.web_dir.iter().cloned().collect();
    candidates.extend(std::env::var_os("AIOSTREAMS_WEB_DIR").map(PathBuf::from));
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(PathBuf::from))
    {
        candidates.push(dir.join("web"));
        // Inside a macOS app bundle, beside `Contents/MacOS`.
        candidates.push(dir.join("../Resources/web"));
    }
    if cfg!(debug_assertions) {
        candidates.push(PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../web/dist-standalone"
        )));
    }
    candidates
        .iter()
        .find(|dir| dir.join("index.html").is_file())
        .cloned()
        .unwrap_or_else(|| {
            platform::fatal(&format!(
                "The web app was not found. Build it with `pnpm -F @aiostreams/web build:standalone`. Looked in:\n{}",
                candidates
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            ))
        })
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        _ => "application/octet-stream",
    }
}

pub struct Served {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

/// Serves the web app's files and thumbnails; a path without an extension is one of its routes.
pub fn serve(root: Option<&Path>, path: &str) -> Served {
    if let Some(body) = thumbnails::image(path) {
        return Served {
            status: 200,
            content_type: "image/jpeg",
            body,
        };
    }
    let not_found = Served {
        status: 404,
        content_type: "text/plain",
        body: Vec::new(),
    };
    let Some(root) = root else { return not_found };
    let relative = Path::new(path.trim_start_matches('/'));
    if relative
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return not_found;
    }
    let mut file = root.join(relative);
    if !file.is_file() {
        if relative.extension().is_some() {
            return not_found;
        }
        file = root.join("index.html");
    }
    match std::fs::read(&file) {
        Ok(body) => Served {
            status: 200,
            content_type: content_type(&file),
            body,
        },
        Err(_) => not_found,
    }
}

pub fn allowed_navigation(url: &str, app_origin: &str) -> bool {
    origin(url).as_deref() == Some(app_origin)
        || url.starts_with("about:")
        || url.starts_with("blob:")
}

/// Where the app keeps its files, and the day's log.
pub struct Paths {
    mpv: PathBuf,
    logs: PathBuf,
    log_file: PathBuf,
    players: PathBuf,
    subtitles: PathBuf,
    /// The download queue, kept across restarts.
    downloads: PathBuf,
    /// Where downloads go until the user picks a folder.
    download_folder: PathBuf,
    /// The upscaling shader, which mpv only reads from a file.
    upscaling_hook: PathBuf,
    /// Whether the page last chose the TV layout, so the next start opens at its zoom.
    tv_layout: PathBuf,
}

/// The page width a TV lays the TV layout out at, and the one it starts at here.
pub const TV_WIDTH: u32 = 1280;
/// The page widths the TV layout may be laid out at.
const TV_WIDTHS: std::ops::RangeInclusive<u32> = 960..=3840;

/// The page's layout: the TV layout or not, and the width it lays the TV layout out at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub tv: bool,
    pub width: u32,
}

/// A width the TV layout may take, or none.
fn tv_width(width: u32) -> Option<u32> {
    TV_WIDTHS.contains(&width).then_some(width)
}

/// The layout the page last chose, if it has: `tv` or `desktop`, then its TV width.
fn load_tv_layout(path: &Path) -> Option<Layout> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut words = text.split_whitespace();
    let tv = match words.next()? {
        "tv" => true,
        "desktop" => false,
        _ => return None,
    };
    let width = words
        .next()
        .and_then(|w| w.parse().ok())
        .and_then(tv_width)
        .unwrap_or(TV_WIDTH);
    Some(Layout { tv, width })
}

/// The layout to open in: the page's last choice, or `--tv` until it has made one.
fn starting_tv_layout(path: &Path, flag: bool) -> Layout {
    load_tv_layout(path).unwrap_or(Layout {
        tv: flag,
        width: TV_WIDTH,
    })
}

fn save_tv_layout(path: &Path, layout: Layout) {
    let text = format!(
        "{} {}",
        if layout.tv { "tv" } else { "desktop" },
        layout.width
    );
    if let Err(e) = std::fs::write(path, text) {
        log::warn!("could not save the layout to {}: {e}", path.display());
    }
}

pub struct App {
    pub args: Args,
    pub web: Option<PathBuf>,
    pub start_url: String,
    pub app_origin: String,
    pub data_dir: PathBuf,
    pub paths: Rc<Paths>,
    pub bridge: String,
}

fn about() -> String {
    format!(
        "version={} os=\"{}\" webview={}",
        env!("CARGO_PKG_VERSION"),
        platform::os_version(),
        shell::webview_version()
    )
}

fn bridge_script(layout: Layout) -> String {
    include_str!("bridge.js")
        .replace("__PROTOCOL__", &PROTOCOL_VERSION.to_string())
        .replace("__TV_WIDTH__", &layout.width.to_string())
        .replace("__TV__", &layout.tv.to_string())
        .replace(
            "__VERSION__",
            &serde_json::to_string(env!("CARGO_PKG_VERSION")).unwrap(),
        )
        .replace(
            "__PLATFORM__",
            &serde_json::to_string(platform::PLATFORM).unwrap(),
        )
        .replace("__PIP__", &platform::PIP.to_string())
        .replace(
            "__DEVICE__",
            &serde_json::to_string(&platform::device_name()).unwrap(),
        )
        .replace(
            "__HOURS24__",
            &platform::hours24().map_or("undefined".into(), |on| on.to_string()),
        )
}

fn main() {
    // Runs Velopack's install and update hooks, which exit when they are the reason for this launch.
    let mut velopack = velopack::VelopackApp::build();
    // Only an installed copy claims the scheme, so a portable one never takes it over.
    #[cfg(windows)]
    {
        velopack = velopack
            .on_after_install_fast_callback(|_| platform::register_links())
            .on_after_update_fast_callback(|_| platform::register_links())
            .on_before_uninstall_fast_callback(|_| platform::unregister_links());
    }
    velopack.run();
    #[cfg(windows)]
    platform::claim_app_id();
    let portable = portable_root();
    let (config_dir, data_dir) = match &portable {
        Some(root) => (root.join("data"), root.join("data")),
        None => (app_dir(dirs::config_dir()), app_dir(dirs::data_local_dir())),
    };
    let download_folder = match (&portable, dirs::video_dir()) {
        (None, Some(videos)) => videos.join("AIOStreams"),
        _ => data_dir.join("downloads"),
    };
    let logs = data_dir.join("logs");
    let log_file = logging::init(&logs);
    log::info!("starting {}", about());
    let args = args();
    let Some(_instance) = platform::claim_instance(&data_dir, args.link.as_deref()) else {
        log::info!("already running; brought its window forward");
        return;
    };
    platform::check_super_resolution();
    #[cfg(target_os = "linux")]
    if let Some(port) = args.debug_port {
        // SAFETY: set before the web view starts, which is what reads it.
        unsafe { std::env::set_var("WEBKIT_INSPECTOR_HTTP_SERVER", format!("127.0.0.1:{port}")) };
    }

    let web = args.web.is_none().then(|| web_dir(&args));
    let start_url = args.web.clone().unwrap_or_else(|| platform::APP_URL.into());
    log::info!(
        "paths web={} data={}",
        web.as_ref()
            .map(|dir| dir.display().to_string())
            .unwrap_or_else(|| start_url.clone()),
        data_dir.display()
    );
    let app_origin =
        origin(&start_url).unwrap_or_else(|| platform::fatal("--web: not a valid address"));
    let subtitles = data_dir.join("subtitles");
    let _ = std::fs::remove_dir_all(&subtitles);
    let paths = Rc::new(Paths {
        mpv: mpv_config_dir(&config_dir),
        logs,
        log_file,
        players: config_dir.join("players.json"),
        subtitles,
        downloads: data_dir.join("downloads.json"),
        download_folder,
        upscaling_hook: data_dir.join("upscaling.glsl"),
        tv_layout: data_dir.join("layout"),
    });
    // The layout chosen in the page's settings wins over --tv, which only picks the first one.
    let mut args = args;
    let layout = starting_tv_layout(&paths.tv_layout, args.tv);
    (args.tv, args.tv_width) = (layout.tv, layout.width);
    let bridge = bridge_script(layout);
    shell::run(App {
        args,
        web,
        start_url,
        app_origin,
        data_dir,
        paths,
        bridge,
    });
}

pub fn receive_script(message: &Outbound) -> String {
    format!("window.__aiostreamsAppReceive?.({})", message.to_json())
}

fn mpv_config_dir(config_dir: &Path) -> PathBuf {
    let dir = config_dir.join("mpv");
    let _ = std::fs::create_dir_all(&dir);
    let conf = dir.join("mpv.conf");
    if !conf.exists() {
        let _ = std::fs::write(&conf, include_str!("mpv.conf"));
    }
    dir
}

/// `emit` is called on mpv's event thread.
static VIDEO_ASPECT: Mutex<Option<f64>> = Mutex::new(None);

#[cfg(not(target_os = "linux"))]
pub fn video_aspect() -> Option<f64> {
    VIDEO_ASPECT.lock().ok().and_then(|aspect| *aspect)
}

pub fn start_player(
    video: &platform::VideoSurface,
    paths: &Paths,
    emit: impl Fn(Outbound) + Send + Sync + 'static,
) -> Player {
    let mpv_dir = &paths.mpv;
    let mut defaults: Vec<(&str, String)> = vec![
        ("config-dir", mpv_dir.to_string_lossy().into_owned()),
        ("config", "yes".into()),
        ("audio-client-name", "AIOStreams".into()),
        // `%23%` quotes the next 23 characters, whose commas would split the option.
        (
            "stream-lavf-o",
            "reconnect=1,reconnect_on_http_error=%23%408,429,500,502,503,504,reconnect_delay_max=15".into(),
        ),
    ];
    defaults.extend(platform::mpv_options(video));
    let defaults: Vec<(&str, &str)> = defaults.iter().map(|(k, v)| (*k, v.as_str())).collect();
    // The page drives playback, so these hold whatever mpv.conf says.
    let required = [
        ("idle", "yes"),
        ("keep-open", "no"),
        // With the render API, mpv's output needs the app's OpenGL context first.
        ("force-window", if cfg!(windows) { "yes" } else { "no" }),
        ("input-default-bindings", "no"),
        ("input-vo-keyboard", "no"),
        ("input-cursor", "no"),
        ("osc", "no"),
        ("osd-bar", "no"),
        ("background", "color"),
        ("background-color", "#000000"),
        ("ytdl", "no"),
    ];

    let candidates = platform::libmpv_candidates();
    let Some(library) = candidates.iter().find(|p| p.exists()) else {
        platform::fatal(&format!(
            "libmpv was not found. Looked in:\n{}",
            candidates
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("\n")
        ));
    };
    log::info!(
        "mpv library={} config={}",
        library.display(),
        mpv_dir.display()
    );
    #[cfg(windows)]
    platform::load_vulkan_loader(library);
    let awake = Mutex::new(Awake::default());
    let emit = move |message: Outbound| {
        if let Outbound::PlayerState { state, .. } = &message
            && let Ok(mut awake) = awake.lock()
        {
            awake.update(state);
        }
        if let Outbound::VideoColour { gamma, primaries } = message {
            platform::update_hdr(gamma, primaries);
            return;
        }
        if let Outbound::VideoAspect { aspect } = message {
            if let Ok(mut shown) = VIDEO_ASPECT.lock() {
                *shown = aspect;
            }
            return;
        }
        now_playing::observe(&message);
        emit(message)
    };
    let sized = std::fs::read_to_string(mpv_dir.join("mpv.conf"))
        .is_ok_and(|c| c.contains("demuxer-max-bytes"));
    let auto_cache = (!sized).then(|| playback::auto_cache(platform::total_memory()));
    Player::start(
        library,
        &defaults,
        &required,
        auto_cache,
        paths.upscaling_hook.clone(),
        platform::display_probe(video),
        Arc::new(emit),
    )
    .unwrap_or_else(|e| platform::fatal(&format!("mpv failed to start: {e}")))
}

/// `emit` is called on the player's own threads.
pub fn start_external(paths: &Paths, emit: impl Fn(Outbound) + Send + Sync + 'static) -> External {
    External::new(
        paths.players.clone(),
        Arc::new(move |message: Outbound| {
            now_playing::observe(&message);
            emit(message)
        }),
    )
}

/// `emit` is called on the download queue's thread.
pub fn start_downloads(paths: &Paths, emit: impl Fn(Outbound) + Send + 'static) -> Downloads {
    Downloads::start(paths.downloads.clone(), paths.download_folder.clone(), emit)
}

const SUBTITLE_TYPES: &[&str] = &["srt", "vtt", "ass", "ssa", "sub", "sup"];
const MAX_SUBTITLE_BYTES: usize = 10 << 20;
const MAX_PAGE_TEXT: usize = 4000;

/// The page never names a path for mpv to open, so the app writes the file itself.
fn save_subtitle(dir: &Path, name: &str, data: &str) -> Result<PathBuf, String> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let extension = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|e| SUBTITLE_TYPES.contains(&e.as_str()))
        .ok_or("not a subtitle file")?;
    let bytes = BASE64.decode(data).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_SUBTITLE_BYTES {
        return Err("too big".into());
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!(
        "{}.{extension}",
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Keeps the display on while a file plays.
struct Awake {
    paused: bool,
    idle: bool,
    on: bool,
}

impl Default for Awake {
    fn default() -> Self {
        Self {
            paused: false,
            idle: true,
            on: false,
        }
    }
}

impl Awake {
    fn update(&mut self, state: &PlayerState) {
        if state.paused.is_none() && state.idle.is_none() {
            return;
        }
        self.paused = state.paused.unwrap_or(self.paused);
        self.idle = state.idle.unwrap_or(self.idle);
        let on = !self.paused && !self.idle;
        if on != self.on {
            self.on = on;
            platform::keep_awake(on);
        }
    }
}

pub fn handle(
    message: Inbound,
    player: &RefCell<Option<Player>>,
    external: &External,
    send: &dyn Fn(UserEvent),
    paths: &Paths,
    updater: &Option<Updater>,
    downloads: &Downloads,
) {
    let emit = |message: Outbound| send(UserEvent::Emit(receive_script(&message)));
    let fail = |message: String| {
        log::warn!("{message}");
        emit(Outbound::Error { message });
    };
    let player = player.borrow();
    match message {
        Inbound::PlayerLoad {
            load,
            external: to_external,
        } => {
            let local = |path: &str| downloads.is_local(path);
            let done = if to_external {
                Some(external.load(&load, &local))
            } else {
                player.as_ref().map(|p| p.load(&load, &local))
            };
            if let Some(Err(e)) = done {
                fail(format!("player-load: {e}"));
            }
        }
        Inbound::PlayerStop { external: true } => external.stop(),
        Inbound::PlayerStop { external: false } => {
            if let Some(p) = player.as_ref() {
                p.stop();
            }
        }
        Inbound::PlayerUnqueue { external: true } => external.unqueue(),
        Inbound::PlayerUnqueue { external: false } => {
            if let Some(p) = player.as_ref() {
                p.unqueue();
            }
        }
        Inbound::PlayerSet {
            change,
            external: to_external,
        } => {
            let done = if to_external {
                Some(external.change(&change))
            } else {
                player.as_ref().map(|p| p.change(&change))
            };
            if let Some(Err(e)) = done {
                fail(format!("player-set: {e}"));
            }
        }
        Inbound::PlayerSeek {
            position_ms,
            external: true,
        } => external.seek(position_ms),
        Inbound::PlayerSeek { position_ms, .. } => {
            if let Some(p) = player.as_ref() {
                p.seek(position_ms);
            }
        }
        Inbound::PlayerSubtitleAdd {
            url,
            title,
            language,
            replace,
            external: to_external,
        } => {
            let local = |path: &str| downloads.is_local(path);
            let done = if to_external {
                Some(external.add_subtitle_url(&url, &title, &language, replace, &local))
            } else {
                player
                    .as_ref()
                    .map(|p| p.add_subtitle_url(&url, &title, &language, replace, &local))
            };
            if let Some(Err(e)) = done {
                fail(format!("player-subtitle-add: {e}"));
            }
        }
        Inbound::PlayerStyle(mut style) => {
            style.fit = platform::fit_video(style.fit);
            if let Some(Err(e)) = player.as_ref().map(|p| p.style(&style)) {
                fail(format!("player-style: {e}"));
            }
        }
        Inbound::PlayerStatsOverlay {
            page,
            external: to_external,
        } => {
            let done = if to_external {
                Some(external.stats_overlay(page.as_deref()))
            } else {
                player.as_ref().map(|p| p.stats_overlay(page.as_deref()))
            };
            if let Some(Err(e)) = done {
                fail(format!("player-stats-overlay: {e}"));
            }
        }
        Inbound::PlayerSubtitleLines { track } => {
            if let Some(Err(e)) = player.as_ref().map(|p| p.subtitle_lines(track.as_deref())) {
                fail(format!("player-subtitle-lines: {e}"));
            }
        }
        Inbound::PlayerSubtitleIndex => match player.as_ref() {
            Some(p) => p.subtitle_index(),
            None => emit(Outbound::PlayerSubtitleIndex { tracks: None }),
        },
        Inbound::PlayerThumbnail {
            url,
            headers,
            position_ms,
            showing_ms,
        } => {
            let local = |path: &str| downloads.is_local(path);
            let done = player
                .as_ref()
                .map(|p| p.thumbnail(&url, &headers, position_ms, showing_ms, &local));
            if let Some(Err(e)) = done {
                fail(format!("player-thumbnail: {e}"));
            }
        }
        Inbound::PlayerSync { external: true } => external.sync(),
        Inbound::PlayerSync { external: false } => send(UserEvent::Sync),
        Inbound::PlayerKey { key } => {
            if let Some(Err(e)) = player.as_ref().map(|p| p.keypress(&key)) {
                fail(format!("player-key: {e}"));
            }
        }
        Inbound::PlayerSettings(settings) => {
            if let Some(p) = player.as_ref() {
                p.settings(&settings);
            }
        }
        Inbound::YoutubeResolve { request, id } => {
            if let Some(p) = player.as_ref() {
                p.resolve_youtube(request, id);
            }
        }
        Inbound::AudioDevices => {
            if let Some(p) = player.as_ref() {
                p.audio_devices();
            }
        }
        Inbound::SubtitleFile {
            name,
            data,
            external: to_external,
        } => {
            let title: String = name.chars().take(200).collect();
            match save_subtitle(&paths.subtitles, &name, &data) {
                Ok(path) if to_external => external.add_subtitle(&path, &title),
                Ok(path) => {
                    if let Some(p) = player.as_ref() {
                        p.add_subtitle(&path, &title);
                    }
                }
                Err(e) => fail(format!("subtitle file {title}: {e}")),
            }
        }
        Inbound::ExternalPlayers => emit(external.players()),
        Inbound::ExternalChoose { player } => match external::Kind::parse(&player) {
            Some(kind) => send(UserEvent::ChoosePlayer(kind)),
            None => log::warn!("external-choose: unknown player {player}"),
        },
        Inbound::ExternalOpen { player, title } => match external::Kind::parse(&player) {
            Some(kind) => external.open(kind, title.as_deref()),
            None => log::warn!("external-open: unknown player {player}"),
        },
        Inbound::ExternalClose => external.close(),
        Inbound::Fullscreen { value } => send(UserEvent::Fullscreen(value)),
        Inbound::TvLayout { value, width } => {
            let width = width
                .and_then(tv_width)
                .or_else(|| load_tv_layout(&paths.tv_layout).map(|l| l.width))
                .unwrap_or(TV_WIDTH);
            let layout = Layout { tv: value, width };
            save_tv_layout(&paths.tv_layout, layout);
            send(UserEvent::TvLayout(layout));
        }
        Inbound::Pip { value } => send(UserEvent::Pip(value)),
        Inbound::PlayerFrame { frame } => {
            if let Some(p) = player.as_ref() {
                let shown = frame.is_none_or(|frame| frame.subtitles);
                let _ = p
                    .mpv()
                    .set_property_async("sub-visibility", if shown { "yes" } else { "no" });
            }
            send(UserEvent::VideoFrame(frame));
        }
        Inbound::Minimize => send(UserEvent::Minimize),
        Inbound::WindowDrag => send(UserEvent::Drag),
        Inbound::WindowResize { edge } => match edge.as_str() {
            "n" => send(UserEvent::Resize(Edge::North)),
            "ne" => send(UserEvent::Resize(Edge::NorthEast)),
            "nw" => send(UserEvent::Resize(Edge::NorthWest)),
            _ => log::warn!("window-resize: unknown edge {edge}"),
        },
        Inbound::WindowMaximize => send(UserEvent::ToggleMaximize),
        Inbound::WindowState => send(UserEvent::WindowState),
        Inbound::WindowButtons { visible } => send(UserEvent::WindowButtons(visible)),
        Inbound::Close => send(UserEvent::Close),
        Inbound::AppInfo => {
            let (mpv, ffmpeg) = player.as_ref().map(Player::versions).unwrap_or_default();
            emit(Outbound::AppInfo {
                app: env!("CARGO_PKG_VERSION"),
                platform: platform::PLATFORM,
                mpv,
                ffmpeg,
                ac3_encoder: player.as_ref().is_some_and(Player::ac3_encoder),
                super_resolution: platform::super_resolution(),
            });
        }
        Inbound::VideoOutput { output } => {
            if let Some(output) = output {
                platform::choose_video_output(&output);
            }
            let (outputs, running, chosen) = platform::video_outputs();
            emit(Outbound::VideoOutput {
                outputs,
                running,
                chosen,
            });
        }
        Inbound::Restart => {
            if platform::relaunch() {
                send(UserEvent::Close);
            }
        }
        Inbound::OpenMpvConfig => platform::open_external(&paths.mpv.to_string_lossy()),
        Inbound::OpenLogs => platform::open_external(&paths.logs.to_string_lossy()),
        Inbound::Diagnostics { web, server } => {
            let (mpv, ffmpeg) = player.as_ref().map(Player::versions).unwrap_or_default();
            let text = format!(
                "AIOStreams Desktop {}\nweb=\"{}\" server=\"{}\"\nmpv=\"{}\" ffmpeg={}\nlog={}\n\n{}",
                about(),
                web.unwrap_or_default(),
                server.unwrap_or_default(),
                mpv.unwrap_or_default(),
                ffmpeg.unwrap_or_default(),
                paths.log_file.display(),
                logging::tail(&paths.log_file, 300)
            );
            emit(Outbound::Diagnostics { text });
        }
        Inbound::Licenses => emit(Outbound::Licenses(licenses::read(&licence_dirs()))),
        Inbound::UpdateCheck { channel } => match updater {
            Some(updater) => updater.send(Command::Check(channel)),
            None => emit(Outbound::UpdateState {
                state: "off",
                channel: None,
                version: None,
                error: None,
            }),
        },
        Inbound::UpdateApply => {
            if let Some(updater) = updater {
                updater.send(Command::Apply);
            }
        }
        Inbound::Presence { presence } => now_playing::browsing(presence),
        Inbound::NowPlaying { item } => now_playing::set_item(item),
        Inbound::DiscordCheck => discord::check(),
        Inbound::LinksReady => send(UserEvent::LinksReady),
        Inbound::DownloadAdd { jobs } => downloads.send(downloads::Command::Add(jobs)),
        Inbound::DownloadControl { id, action } => match action.as_str() {
            "pause" => downloads.send(downloads::Command::Pause(id)),
            "resume" => downloads.send(downloads::Command::Resume(id)),
            "retry" => downloads.send(downloads::Command::Retry(id)),
            _ => log::warn!("download-control: unknown action {action}"),
        },
        Inbound::DownloadRemove { id, files } => {
            downloads.send(downloads::Command::Remove { id, files })
        }
        Inbound::DownloadList => downloads.send(downloads::Command::List),
        Inbound::DownloadFolder => send(UserEvent::ChooseDownloadFolder),
        Inbound::DownloadOpen { id } => {
            let folder = id
                .and_then(|id| downloads.job_folder(&id))
                .unwrap_or_else(|| downloads.folder());
            let _ = std::fs::create_dir_all(&folder);
            platform::open_external(&folder.to_string_lossy());
        }
        Inbound::DownloadConfig { concurrent } => {
            downloads.send(downloads::Command::SetConcurrent(concurrent))
        }
        Inbound::WebError { message } => log::error!(target: "web", "{}", page_text(&message)),
        Inbound::Log { level, message } => {
            log::log!(target: "web", log::Level::from(level), "{}", page_text(&message))
        }
    }
}

fn page_text(message: &str) -> String {
    message.chars().take(MAX_PAGE_TEXT).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file path of its own in the system's temporary folder, removed after the test.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "aiostreams-desktop-test-{}-{name}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self) -> PathBuf {
            self.0.join("layout")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn given(args: &[&str]) -> Args {
        args_from(args.iter().map(|a| a.to_string()))
    }

    #[test]
    fn tv_flag_turns_the_tv_layout_on() {
        assert!(given(&["--tv"]).tv);
        assert!(!given(&[]).tv);
        assert!(given(&["--devtools", "--tv", "--remote-debugging-port", "9222"]).tv);
    }

    #[test]
    fn tv_flag_leaves_the_other_arguments_alone() {
        let args = given(&[
            "--tv",
            "--remote-debugging-port",
            "9222",
            "--web",
            "http://x",
        ]);
        assert_eq!(args.debug_port, Some(9222));
        assert_eq!(args.web.as_deref(), Some("http://x"));
    }

    const TV: Layout = Layout {
        tv: true,
        width: TV_WIDTH,
    };
    const DESKTOP: Layout = Layout {
        tv: false,
        width: TV_WIDTH,
    };

    #[test]
    fn saved_layout_reads_back() {
        let scratch = Scratch::new("round-trip");
        for layout in [
            TV,
            DESKTOP,
            Layout {
                tv: true,
                width: 1920,
            },
            Layout {
                tv: false,
                width: 1600,
            },
        ] {
            save_tv_layout(&scratch.file(), layout);
            assert_eq!(load_tv_layout(&scratch.file()), Some(layout));
        }
    }

    #[test]
    fn no_saved_layout_follows_the_flag() {
        let scratch = Scratch::new("missing");
        assert_eq!(load_tv_layout(&scratch.file()), None);
        assert_eq!(starting_tv_layout(&scratch.file(), true), TV);
        assert_eq!(starting_tv_layout(&scratch.file(), false), DESKTOP);
    }

    #[test]
    fn saved_layout_wins_over_the_flag() {
        let scratch = Scratch::new("wins");
        save_tv_layout(&scratch.file(), DESKTOP);
        assert_eq!(starting_tv_layout(&scratch.file(), true), DESKTOP);
        save_tv_layout(&scratch.file(), TV);
        assert_eq!(starting_tv_layout(&scratch.file(), false), TV);
    }

    #[test]
    fn unreadable_saved_layout_follows_the_flag() {
        let scratch = Scratch::new("garbage");
        for text in ["", "yes", "TV", "1"] {
            std::fs::write(scratch.file(), text).unwrap();
            assert_eq!(load_tv_layout(&scratch.file()), None, "{text:?}");
            assert_eq!(starting_tv_layout(&scratch.file(), true), TV, "{text:?}");
        }
        // A trailing newline, as an editor leaves, still reads.
        std::fs::write(scratch.file(), "desktop\n").unwrap();
        assert_eq!(load_tv_layout(&scratch.file()), Some(DESKTOP));
    }

    #[test]
    fn layout_saved_before_widths_reads_at_the_tv_width() {
        let scratch = Scratch::new("old");
        std::fs::write(scratch.file(), "tv").unwrap();
        assert_eq!(load_tv_layout(&scratch.file()), Some(TV));
    }

    #[test]
    fn a_width_out_of_range_reads_as_the_tv_width() {
        let scratch = Scratch::new("width");
        for text in ["tv 100", "tv 99999", "tv wide", "tv -1280"] {
            std::fs::write(scratch.file(), text).unwrap();
            assert_eq!(load_tv_layout(&scratch.file()), Some(TV), "{text:?}");
        }
        assert_eq!(tv_width(1920), Some(1920));
        assert_eq!(tv_width(959), None);
        assert_eq!(tv_width(3841), None);
    }
}
