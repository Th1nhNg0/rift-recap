//! Daily, weekly and monthly reports: one data model, rendered as a card image or as text.
use crate::{App, i18n::{Lang, Msg}, model::{Match, Notice, Rank, now}, presentation::plain};
use anyhow::Result;
use chrono::{Datelike, TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use std::{collections::{BTreeMap, HashSet}, fmt::Write, sync::Arc, time::Duration};
use tokio::time::sleep;
use tracing::warn;

pub async fn refresh_ranks(app: &App) -> Result<usize> {
    let mut succeeded = 0;
    for player in app.store.players()?.iter().filter(|p| !p.puuid.is_empty()) {
        match app.riot.ranks(&player.puuid).await {
            Ok(ranks) => {
                // A periodic refresh must not advance the rank-checked marker: it says nothing about
                // which games are accounted for, and doing so would discard a correct pre-game rank
                // for the first match after a restart, which is the LP bug this marker exists to fix.
                for rank in ranks { app.store.snapshot(&player.puuid, &rank, now())?; }
                succeeded += 1;
            }
            Err(error) => warn!(%error, "Rank refresh failed for one account"),
        }
    }
    Ok(succeeded)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Period { Daily, Weekly, Monthly }
impl Period {
    pub fn key(self) -> &'static str { match self { Self::Daily => "daily", Self::Weekly => "weekly", Self::Monthly => "monthly" } }
    pub fn parse(text: &str) -> Option<Self> { [Self::Daily, Self::Weekly, Self::Monthly].into_iter().find(|p| p.key().eq_ignore_ascii_case(text.trim())) }
    /// Rolling day/week windows; the monthly window is the local calendar month so far.
    fn start(self, end: i64, timezone: chrono_tz::Tz) -> i64 {
        match self {
            Self::Daily => end - 86400,
            Self::Weekly => end - 7 * 86400,
            Self::Monthly => {
                let local = chrono::DateTime::from_timestamp(end, 0).unwrap_or_default().with_timezone(&timezone);
                timezone.with_ymd_and_hms(local.year(), local.month(), 1, 0, 0, 0).earliest().map_or(end - 30 * 86400, |t| t.timestamp())
            }
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Report {
    pub period: Period,
    pub start: i64,
    pub end: i64,
    pub timezone: String,
    /// Distinct non-remake matches with at least one tracked player.
    pub matches: i64,
    /// Ordered for display: highest known LP change first, then most games.
    pub players: Vec<PlayerLine>,
    /// Some LP change starts at the first snapshot inside the window rather than before it.
    pub partial_lp: bool,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct PlayerLine {
    pub name: String,
    pub games: i64, pub wins: i64, pub kills: i64, pub deaths: i64, pub assists: i64,
    pub cs: i64, pub damage: i64, pub vision: i64, pub minutes: f64,
    /// Most played first, at most three.
    pub champions: Vec<(String, i64)>,
    pub ranks: Vec<RankLine>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct RankLine { pub queue: u16, pub games: i64, pub wins: i64, pub delta: Option<i64>, pub rank: Option<Rank> }
pub struct Award { pub title: Msg, pub player: usize, pub value: String, pub negative: bool }

/// Monthly MVP: the highest group-relative rating among players with a real sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mvp { pub player: usize, pub rating: f64 }
/// A player needs this many games, and at least a fifth of the busiest player's games, to be MVP.
const MVP_MIN_GAMES: i64 = 10;
const MVP_SHARE: i64 = 5;

impl PlayerLine {
    /// Sum of known per-queue LP changes; `None` when no change is known.
    pub fn lp(&self) -> Option<i64> { self.ranks.iter().filter_map(|r| r.delta).reduce(|a, b| a + b) }
    pub fn kda(&self) -> f64 { (self.kills + self.assists) as f64 / self.deaths.max(1) as f64 }
    pub fn cs_per_min(&self) -> f64 { self.cs as f64 / self.minutes.max(1.0) }
    pub fn damage_per_game(&self) -> f64 { self.damage as f64 / self.games.max(1) as f64 }
    pub fn vision_per_min(&self) -> f64 { self.vision as f64 / self.minutes.max(1.0) }
    pub fn win_rate(&self) -> f64 { self.wins as f64 / self.games.max(1) as f64 * 100.0 }
}
impl Report {
    pub fn title(&self, lang: Lang) -> String {
        match self.period {
            Period::Daily => lang.t(Msg::DailyReport).into(),
            Period::Weekly => lang.t(Msg::WeeklyReport).into(),
            Period::Monthly => lang.f(Msg::MonthlyReport, &[&self.month(lang)]),
        }
    }
    pub fn range(&self, lang: Lang) -> String {
        let format = lang.t(Msg::ReportRangeFormat);
        format!("{}  –  {}", self.local(self.start).format(format), self.local(self.end).format(format))
    }
    fn local(&self, ts: i64) -> chrono::DateTime<chrono_tz::Tz> {
        chrono::DateTime::from_timestamp(ts, 0).unwrap_or_default().with_timezone(&self.timezone.parse().unwrap_or(chrono_tz::UTC))
    }
    pub fn net_lp(&self) -> Option<i64> { self.players.iter().filter_map(PlayerLine::lp).reduce(|a, b| a + b) }
    /// (gained, lost) over all known per-queue changes.
    pub fn gained_lost(&self) -> (i64, i64) {
        self.players.iter().flat_map(|p| p.ranks.iter().filter_map(|r| r.delta))
            .fold((0, 0), |(g, l), d| if d >= 0 { (g + d, l) } else { (g, l - d) })
    }
    /// Weekly and monthly honours, each with a minimum sample size.
    pub fn awards(&self, lang: Lang) -> Vec<Award> {
        if self.period == Period::Daily { return Vec::new(); }
        type Metric = fn(&PlayerLine) -> f64;
        let one = |x: f64| format!("{x:.1}");
        let rows: [(Msg, i64, Metric, bool); 6] = [
            (Msg::AwKills, 1, |p| p.kills as f64, false),
            (Msg::AwDeaths, 1, |p| p.deaths as f64, true),
            (Msg::AwDamage, 2, PlayerLine::damage_per_game, false),
            (Msg::AwFarm, 2, PlayerLine::cs_per_min, false),
            (Msg::AwVision, 2, PlayerLine::vision_per_min, false),
            (Msg::AwWinRate, 3, PlayerLine::win_rate, false),
        ];
        rows.into_iter().filter_map(|(title, minimum, metric, negative)| {
            let (index, p) = self.players.iter().enumerate().filter(|(_, p)| p.games >= minimum).max_by(|a, b| metric(a.1).total_cmp(&metric(b.1)))?;
            let value = match title {
                Msg::AwKills => lang.f(Msg::FmtKills, &[&p.kills]),
                Msg::AwDeaths => lang.f(Msg::FmtDeaths, &[&p.deaths]),
                Msg::AwDamage => lang.f(Msg::DamagePerGame, &[&format!("{:.0}", p.damage_per_game())]),
                Msg::AwFarm => lang.f(Msg::CsPerMin, &[&one(p.cs_per_min())]),
                Msg::AwVision => lang.f(Msg::VisionPerMin, &[&format!("{:.2}", p.vision_per_min())]),
                _ => lang.f(Msg::FmtWinRate, &[&format!("{:.0}", p.win_rate()), &p.games]),
            };
            Some(Award { title, player: index, value, negative })
        }).collect()
    }
    /// The report's month, in the language's month format.
    pub fn month(&self, lang: Lang) -> String { self.local(self.end).format(lang.t(Msg::MonthFormat)).to_string() }
    /// Rating of every eligible player, normalised so the eligible group averages 1.00: each of
    /// win rate, KDA, damage per game, CS per minute and vision per minute is divided by the group
    /// average, the ratios are averaged, and the result is scaled by the group's mean.
    pub fn mvp_ratings(&self) -> Vec<(usize, f64)> {
        if self.period != Period::Monthly { return Vec::new(); }
        let busiest = self.players.iter().map(|p| p.games).max().unwrap_or(0);
        let eligible: Vec<usize> = self.players.iter().enumerate()
            .filter(|(_, p)| p.games >= MVP_MIN_GAMES && p.games * MVP_SHARE >= busiest)
            .map(|(index, _)| index).collect();
        if eligible.is_empty() { return Vec::new(); }
        type Metric = fn(&PlayerLine) -> f64;
        let metrics: [Metric; 5] = [PlayerLine::win_rate, PlayerLine::kda, PlayerLine::damage_per_game, PlayerLine::cs_per_min, PlayerLine::vision_per_min];
        let means: Vec<f64> = metrics.iter().map(|metric| eligible.iter().map(|&i| metric(&self.players[i])).sum::<f64>() / eligible.len() as f64).collect();
        let raw = |p: &PlayerLine| {
            let (total, count) = metrics.iter().zip(&means).filter(|(_, mean)| **mean > 0.0)
                .fold((0.0, 0), |(total, count), (metric, mean)| (total + metric(p) / mean, count + 1));
            if count == 0 { 0.0 } else { total / f64::from(count) }
        };
        let average = eligible.iter().map(|&i| raw(&self.players[i])).sum::<f64>() / eligible.len() as f64;
        let scale = if average > 0.0 { 1.0 / average } else { 1.0 };
        eligible.into_iter().map(|i| (i, raw(&self.players[i]) * scale)).collect()
    }
    /// The monthly MVP, if anyone played enough. Ties go to more games, then the earlier name.
    pub fn mvp(&self) -> Option<Mvp> {
        self.mvp_ratings().into_iter().max_by(|a, b| a.1.total_cmp(&b.1)
            .then_with(|| self.players[a.0].games.cmp(&self.players[b.0].games))
            .then_with(|| self.players[b.0].name.cmp(&self.players[a.0].name))
            .then_with(|| b.0.cmp(&a.0)))
            .map(|(player, rating)| Mvp { player, rating })
    }
    pub fn text(&self, lang: Lang) -> String {
        let mut out = format!("{}\n{} ({})\n{}\n", self.title(lang), self.range(lang), self.timezone, lang.f(Msg::DistinctMatches, &[&self.matches]));
        if let Some(mvp) = self.mvp() {
            let p = &self.players[mvp.player];
            let rating = format!("{:.2}", mvp.rating);
            let _ = writeln!(out, "{}: {} · {}", lang.f(Msg::MvpMonth, &[&self.month(lang)]), plain(&p.name), lang.f(Msg::MvpLine, &[&rating, &p.games, &p.wins]));
        }
        let awards = self.awards(lang);
        if !awards.is_empty() {
            let _ = writeln!(out, "\n{}", lang.t(Msg::HonoursSection));
            for award in awards { let _ = writeln!(out, "{}: {} · {}", lang.t(award.title), plain(&self.players[award.player].name), award.value); }
        }
        let _ = writeln!(out, "\n{}", lang.t(Msg::RankSection));
        for p in &self.players {
            for r in &p.ranks {
                let delta = r.delta.map(|d| format!("{d:+} LP")).unwrap_or_else(|| lang.t(Msg::LpUnknown).into());
                let rank = r.rank.as_ref().map(|r| lang.rank(r)).unwrap_or_else(|| lang.t(Msg::RankNoData).into());
                let _ = writeln!(out, "{} · {} · {} · {delta} · {rank}", plain(&p.name), lang.queue(r.queue), lang.f(Msg::GamesWins, &[&r.games, &r.wins]));
            }
        }
        if self.partial_lp { let _ = writeln!(out, "{}", lang.t(Msg::PartialLp)); }
        let _ = writeln!(out, "\n{}", lang.t(Msg::StatsSection));
        for p in &self.players {
            let champion = p.champions.first().map(|(s, n)| format!(" · {} ×{n}", plain(s))).unwrap_or_default();
            let _ = writeln!(out, "{} · {} ({:.0}%) · {:.2} KDA · {} · {} · {}{champion}{}",
                plain(&p.name), lang.f(Msg::GamesWins, &[&p.games, &p.wins]), p.win_rate(), p.kda(),
                lang.f(Msg::CsPerMin, &[&format!("{:.1}", p.cs_per_min())]),
                lang.f(Msg::DamagePerGame, &[&format!("{:.0}", p.damage_per_game())]),
                lang.f(Msg::VisionPerMin, &[&format!("{:.2}", p.vision_per_min())]),
                if p.games < 20 { format!(" · {}", lang.t(Msg::SmallSample)) } else { String::new() });
        }
        out
    }
}

/// Report for the window ending now, from stored matches.
pub fn build(app: &App, period: Period) -> Result<Option<Report>> {
    build_at(app, period, now())
}
pub(crate) fn build_at(app: &App, period: Period, end: i64) -> Result<Option<Report>> {
    let start = period.start(end, app.cfg.reports.timezone.parse()?);
    compute_scanned(app, period, start, end, |visit| app.store.visit_matches(start, end, visit))
}
pub fn window(app: &App, period: Period) -> Result<(i64, i64)> {
    let end = now();
    Ok((period.start(end, app.cfg.reports.timezone.parse()?), end))
}
/// Aggregates the supplied matches; ranks and roster come from the store.
pub fn compute(app: &App, period: Period, start: i64, end: i64, matches: &[Match]) -> Result<Option<Report>> {
    compute_scanned(app, period, start, end, |visit| {
        for game in matches { visit(game); }
        Ok(())
    })
}
// Aggregate during the scan, then enrich ranks only after the scan has returned
// and released its database statement and mutex. Stored reports and previews
// share the exact same accumulator and ordering.
fn compute_scanned(app: &App, period: Period, start: i64, end: i64, scan: impl FnOnce(&mut dyn FnMut(&Match)) -> Result<()>) -> Result<Option<Report>> {
    let roster = app.store.players()?;
    let current: HashSet<&str> = roster.iter().map(|p| p.puuid.as_str()).filter(|p| !p.is_empty()).collect();
    let mut stats: BTreeMap<String, PlayerLine> = BTreeMap::new();
    let mut champions: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
    let mut records: BTreeMap<(String, u16), (i64, i64)> = BTreeMap::new();
    let mut games = 0;
    scan(&mut |m| {
        if m.remake || m.ended_at <= start || m.ended_at > end { return; }
        let tracked: Vec<_> = m.participants.iter().filter(|p| p.tracked && current.contains(p.puuid.as_str())).collect();
        if tracked.is_empty() { return; }
        games += 1;
        for p in &tracked {
            let a = stats.entry(p.puuid.clone()).or_default();
            a.name = roster.iter().find(|q| q.puuid == p.puuid).map_or_else(|| p.name.clone(), |q| q.name().to_owned());
            a.games += 1; a.wins += i64::from(p.win);
            a.kills += p.kills; a.deaths += p.deaths; a.assists += p.assists;
            a.cs += p.cs; a.damage += p.damage; a.vision += p.vision;
            a.minutes += (m.duration as f64 / 60.0).max(1.0);
            *champions.entry(p.puuid.clone()).or_default().entry(p.champion.clone()).or_default() += 1;
            if matches!(m.queue, 420 | 440) {
                let row = records.entry((p.puuid.clone(), m.queue)).or_default(); row.0 += 1; row.1 += i64::from(p.win);
            }
        }
    })?;
    if games == 0 { return Ok(None); }
    let mut partial = false;
    for ((puuid, queue), (count, wins)) in records {
        let current = app.store.rank_at(&puuid, queue, end)?;
        let mut base = app.store.rank_at(&puuid, queue, start)?;
        if base.is_none() { base = app.store.first_rank(&puuid, queue, start, end)?; partial = true; }
        // A single observation cannot show a change; unknown is not zero.
        let delta = base.as_ref().zip(current.as_ref()).filter(|(b, c)| b.at != c.at)
            .and_then(|(b, c)| Some(c.rank.absolute()? - b.rank.absolute()?));
        if let Some(line) = stats.get_mut(&puuid) { line.ranks.push(RankLine { queue, games: count, wins, delta, rank: current.map(|s| s.rank) }); }
    }
    for (puuid, line) in &mut stats {
        let mut played: Vec<(String, i64)> = champions.remove(puuid).unwrap_or_default().into_iter().collect();
        played.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        played.truncate(3);
        line.champions = played;
    }
    let mut players: Vec<PlayerLine> = stats.into_values().collect();
    players.sort_by(|a, b| b.lp().unwrap_or(i64::MIN).cmp(&a.lp().unwrap_or(i64::MIN)).then(b.games.cmp(&a.games)).then_with(|| a.name.cmp(&b.name)));
    Ok(Some(Report { period, start, end, timezone: app.cfg.reports.timezone.clone(), matches: games, players, partial_lp: partial }))
}

/// Which reports a local date gets: weekly on Sunday, monthly on the month's last day
/// (replacing the daily one), both when the month ends on a Sunday.
fn periods(date: chrono::NaiveDate, monthly: bool) -> Vec<Period> {
    let sunday = date.weekday() == chrono::Weekday::Sun;
    let month_end = monthly && date.succ_opt().is_some_and(|next| next.day() == 1);
    match (sunday, month_end) {
        (true, true) => vec![Period::Weekly, Period::Monthly],
        (true, false) => vec![Period::Weekly],
        (false, true) => vec![Period::Monthly],
        (false, false) => vec![Period::Daily],
    }
}
pub fn notice(report: Report, lang: Lang) -> Notice { Notice::Report { text: report.text(lang), report: Some(report) } }

pub async fn scheduler(app: Arc<App>) -> Result<()> {
    let timezone = app.cfg.reports.timezone.parse::<chrono_tz::Tz>()?;
    loop {
        let result: Result<()> = async {
            let time = chrono::Utc::now().with_timezone(&timezone);
            let enabled = app.store.setting("rank_enabled")?.map(|s| s == "1").unwrap_or(app.cfg.reports.enabled);
            let due = (time.hour(), time.minute()) >= (app.cfg.reports.hour, app.cfg.reports.minute);
            let day = time.date_naive().to_string();
            if enabled && due && app.channel(true)? != 0 && app.store.setting("last_report")?.as_deref() != Some(&day) {
                refresh_ranks(&app).await?;
                let mut queued = Vec::new();
                for period in periods(time.date_naive(), app.cfg.reports.monthly) {
                    if let Some(report) = build(&app, period)? { queued.push((format!("report/{day}/{}", period.key()), notice(report, app.cfg.language))); }
                }
                app.store.schedule_reports(&day, &queued)?;
            }
            Ok(())
        }.await;
        if let Err(error) = result { warn!(%error, "Report scheduler failed; retrying next minute"); }
        sleep(Duration::from_secs(30)).await;
    }
}
