//! Keeping the program up to date without reinstalling it.
//!
//! The program is built with a copy of beleaf inside it. Each time it starts, it quietly downloads the newest copy of the
//! same files from the website and keeps them in its own folder (app data/live). From then on those are what the window
//! shows: at once if the download finished before the window opened, otherwise after "Restart now" or the next start.
//! The address the window uses never changes, so drawings, brushes and settings stay where they are.
//! A newly installed program starts clean (its own copy is newer than anything kept), and with no internet the last
//! copy that was fetched (or the built-in one) is used.

use std::borrow::Cow;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::utils::assets::{AssetKey, AssetsIter, CspHash};
use tauri::{App, Assets, Manager, Runtime};

const SITE: &str = "https://cozycoded.github.io/beleaf/";
const LIST: &str = include_str!("../live.txt");

static DIR: OnceLock<PathBuf> = OnceLock::new();
/// whether the window has already loaded the page (then a newer download only counts from the next load)
static SERVED: AtomicBool = AtomicBool::new(false);
static STATUS: Mutex<&'static str> = Mutex::new("checking");

pub fn status() -> String {
    STATUS.lock().map(|s| s.to_string()).unwrap_or_default()
}

fn files() -> Vec<&'static str> {
    LIST.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect()
}

fn kept(rel: &str) -> Option<Vec<u8>> {
    std::fs::read(DIR.get()?.join(rel)).ok()
}

/// Stand-in while the built-in files are moved into LiveAssets.
pub struct Empty;
impl<R: Runtime> Assets<R> for Empty {
    fn get(&self, _key: &AssetKey) -> Option<Cow<'_, [u8]>> {
        None
    }
    fn iter(&self) -> Box<AssetsIter<'_>> {
        Box::new(std::iter::empty())
    }
    fn csp_hashes(&self, _html_path: &AssetKey) -> Box<dyn Iterator<Item = CspHash<'_>> + '_> {
        Box::new(std::iter::empty())
    }
}

/// The built-in files, with the kept (downloaded) copies used in their place.
pub struct LiveAssets<R: Runtime> {
    pub inner: Box<dyn Assets<R>>,
}

impl<R: Runtime> LiveAssets<R> {
    fn built_in(&self, rel: &str) -> Vec<u8> {
        self.inner.get(&AssetKey::from(rel)).map(|c| c.into_owned()).unwrap_or_default()
    }
}

impl<R: Runtime> Assets<R> for LiveAssets<R> {
    fn setup(&self, app: &App<R>) {
        self.inner.setup(app);
        let list = files();
        let Ok(dir) = app.path().app_data_dir().map(|d| d.join("live")) else {
            *STATUS.lock().unwrap() = "offline";
            return;
        };
        // which install the kept files belong to: a new install throws away what an older one fetched
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.built_in(list[0]).hash(&mut h);
        let stamp = format!("{:016x}", h.finish());
        if std::fs::read_to_string(dir.join("built-in.txt")).ok().as_deref() != Some(stamp.as_str()) {
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(dir.join("built-in.txt"), &stamp);
        }
        let _ = DIR.set(dir);
        let now: Vec<Vec<u8>> = list.iter().map(|f| kept(f).unwrap_or_else(|| self.built_in(f))).collect();
        std::thread::spawn(move || {
            let s = fetch(&list, &now);
            if let Ok(mut st) = STATUS.lock() {
                *st = s;
            }
        });
    }

    fn get(&self, key: &AssetKey) -> Option<Cow<'_, [u8]>> {
        let key_str: &str = key.as_ref();
        let rel = key_str.trim_start_matches('/');
        if rel.ends_with(".html") {
            SERVED.store(true, Ordering::SeqCst);
        }
        if files().contains(&rel) {
            if let Some(b) = kept(rel) {
                return Some(Cow::Owned(b));
            }
        }
        self.inner.get(key)
    }

    fn iter(&self) -> Box<AssetsIter<'_>> {
        self.inner.iter()
    }

    fn csp_hashes(&self, html_path: &AssetKey) -> Box<dyn Iterator<Item = CspHash<'_>> + '_> {
        self.inner.csp_hashes(html_path)
    }
}

/// a download that stopped halfway is never used
fn whole(rel: &str, b: &[u8]) -> bool {
    if rel.ends_with(".html") {
        b.len() > 100_000 && b[b.len().saturating_sub(64)..].windows(7).any(|w| w == b"</html>")
    } else if rel.ends_with(".wasm") {
        b.starts_with(b"\0asm")
    } else {
        !b.is_empty()
    }
}

fn fetch(list: &[&str], now: &[Vec<u8>]) -> &'static str {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(90)).build();
    let mut got = Vec::new();
    for rel in list {
        // ?t= skips the website's short-term cache
        let Ok(r) = agent.get(&format!("{SITE}{rel}?t={t}")).call() else { return "offline" };
        let mut b = Vec::new();
        if r.into_reader().take(96 << 20).read_to_end(&mut b).is_err() || !whole(rel, &b) {
            return "offline";
        }
        got.push(b);
    }
    if got.iter().zip(now).all(|(a, b)| a == b) {
        return "same";
    }
    let Some(dir) = DIR.get() else { return "offline" };
    // every file is downloaded before any is replaced, so the page and its engine always match
    for (rel, b) in list.iter().zip(&got) {
        let p = dir.join(rel);
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let part = p.with_extension("part");
        if std::fs::write(&part, b).is_err() || std::fs::rename(&part, &p).is_err() {
            return "offline";
        }
    }
    if SERVED.load(Ordering::SeqCst) { "updated" } else { "same" }
}
