//! Quietly archive older games of the roster: the normalized match, the raw Riot payload and the
//! timeline. Nothing is announced: imported games get no outbox entry, no commentary and no post.
//! Safe to stop and run again: a game already fully archived costs no request.

use crate::{App, model::{Match, now}, riot::Riot};
use anyhow::{Result, bail};
use chrono::{NaiveDate, TimeZone};
use std::{collections::{BTreeSet, HashSet}, future::Future, time::Duration};
use tokio::time::sleep;
use tracing::{info, warn};

#[derive(Default, Debug)]
pub struct Summary { pub found: usize, pub imported: usize, pub completed: usize, pub skipped: usize, pub timelines: usize }

/// Midnight of `date` (YYYY-MM-DD) in the reports timezone, as Unix seconds.
pub fn day_start(app: &App, date: &str) -> Result<i64> {
    let day = NaiveDate::parse_from_str(date, "%Y-%m-%d").map_err(|_| anyhow::anyhow!("--since must be YYYY-MM-DD"))?;
    let timezone: chrono_tz::Tz = app.cfg.reports.timezone.parse().unwrap_or(chrono_tz::UTC);
    timezone.from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap_or_default()).earliest().map(|t| t.timestamp())
        .ok_or_else(|| anyhow::anyhow!("--since is not a valid local date"))
}

/// Archive every roster game that ended between `since` and the start of the live catch-up window.
/// Newer games are left to the running tracker, so a game it is about to announce is never taken from it.
pub async fn run(app: &App, riot: &Riot, since: i64) -> Result<Summary> {
    let until = now() - i64::from(app.cfg.riot.catch_up_hours) * 3600;
    let roster = app.store.players()?;
    let current: HashSet<&str> = roster.iter().map(|p| p.puuid.as_str()).filter(|p| !p.is_empty()).collect();
    let mut ids = BTreeSet::new();
    for player in roster.iter().filter(|p| !p.puuid.is_empty()) {
        let before = ids.len();
        for offset in (0..).step_by(100) {
            let page = patient("match history", || riot.history_between(&player.puuid, since, until, offset)).await?;
            let full = page.len() == 100;
            ids.extend(page);
            if !full { break; }
        }
        info!(games = ids.len() - before, "Listed one player's games");
    }
    let mut summary = Summary { found: ids.len(), ..Default::default() };
    info!(games = summary.found, "Archiving games");
    for (index, id) in ids.iter().enumerate() {
        let (stored, raw, timeline) = app.store.archived(id)?;
        if !stored || !raw {
            let Some(value) = patient("match", || riot.match_data(id)).await? else { summary.skipped += 1; continue };
            let mut game = match Match::from_riot(id, &value, &roster) {
                Ok(game) => game,
                Err(error) => { warn!(%error, "Match normalization failed; skipping it"); summary.skipped += 1; continue; }
            };
            for part in &mut game.participants { part.tracked &= current.contains(part.puuid.as_str()); }
            // The ID can be listed by a player's history and still end outside the window by a few seconds.
            if !game.participants.iter().any(|p| p.tracked) || game.ended_at >= until { summary.skipped += 1; continue; }
            if app.store.import(&game, &value)? { summary.imported += 1 } else { summary.completed += 1 }
        }
        if !timeline && app.store.archived(id)?.0
            && let Some(value) = patient("timeline", || riot.timeline(id)).await? {
            app.store.save_timeline(id, &value)?;
            summary.timelines += 1;
        }
        if (index + 1) % 25 == 0 { info!(done = index + 1, total = summary.found, imported = summary.imported, timelines = summary.timelines, "Backfill progress"); }
    }
    Ok(summary)
}

/// Retry a Riot call through rate limits and outages, waiting longer each time.
async fn patient<T, F, Fut>(what: &str, mut call: F) -> Result<T> where F: FnMut() -> Fut, Fut: Future<Output = Result<T>> {
    for attempt in 1..=8u64 {
        match call().await {
            Ok(value) => return Ok(value),
            Err(error) => { warn!(%error, what, attempt, "Riot request failed; waiting before retrying"); sleep(Duration::from_secs(15 * attempt)).await; }
        }
    }
    bail!("Riot kept failing for {what}; run the backfill again later to resume")
}
