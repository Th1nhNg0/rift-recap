//! Independently authored Rust scoreboard with public Data Dragon artwork.
use crate::{App, i18n::{Lang, Msg}, model::{Match}, presentation::{Badge, plain}, render_assets::{Assets, Wanted}};
use anyhow::{Result, ensure};
use resvg::usvg::fontdb;
use serde_json::Value;
use std::{collections::HashSet, path::Path, sync::{Arc, OnceLock}};

pub(crate) const DIM: &str = "#9ca8bb";
pub(crate) const GOLD: &str = "#edc976";
pub(crate) const RED: &str = "#fa8297";

pub(crate) struct Canvas { pub svg: String, pub clips: usize }
pub(crate) fn escape(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).map(|c| match c {
        '&' => "&amp;".into(), '<' => "&lt;".into(), '>' => "&gt;".into(), '\'' => "&apos;".into(), '"' => "&quot;".into(), _ => c.to_string(),
    }).collect()
}
pub(crate) fn short(text: &str, count: usize) -> String {
    let text = plain(text);
    if text.chars().count() <= count { text } else { format!("{}…", text.chars().take(count.saturating_sub(1)).collect::<String>()) }
}
pub(crate) fn number(value: i64) -> String {
    let digits = value.to_string();
    digits.chars().rev().enumerate().flat_map(|(i, c)| if i > 0 && i % 3 == 0 { vec![',', c] } else { vec![c] }).collect::<Vec<_>>().into_iter().rev().collect()
}
pub(crate) fn team_name(team: u16, lang: Lang) -> String { lang.team(team).to_uppercase() }

pub(crate) fn tier_color(tier: &str) -> &'static str {
    match tier {
        "IRON" => "#9a9a9a", "BRONZE" => "#c58b65", "SILVER" => "#b8c4d0", "GOLD" => "#e6c06a",
        "PLATINUM" => "#4fc1b8", "EMERALD" => "#3fcf86", "DIAMOND" => "#7aa8ff", "MASTER" => "#c27cf0",
        "GRANDMASTER" => "#ff6b6b", "CHALLENGER" => "#f5d77a", _ => DIM,
    }
}

/// Scanning system fonts is slow; build the font database once per process.
pub(crate) fn fonts() -> Arc<fontdb::Database> {
    static FONTS: OnceLock<Arc<fontdb::Database>> = OnceLock::new();
    FONTS.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        // The embedded bytes already have static storage; don't duplicate the
        // complete fonts into long-lived heap buffers.
        db.load_font_source(fontdb::Source::Binary(Arc::new(&include_bytes!("../assets/fonts/BeVietnamPro-Regular.ttf")[..])));
        db.load_font_source(fontdb::Source::Binary(Arc::new(&include_bytes!("../assets/fonts/BeVietnamPro-Bold.ttf")[..])));
        Arc::new(db)
    }).clone()
}
/// The embedded face pair only: the race video needs no system font fallback, and skipping the
/// scan keeps its per-process memory and start-up cost down (the Docker image has no system fonts).
pub(crate) fn embedded_fonts() -> Arc<fontdb::Database> {
    static FONTS: OnceLock<Arc<fontdb::Database>> = OnceLock::new();
    FONTS.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_font_source(fontdb::Source::Binary(Arc::new(&include_bytes!("../assets/fonts/BeVietnamPro-Regular.ttf")[..])));
        db.load_font_source(fontdb::Source::Binary(Arc::new(&include_bytes!("../assets/fonts/BeVietnamPro-Bold.ttf")[..])));
        Arc::new(db)
    }).clone()
}

/// Images are laid out on a 1200 px grid and drawn at twice that size.
pub(crate) const SCALE: f32 = 2.0;
/// Lossy WebP quality: text and icons stay crisp and a full scoreboard is a few hundred KB.
const WEBP_QUALITY: f32 = 90.0;
pub(crate) const IMAGE_EXT: &str = "webp";
/// Draws the SVG, laid out on a `width` x `height` grid, at `SCALE` and encodes it as WebP.
pub(crate) fn rasterize(svg: String, width: u32, height: u32) -> Result<Vec<u8>> {
    let options = resvg::usvg::Options { font_family: "Be Vietnam Pro".into(), fontdb: fonts(), ..Default::default() };
    let tree = resvg::usvg::Tree::from_str(&svg, &options).map_err(|_| anyhow::anyhow!("Cannot parse image SVG"))?;
    // The parsed tree owns its image data; release the XML/base64 copy before
    // allocating the framebuffer and render/encoder working memory.
    drop(svg);
    let (w, h) = ((width as f32 * SCALE) as u32, (height as f32 * SCALE) as u32);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h).ok_or_else(|| anyhow::anyhow!("Cannot allocate image"))?;
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(SCALE, SCALE), &mut pixmap.as_mut());
    // Every image starts with an opaque background rectangle, so premultiplied and straight alpha agree.
    crate::webp_encode::rgba(pixmap.take(), w, h, WEBP_QUALITY)
}
// A delivery and two slash commands can otherwise allocate several full-size
// framebuffers at once. Queue only the CPU-heavy stage, keeping HTTP/gateway
// futures responsive and completing every admitted job without dropping work.
struct RenderWorkers { slots: Arc<tokio::sync::Semaphore> }
impl RenderWorkers {
    fn new(limit: usize) -> Self { Self { slots: Arc::new(tokio::sync::Semaphore::new(limit)) } }
    async fn run(&self, work: impl FnOnce() -> Result<Vec<u8>> + Send + 'static) -> Result<Vec<u8>> {
        let permit = self.slots.clone().acquire_owned().await.map_err(|_| anyhow::anyhow!("Image workers stopped"))?;
        tokio::task::spawn_blocking(move || {
            // A canceled async waiter cannot cancel already-running native work.
            // Keep admission owned by the closure until it actually finishes.
            let _permit = permit;
            work()
        }).await.map_err(|_| anyhow::anyhow!("Image worker failed"))?
    }
}
pub async fn blocking(work: impl FnOnce() -> Result<Vec<u8>> + Send + 'static) -> Result<Vec<u8>> {
    static WORKERS: OnceLock<RenderWorkers> = OnceLock::new();
    WORKERS.get_or_init(|| RenderWorkers::new(1)).run(work).await
}

/// Approximate advance width for laying out inline chips (Be Vietnam Pro averages ~0.6em).
pub(crate) fn text_width(text: &str, size: u32) -> u32 { (text.chars().count() as f64 * f64::from(size) * 0.6).round() as u32 }
/// Label, ink and pill fill for a scoreboard badge.
pub(crate) fn badge_style(badge: Badge, lang: Lang) -> (&'static str, &'static str, &'static str) {
    match badge {
        Badge::Mvp => (lang.t(Msg::ImgMvp), GOLD, "#3d3418"),
        Badge::Ace => (lang.t(Msg::ImgAce), "#c4cee4", "#2a3242"),
        Badge::Worst => (lang.t(Msg::ImgWorst), RED, "#3d1d26"),
    }
}
pub async fn image(app: &App, game: Match, lang: Lang, preview: bool) -> Result<Vec<u8>> {
    let directory = app.cfg.database.parent().unwrap_or(Path::new(".")).join("artwork");
    let art = Assets::load(Wanted::from_match(&game), directory).await;
    let timezone = app.cfg.reports.timezone.clone();
    blocking(move || crate::scoreboard::scoreboard(&game, lang, &timezone, preview, &art)).await
}

/// Preview lookup does not import matches or enqueue historical notifications.
pub async fn preview_match(app: &App, match_id: Option<&str>) -> Result<Match> {
    if let Some(game) = app.store.get_match(match_id)? { return Ok(game); }
    let mut roster = app.store.players()?;
    for player in &mut roster {
        if player.puuid.is_empty() { player.puuid = app.riot.resolve(player).await?.unwrap_or_default(); }
    }
    if let Some(id) = match_id {
        ensure!(id.starts_with(&format!("{}_", app.cfg.riot.platform.to_uppercase())) && id.len() < 80 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'), "Match ID must use this configured platform prefix");
        let value = app.riot.match_data(id).await?.ok_or_else(|| anyhow::anyhow!("Requested match is not published"))?;
        return Match::from_riot(id, &value, &roster);
    }
    let mut ids = HashSet::new();
    for player in roster.iter().filter(|p| !p.puuid.is_empty()) {
        match app.riot.history(&player.puuid, 1).await {
            Ok(history) => ids.extend(history),
            Err(error) => tracing::warn!(%error, "Preview history lookup unavailable for one account"),
        }
    }
    // Match IDs grow over time, so only the newest one needs downloading.
    let id = ids.into_iter().max_by_key(|id| (id.len(), id.clone())).ok_or_else(|| anyhow::anyhow!("No published recent match is available for this roster"))?;
    let value = app.riot.match_data(&id).await?.ok_or_else(|| anyhow::anyhow!("Newest match is not published yet"))?;
    Match::from_riot(&id, &value, &roster)
}

/// Real recent matches for a report preview, fetched read-only and never stored.
pub async fn preview_matches(app: &App, start: i64, limit: usize) -> Result<Vec<Match>> {
    let roster = app.store.players()?;
    let mut ids = std::collections::BTreeSet::new();
    for player in roster.iter().filter(|p| !p.puuid.is_empty()) {
        match app.riot.history_since(&player.puuid, start, 100).await {
            Ok(history) => ids.extend(history),
            Err(error) => tracing::warn!(%error, "Preview history lookup unavailable for one account"),
        }
    }
    // Match IDs grow over time: keep the newest when the window is larger than the limit.
    let ids: Vec<String> = ids.into_iter().rev().take(limit).collect();
    // Finished matches never change; a private file cache makes repeated previews instant.
    let cache = app.cfg.database.parent().unwrap_or(Path::new(".")).join("preview-matches");
    std::fs::create_dir_all(&cache)?;
    let total = ids.len();
    tracing::info!(matches = total, "Loading recent matches for a report preview (not tracked)");
    let mut matches = Vec::new();
    for (index, id) in ids.into_iter().enumerate() {
        let file = cache.join(format!("{id}.json"));
        let cached = std::fs::read(&file).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let fetched = match cached {
            Some(value) => Ok(Some(value)),
            None => {
                if index % 10 == 0 { tracing::info!(done = index, total, "Fetching preview matches from Riot"); }
                let value = app.riot.match_data(&id).await;
                if let Ok(Some(value)) = &value { let _ = std::fs::write(&file, value.to_string()); }
                value
            }
        };
        match fetched {
            Ok(Some(value)) => match Match::from_riot(&id, &value, &roster) {
                Ok(game) => matches.push(game),
                Err(error) => tracing::warn!(%error, "Skipping an unparseable preview match"),
            },
            Ok(None) => (),
            Err(error) => tracing::warn!(%error, "Preview match unavailable"),
        }
    }
    Ok(matches)
}

#[cfg(test)]
mod worker_tests {
    use super::*;
    use std::{future::{Future, poll_fn}, sync::atomic::{AtomicBool, AtomicUsize, Ordering}, task::Poll, time::Duration};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bounds_heavy_work_and_completes_every_job() {
        let workers = Arc::new(RenderWorkers::new(1));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = tokio::task::JoinSet::new();
        for id in 0..6u8 {
            let (workers, active, peak) = (workers.clone(), active.clone(), peak.clone());
            tasks.spawn(async move { workers.run(move || {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(10));
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(vec![id])
            }).await.unwrap() });
        }
        let mut completed = Vec::new();
        while let Some(result) = tasks.join_next().await { completed.push(result.unwrap()[0]); }
        completed.sort_unstable();
        assert_eq!(completed, [0, 1, 2, 3, 4, 5]);
        assert_eq!(peak.load(Ordering::SeqCst), 1);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_waiter_does_not_release_running_native_work() {
        let workers = Arc::new(RenderWorkers::new(1));
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let first_workers = workers.clone();
        let first = tokio::spawn(async move { first_workers.run(move || {
            let _ = started.send(());
            released.recv().unwrap();
            Ok(vec![1])
        }).await });
        ready.await.unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert_eq!(workers.slots.available_permits(), 0);
        let second_workers = workers.clone();
        let second = tokio::spawn(async move { second_workers.run(|| Ok(vec![2])).await });
        tokio::task::yield_now().await;
        assert!(!second.is_finished());
        release.send(()).unwrap();
        assert_eq!(tokio::time::timeout(Duration::from_secs(10), second).await.unwrap().unwrap().unwrap(), [2]);
        assert_eq!(workers.slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn cancelled_queued_work_does_not_run_or_lose_capacity() {
        let workers = Arc::new(RenderWorkers::new(1));
        let held = workers.slots.clone().acquire_owned().await.unwrap();
        let ran = Arc::new(AtomicBool::new(false));
        let (waiting_workers, flag) = (workers.clone(), ran.clone());
        let waiting = tokio::spawn(async move { waiting_workers.run(move || {
            flag.store(true, Ordering::SeqCst);
            Ok(vec![])
        }).await });
        tokio::task::yield_now().await;
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        drop(held);
        assert!(!ran.load(Ordering::SeqCst));
        assert_eq!(workers.run(|| Ok(vec![3])).await.unwrap(), [3]);
        assert_eq!(workers.slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn admission_is_fifo_and_errors_and_panics_release_capacity() {
        let workers = RenderWorkers::new(1);
        let held = workers.slots.clone().acquire_owned().await.unwrap();
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut queued = Vec::new();
        for id in 0..4u8 {
            let order = order.clone();
            let mut job = Box::pin(workers.run(move || {
                order.lock().unwrap().push(id);
                Ok(vec![id])
            }));
            // Poll exactly once while capacity is held, fixing admission order
            // without relying on nondeterministic async task scheduling.
            poll_fn(|cx| { assert!(job.as_mut().poll(cx).is_pending()); Poll::Ready(()) }).await;
            queued.push(job);
        }
        drop(held);
        tokio::time::timeout(Duration::from_secs(10), async {
            for job in queued { job.await.unwrap(); }
        }).await.unwrap();
        assert_eq!(*order.lock().unwrap(), [0, 1, 2, 3]);
        assert!(workers.run(|| anyhow::bail!("expected worker error")).await.is_err());
        assert!(workers.run(|| panic!("expected worker panic")).await.is_err());
        assert_eq!(workers.run(|| Ok(vec![4])).await.unwrap(), [4]);
        assert_eq!(workers.slots.available_permits(), 1);
    }
}
