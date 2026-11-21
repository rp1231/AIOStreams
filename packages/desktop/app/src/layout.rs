//! The TV layout: the saved layout, the current layout, and the page's copy of it.
//!
//! The app owns the layout. The page reads it from a `<meta name="aiostreams-layout">` the app
//! writes into each `index.html` it serves, so a reload always lays out in the current layout.

use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The page width a TV lays the TV layout out at, and the one it starts at here.
pub const TV_WIDTH: u32 = 1280;
/// The page widths the TV layout may be laid out at. The page offers its own few within them.
const WIDTHS: RangeInclusive<u32> = 960..=3840;

/// The TV layout or the desktop layout, and the page width the TV layout is laid out at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub tv: bool,
    pub width: u32,
}

impl Layout {
    /// The layout, or none for a width the TV layout can't take.
    pub fn new(tv: bool, width: u32) -> Option<Layout> {
        WIDTHS.contains(&width).then_some(Layout { tv, width })
    }

    /// `tv 1280` or `desktop 1600`, as the saved layout and the page's meta write it.
    fn text(self) -> String {
        format!("{} {}", if self.tv { "tv" } else { "desktop" }, self.width)
    }

    /// A saved layout, which a width missing or out of range leaves at the TV width.
    fn parse(text: &str) -> Option<Layout> {
        let mut words = text.split_whitespace();
        let tv = match words.next()? {
            "tv" => true,
            "desktop" => false,
            _ => return None,
        };
        let width = words
            .next()
            .and_then(|w| w.parse().ok())
            .and_then(|w| Layout::new(tv, w))
            .map_or(TV_WIDTH, |l| l.width);
        Some(Layout { tv, width })
    }
}

/// The layout the app shows now, shared with whatever serves the page, and the saved layout
/// behind it.
#[derive(Clone)]
pub struct CurrentLayout {
    file: PathBuf,
    now: Arc<Mutex<Layout>>,
}

impl CurrentLayout {
    /// Starts in the saved layout, or the desktop layout without one. `--tv` forces the TV
    /// layout for this launch, at the saved TV size, without saving it.
    pub fn start(file: PathBuf, tv_flag: bool) -> CurrentLayout {
        let saved = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| Layout::parse(&text));
        let mut now = saved.unwrap_or(Layout {
            tv: false,
            width: TV_WIDTH,
        });
        now.tv |= tv_flag;
        CurrentLayout {
            file,
            now: Arc::new(Mutex::new(now)),
        }
    }

    pub fn get(&self) -> Layout {
        *self.now.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Switches to `layout` and saves it. True when it changes the layout itself, which the
    /// page only takes by laying itself out again, rather than only the TV size.
    pub fn switch(&self, layout: Layout) -> bool {
        let before = std::mem::replace(
            &mut *self.now.lock().unwrap_or_else(|e| e.into_inner()),
            layout,
        );
        if let Err(e) = std::fs::write(&self.file, layout.text()) {
            log::warn!("could not save the layout to {}: {e}", self.file.display());
        }
        before.tv != layout.tv
    }

    /// The web app's `index.html` with the current layout in its head.
    pub fn page(&self, html: Vec<u8>) -> Vec<u8> {
        let meta = format!(
            r#"<meta name="aiostreams-layout" content="{}">"#,
            self.get().text()
        );
        let at = find(&html, b"<head>").map_or(0, |i| i + b"<head>".len());
        let mut page = Vec::with_capacity(html.len() + meta.len());
        page.extend_from_slice(&html[..at]);
        page.extend_from_slice(meta.as_bytes());
        page.extend_from_slice(&html[at..]);
        page
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of its own under the system's temporary one, removed afterwards.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Scratch {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("aiostreams-layout-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn file(&self) -> PathBuf {
            self.0.join("layout")
        }

        fn saved(&self, text: &str) -> &Scratch {
            std::fs::write(self.file(), text).unwrap();
            self
        }

        fn start(&self, tv_flag: bool) -> CurrentLayout {
            CurrentLayout::start(self.file(), tv_flag)
        }

        fn read(&self) -> String {
            std::fs::read_to_string(self.file()).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn layout(tv: bool, width: u32) -> Layout {
        Layout::new(tv, width).unwrap()
    }

    #[test]
    fn starts_in_the_desktop_layout_without_a_saved_one() {
        assert_eq!(Scratch::new().start(false).get(), layout(false, TV_WIDTH));
    }

    #[test]
    fn starts_in_the_saved_layout() {
        let scratch = Scratch::new();
        assert_eq!(
            scratch.saved("tv 1600").start(false).get(),
            layout(true, 1600)
        );
        assert_eq!(
            scratch.saved("desktop 1920").start(false).get(),
            layout(false, 1920)
        );
    }

    #[test]
    fn tv_flag_forces_the_tv_layout_at_the_saved_size_without_saving() {
        let scratch = Scratch::new();
        assert_eq!(scratch.start(true).get(), layout(true, TV_WIDTH));
        assert!(!scratch.file().exists());
        assert_eq!(
            scratch.saved("desktop 1600").start(true).get(),
            layout(true, 1600)
        );
        assert_eq!(scratch.read(), "desktop 1600");
    }

    #[test]
    fn a_switch_changes_the_current_and_saved_layout() {
        let scratch = Scratch::new();
        let current = scratch.saved("desktop 1280").start(true);
        assert!(current.switch(layout(false, 1280)));
        assert_eq!(current.get(), layout(false, 1280));
        assert_eq!(scratch.read(), "desktop 1280");
        // A plain launch opens what was switched to; --tv still forces the TV layout.
        assert_eq!(scratch.start(false).get(), layout(false, 1280));
        assert_eq!(scratch.start(true).get(), layout(true, 1280));
    }

    #[test]
    fn a_switch_tells_a_new_layout_from_a_new_size() {
        let current = Scratch::new().start(false);
        assert!(!current.switch(layout(false, 1600)));
        assert!(current.switch(layout(true, 1600)));
        assert!(!current.switch(layout(true, 1920)));
        assert_eq!(current.get(), layout(true, 1920));
    }

    #[test]
    fn widths_outside_the_range_are_refused() {
        assert_eq!(Layout::new(true, 959), None);
        assert_eq!(Layout::new(true, 3841), None);
        assert_eq!(Layout::new(true, 0), None);
        assert!(Layout::new(true, 960).is_some());
        assert!(Layout::new(false, 3840).is_some());
    }

    #[test]
    fn an_unreadable_saved_layout_is_ignored() {
        let scratch = Scratch::new();
        for text in ["", "wide", "TV", "1280 tv"] {
            assert_eq!(
                scratch.saved(text).start(false).get(),
                layout(false, TV_WIDTH),
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_saved_width_missing_or_out_of_range_reads_as_the_tv_width() {
        let scratch = Scratch::new();
        for text in ["tv", "tv 100", "tv 99999", "tv wide", "tv -1280"] {
            assert_eq!(
                scratch.saved(text).start(false).get(),
                layout(true, TV_WIDTH),
                "{text:?}"
            );
        }
    }

    #[test]
    fn the_page_carries_the_current_layout() {
        let current = Scratch::new().saved("tv 1600").start(false);
        let html = b"<!doctype html><html><head><title>x</title></head></html>".to_vec();
        let page = String::from_utf8(current.page(html.clone())).unwrap();
        assert_eq!(
            page,
            r#"<!doctype html><html><head><meta name="aiostreams-layout" content="tv 1600"><title>x</title></head></html>"#
        );
        current.switch(layout(false, 1600));
        let page = String::from_utf8(current.page(html)).unwrap();
        assert!(page.contains(r#"content="desktop 1600""#), "{page}");
    }

    #[test]
    fn a_page_without_a_head_still_carries_the_layout() {
        let current = Scratch::new().start(true);
        let page = String::from_utf8(current.page(b"<p>hi</p>".to_vec())).unwrap();
        assert_eq!(
            page,
            r#"<meta name="aiostreams-layout" content="tv 1280"><p>hi</p>"#
        );
    }
}
