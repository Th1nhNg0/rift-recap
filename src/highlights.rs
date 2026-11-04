//! Streaks, milestones and nemeses, derived from stored matches only: no table, no Riot call.
//! A game's highlights depend only on the games that ended before it, so they read the same
//! when the scoreboard is posted, when the AI commentary is added and on `/lastgame`.
use crate::{App, i18n::{Lang, Msg}, model::{Match, Participant}, presentation::plain};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Streak lengths worth announcing; beyond the last one, every fifth game.
const STREAKS: [i64; 4] = [3, 5, 7, 10];
/// A streak this long gets a line when it ends.
const STREAK_ENDED: i64 = 5;
/// Games on one champion worth announcing; beyond the last one, every hundredth.
const CHAMPION_GAMES: [i64; 3] = [25, 50, 100];
/// Earlier games in the same mode before a personal best means anything.
const BEST_SAMPLE: i64 = 20;
/// Games against a champion, and the highest win rate, before it counts as a nemesis after a loss.
const NEMESIS_GAMES: i64 = 5;
const NEMESIS_RATE: i64 = 30;
/// Lines per game, so a big duo game cannot flood the channel.
const MAX_LINES: usize = 4;

/// Ranked Solo/Duo and Flex: the only queues whose streaks count.
pub fn ranked(m: &Match) -> bool { matches!(m.queue, 420 | 440) }
/// Summoner's Rift with two teams of five, where a matchup against a champion means something.
/// Payloads stored before `mode` existed fall back to the queue.
pub fn rift(m: &Match) -> bool {
    if m.mode.is_empty() { matches!(m.queue, 400 | 420 | 430 | 440 | 490 | 700) } else { m.mode == "CLASSIC" }
}

/// One player's record against one enemy champion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Matchup { pub champion: String, pub games: i64, pub wins: i64 }
impl Matchup {
    pub fn losses(&self) -> i64 { self.games - self.wins }
    /// Win rate in percent.
    pub fn rate(&self) -> f64 { self.wins as f64 * 100.0 / self.games.max(1) as f64 }
    /// `Zed 1–7`: wins first, as on a scoreboard.
    pub fn label(&self) -> String { format!("{} {}–{}", self.champion, self.wins, self.losses()) }
}

/// Records against every enemy champion, from the player's side.
#[derive(Default)]
pub struct Matchups(BTreeMap<String, (i64, i64)>);
impl Matchups {
    /// Counts each enemy champion once per Summoner's Rift game; other modes and remakes are skipped.
    pub fn add(&mut self, m: &Match, p: &Participant) {
        if m.remake || !rift(m) { return; }
        for enemy in m.participants.iter().filter(|o| o.team != p.team && !o.champion.is_empty()) {
            let row = self.0.entry(enemy.champion.clone()).or_default();
            row.0 += 1; row.1 += i64::from(p.win);
        }
    }
    fn get(&self, champion: &str) -> Matchup {
        let (games, wins) = self.0.get(champion).copied().unwrap_or_default();
        Matchup { champion: champion.to_owned(), games, wins }
    }
    /// Every matchup with enough games, worst first: lowest win rate, then most games, then name.
    pub fn worst(&self, min: i64) -> Vec<Matchup> {
        let mut rows: Vec<Matchup> = self.0.keys().map(|c| self.get(c)).filter(|m| m.games >= min).collect();
        rows.sort_by(|a, b| a.rate().total_cmp(&b.rate()).then(b.games.cmp(&a.games)).then_with(|| a.champion.cmp(&b.champion)));
        rows
    }
    /// The champion this player has lost to most often, if they have a losing record against it.
    pub fn nemesis(&self, min: i64) -> Option<Matchup> { self.worst(min).into_iter().next().filter(|m| m.rate() < 50.0) }
    /// The champion this player beats most often, if they have a winning record against it.
    pub fn victim(&self, min: i64) -> Option<Matchup> {
        self.0.keys().map(|c| self.get(c)).filter(|m| m.games >= min && m.rate() > 50.0)
            .max_by(|a, b| a.rate().total_cmp(&b.rate()).then(a.games.cmp(&b.games)).then_with(|| b.champion.cmp(&a.champion)))
    }
}

/// A run of ranked wins or losses, fed one game at a time in end order.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Streak { pub win: bool, pub len: i64 }
impl Streak {
    /// Adds one result and returns the run it ended, if any.
    pub fn push(&mut self, win: bool) -> Option<Streak> {
        if self.len > 0 && self.win == win { self.len += 1; return None; }
        let ended = (self.len > 0).then_some(*self);
        *self = Streak { win, len: 1 };
        ended
    }
}

/// What one tracked player had done before the game being described.
#[derive(Default)]
struct Past {
    streak: Streak,
    /// Earlier games on the champion played in this game.
    champion_games: i64,
    quadra: bool,
    /// Earlier games in this game's mode, and the best kills and damage among them.
    mode_games: i64,
    kills: i64,
    damage: i64,
    matchups: Matchups,
}

#[derive(Debug, PartialEq)]
enum Kind {
    Promoted { tier: String, queue: u16 },
    Demoted { tier: String, queue: u16 },
    Penta { champion: String },
    Streak(Streak),
    StreakEnded(Streak),
    Nemesis(Matchup),
    FirstQuadra { champion: String },
    ChampionGames { champion: String, games: i64 },
    BestKills(i64),
    BestDamage(i64),
}
impl Kind {
    /// Lower is shown first when a game has more lines than fit.
    fn priority(&self) -> u8 {
        match self {
            Kind::Promoted { .. } | Kind::Demoted { .. } => 0, Kind::Penta { .. } => 1, Kind::Streak(_) => 2,
            Kind::StreakEnded(_) => 3, Kind::Nemesis(_) => 4, Kind::FirstQuadra { .. } => 5,
            Kind::ChampionGames { .. } => 6, Kind::BestKills(_) | Kind::BestDamage(_) => 7,
        }
    }
}
#[derive(Debug, PartialEq)]
struct Highlight { name: String, kind: Kind }

/// The localized highlight lines for one game, or an empty string when there are none or they are off.
pub fn text(app: &App, m: &Match, lang: Lang) -> Result<String> {
    if !app.cfg.highlights.enabled || m.remake || !m.participants.iter().any(|p| p.tracked) { return Ok(String::new()); }
    let mut past: BTreeMap<String, Past> = m.participants.iter().filter(|p| p.tracked).map(|p| (p.puuid.clone(), Past::default())).collect();
    app.store.visit_matches(0, m.ended_at, |old| if old.id != m.id && old.ended_at < m.ended_at { remember(&mut past, old, m) })?;
    Ok(render(&find(m, &past), lang))
}

/// Folds one earlier game into the history of every tracked player of `m` who played it.
fn remember(past: &mut BTreeMap<String, Past>, old: &Match, m: &Match) {
    if old.remake { return; }
    for p in &old.participants {
        let Some(entry) = past.get_mut(&p.puuid) else { continue };
        if ranked(old) { entry.streak.push(p.win); }
        let now = m.participants.iter().find(|q| q.puuid == p.puuid);
        if now.is_some_and(|q| q.champion == p.champion) { entry.champion_games += 1; }
        entry.quadra |= p.quadra_kills > 0 || p.penta_kills > 0;
        if !m.mode.is_empty() && old.mode == m.mode {
            entry.mode_games += 1;
            entry.kills = entry.kills.max(p.kills);
            entry.damage = entry.damage.max(p.damage);
        }
        entry.matchups.add(old, p);
    }
}

fn find(m: &Match, past: &BTreeMap<String, Past>) -> Vec<Highlight> {
    let mut out = Vec::new();
    for p in m.participants.iter().filter(|p| p.tracked) {
        let Some(before) = past.get(&p.puuid) else { continue };
        let mut add = |kind| out.push(Highlight { name: plain(&p.name), kind });
        if ranked(m) {
            // `rank_after` falls back to `rank_before` when unknown, so a different tier is a real move.
            if let (Some(a), Some(b)) = (&p.rank_before, &p.rank_after)
                && a.tier != b.tier && b.queue == m.queue
                && let (Some(x), Some(y)) = (a.absolute(), b.absolute()) {
                let tier = b.tier.clone();
                add(if y > x { Kind::Promoted { tier, queue: m.queue } } else { Kind::Demoted { tier, queue: m.queue } });
            }
            let mut streak = before.streak;
            if let Some(ended) = streak.push(p.win).filter(|s| s.len >= STREAK_ENDED) { add(Kind::StreakEnded(ended)); }
            if STREAKS.contains(&streak.len) || (streak.len > STREAKS[STREAKS.len() - 1] && streak.len % 5 == 0) { add(Kind::Streak(streak)); }
        }
        if p.penta_kills > 0 { add(Kind::Penta { champion: p.champion.clone() }); }
        else if p.quadra_kills > 0 && !before.quadra { add(Kind::FirstQuadra { champion: p.champion.clone() }); }
        if !p.win && rift(m) {
            let mut matchups = Matchups::default();
            for (champion, row) in &before.matchups.0 { matchups.0.insert(champion.clone(), *row); }
            matchups.add(m, p);
            let enemies: Vec<&str> = m.participants.iter().filter(|o| o.team != p.team).map(|o| o.champion.as_str()).collect();
            if let Some(worst) = matchups.worst(NEMESIS_GAMES).into_iter().find(|w| enemies.contains(&w.champion.as_str()) && w.rate() <= NEMESIS_RATE as f64) {
                add(Kind::Nemesis(worst));
            }
        }
        let games = before.champion_games + 1;
        if CHAMPION_GAMES.contains(&games) || (games > CHAMPION_GAMES[CHAMPION_GAMES.len() - 1] && games % 100 == 0) {
            add(Kind::ChampionGames { champion: p.champion.clone(), games });
        }
        if before.mode_games >= BEST_SAMPLE {
            if p.kills > before.kills { add(Kind::BestKills(p.kills)); }
            if p.damage > before.damage { add(Kind::BestDamage(p.damage)); }
        }
    }
    out.sort_by_key(|h| h.kind.priority());
    out.truncate(MAX_LINES);
    out
}

fn render(lines: &[Highlight], lang: Lang) -> String {
    lines.iter().map(|h| {
        let name = &h.name;
        match &h.kind {
            Kind::Promoted { tier, queue } => lang.f(Msg::HlPromoted, &[name, &lang.tier(tier), &lang.queue(*queue)]),
            Kind::Demoted { tier, queue } => lang.f(Msg::HlDemoted, &[name, &lang.tier(tier), &lang.queue(*queue)]),
            Kind::Penta { champion } => lang.f(Msg::HlPenta, &[name, champion]),
            Kind::Streak(s) => lang.f(if s.win { Msg::HlWinStreak } else { Msg::HlLossStreak }, &[name, &s.len]),
            Kind::StreakEnded(s) => lang.f(if s.win { Msg::HlWinStreakEnded } else { Msg::HlLossStreakEnded }, &[name, &s.len]),
            Kind::Nemesis(m) => lang.f(Msg::HlNemesis, &[name, &m.champion, &m.wins, &m.losses()]),
            Kind::FirstQuadra { champion } => lang.f(Msg::HlFirstQuadra, &[name, champion]),
            Kind::ChampionGames { champion, games } => lang.f(Msg::HlChampionGames, &[name, games, champion]),
            Kind::BestKills(n) => lang.f(Msg::HlBestKills, &[name, n]),
            Kind::BestDamage(n) => lang.f(Msg::HlBestDamage, &[name, n]),
        }
    }).collect::<Vec<_>>().join("\n")
}

/// Commentary or fallback text with the highlights after it, bounded to one Discord message.
pub fn join(text: &str, highlights: &str) -> String {
    let joined = match (text.is_empty(), highlights.is_empty()) {
        (_, true) => text.to_owned(),
        (true, false) => highlights.to_owned(),
        (false, false) => format!("{text}\n\n{highlights}"),
    };
    crate::presentation::limit(&joined, 2000)
}

/// Each tracked player's matchups over every stored game, for `/nemesis`.
pub fn matchups(app: &App) -> Result<BTreeMap<String, Matchups>> {
    let roster: Vec<String> = app.store.players()?.into_iter().map(|p| p.puuid).filter(|p| !p.is_empty()).collect();
    let mut out: BTreeMap<String, Matchups> = roster.into_iter().map(|p| (p, Matchups::default())).collect();
    app.store.visit_matches(0, crate::model::now(), |m| {
        for p in &m.participants { if let Some(row) = out.get_mut(&p.puuid) { row.add(m, p); } }
    })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Rank;

    fn game(id: usize, win: bool, champion: &str, enemy: &str) -> Match {
        let me = Participant { id: 1, puuid: "me".into(), name: "Hùng".into(), tracked: true, team: 100, win, champion: champion.into(), kills: 5, damage: 20_000, ..Default::default() };
        let them = Participant { id: 6, puuid: "them".into(), name: "Them".into(), team: 200, win: !win, champion: enemy.into(), ..Default::default() };
        Match { id: format!("VN2_{id}"), queue: 420, mode: "CLASSIC".into(), started_at: id as i64 * 3600, ended_at: id as i64 * 3600 + 1800, duration: 1800, participants: vec![me, them], ..Default::default() }
    }
    fn run(history: &[Match], m: &Match) -> Vec<Kind> {
        let mut past: BTreeMap<String, Past> = m.participants.iter().filter(|p| p.tracked).map(|p| (p.puuid.clone(), Past::default())).collect();
        for old in history { remember(&mut past, old, m); }
        find(m, &past).into_iter().map(|h| h.kind).collect()
    }

    #[test]
    fn streaks_are_announced_at_thresholds_and_when_they_end() {
        let wins: Vec<Match> = (0..4).map(|i| game(i, true, "Ahri", "Lux")).collect();
        assert_eq!(run(&wins[..2], &wins[2]), vec![Kind::Streak(Streak { win: true, len: 3 })]);
        assert!(run(&wins[..3], &wins[3]).is_empty());
        let five: Vec<Match> = (0..5).map(|i| game(i, true, "Ahri", "Lux")).collect();
        assert_eq!(run(&five, &game(5, false, "Ahri", "Lux")), vec![Kind::StreakEnded(Streak { win: true, len: 5 })]);
        // Normals and remakes neither extend nor break a ranked streak.
        let mut normal = game(2, false, "Ahri", "Lux"); normal.queue = 430;
        let mut remake = game(3, false, "Ahri", "Lux"); remake.remake = true;
        let history = [wins[0].clone(), wins[1].clone(), normal, remake];
        assert_eq!(run(&history, &game(4, true, "Ahri", "Lux")), vec![Kind::Streak(Streak { win: true, len: 3 })]);
        let losses: Vec<Match> = (0..15).map(|i| game(i, false, "Ahri", "Lux")).collect();
        let streaks = |n: usize| run(&losses[..n], &losses[n]).into_iter().filter(|k| matches!(k, Kind::Streak(_))).count();
        assert_eq!(streaks(10), 0, "11 is not a milestone");
        assert_eq!(streaks(14), 1, "past 10, every fifth game is");
    }

    #[test]
    fn a_loss_to_a_nemesis_is_called_out() {
        let mut history: Vec<Match> = (0..4).map(|i| game(i, i == 0, "Ahri", "Zed")).collect();
        // A different enemy champion does not count towards the Zed record.
        history.push(game(4, false, "Ahri", "Lux"));
        let kinds = run(&history, &game(5, false, "Ahri", "Zed"));
        assert!(kinds.contains(&Kind::Nemesis(Matchup { champion: "Zed".into(), games: 5, wins: 1 })), "{kinds:?}");
        // A win, or too few games, is no callout.
        assert!(!run(&history, &game(5, true, "Ahri", "Zed")).iter().any(|k| matches!(k, Kind::Nemesis(_))));
        assert!(!run(&history[..3], &game(5, false, "Ahri", "Zed")).iter().any(|k| matches!(k, Kind::Nemesis(_))));
    }

    #[test]
    fn milestones_and_records() {
        let mut m = game(30, true, "Ahri", "Lux");
        m.queue = 430;
        m.participants[0].penta_kills = 1;
        m.participants[0].quadra_kills = 1;
        m.participants[0].kills = 21;
        let history: Vec<Match> = (0..24).map(|i| { let mut g = game(i, true, "Ahri", "Lux"); g.queue = 430; g }).collect();
        let kinds = run(&history, &m);
        assert_eq!(kinds, vec![Kind::Penta { champion: "Ahri".into() }, Kind::ChampionGames { champion: "Ahri".into(), games: 25 }, Kind::BestKills(21)]);
        // ARAM numbers are never compared with Summoner's Rift ones.
        m.mode = "ARAM".into();
        assert!(!run(&history, &m).iter().any(|k| matches!(k, Kind::BestKills(_))));
    }

    #[test]
    fn tier_changes_follow_the_ranks() {
        let rank = |tier: &str, division: &str, lp| Some(Rank { queue: 420, tier: tier.into(), division: division.into(), lp });
        let mut m = game(1, true, "Ahri", "Lux");
        m.participants[0].rank_before = rank("SILVER", "I", 90);
        m.participants[0].rank_after = rank("GOLD", "IV", 0);
        assert_eq!(run(&[], &m), vec![Kind::Promoted { tier: "GOLD".into(), queue: 420 }]);
        m.participants[0].rank_before = rank("GOLD", "IV", 0);
        m.participants[0].rank_after = rank("SILVER", "I", 75);
        assert_eq!(run(&[], &m), vec![Kind::Demoted { tier: "SILVER".into(), queue: 420 }]);
    }

    #[test]
    fn lines_are_capped_and_localized() {
        let line = |kind| Highlight { name: "Lan".into(), kind };
        let text = render(&[line(Kind::Streak(Streak { win: true, len: 5 })), line(Kind::Nemesis(Matchup { champion: "Zed".into(), games: 8, wins: 1 }))], Lang::En);
        assert_eq!(text, "🔥 Lan is on a 5-game win streak\n😈 Lan lost to Zed again (1–7)");
        assert_eq!(join("", ""), "");
        assert_eq!(join("AI", "🔥"), "AI\n\n🔥");
        let mut m = game(9, true, "Ahri", "Lux");
        m.participants[0].penta_kills = 1;
        for i in 2..6 { let mut p = m.participants[0].clone(); p.puuid = format!("p{i}"); p.id = i; m.participants.push(p); }
        assert_eq!(run(&[], &m).len(), MAX_LINES);
    }

    #[test]
    fn nemesis_and_victim_need_a_record() {
        let mut matchups = Matchups::default();
        for i in 0..6 { matchups.add(&game(i, i < 1, "Ahri", "Zed"), &game(i, i < 1, "Ahri", "Zed").participants[0]); }
        for i in 0..6 { matchups.add(&game(i, i < 5, "Ahri", "Lux"), &game(i, i < 5, "Ahri", "Lux").participants[0]); }
        assert_eq!(matchups.nemesis(5).map(|m| m.label()), Some("Zed 1–5".into()));
        assert_eq!(matchups.victim(5).map(|m| m.label()), Some("Lux 5–1".into()));
        assert_eq!(matchups.nemesis(7), None);
        let mut aram = game(1, false, "Ahri", "Zed"); aram.mode = "ARAM".into();
        let mut none = Matchups::default();
        none.add(&aram, &aram.participants[0]);
        assert!(none.worst(1).is_empty());
    }
}

#[cfg(test)]
mod store_tests {
    use crate::{App, config::Config, i18n::Lang, model::{Match, Participant}, reports::{self, Period}, riot::Riot, store::Store};

    fn game(i: i64, win: bool) -> Match {
        let me = Participant { id: 1, puuid: "me".into(), name: "Lan".into(), tracked: true, team: 100, win, champion: "Ahri".into(), ..Default::default() };
        let them = Participant { id: 6, puuid: "them".into(), name: "Them".into(), team: 200, win: !win, champion: "Zed".into(), ..Default::default() };
        let end = 1_711_900_000 - (10 - i) * 3600;
        Match { id: format!("VN2_{i}"), queue: 420, mode: "CLASSIC".into(), started_at: end - 1800, ended_at: end, duration: 1800, participants: vec![me, them], ..Default::default() }
    }

    #[test]
    fn stored_history_drives_lines_and_monthly_honours() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        store.add_player(&crate::model::Player { game_name: "Lan".into(), tag_line: "VN2".into(), nickname: String::new(), puuid: "me".into(), baselined: true }).unwrap();
        // Two wins, then six losses to Zed.
        let games: Vec<Match> = (0..8).map(|i| game(i, i < 2)).collect();
        for m in &games { store.finish(m).unwrap(); }
        let mut cfg = Config::default();
        cfg.reports.timezone = "UTC".into();
        let app = App { cfg, store, riot: Riot::new("offline-fixture-not-a-key", "vn2", 1250).unwrap(), llm: None };
        assert_eq!(super::text(&app, &games[1], Lang::En).unwrap(), "");
        // Later games never change an earlier game's lines.
        assert_eq!(super::text(&app, &games[6], Lang::En).unwrap(), "📉 Lan has lost 5 in a row, someone check on them\n😈 Lan lost to Zed again (2–5)");
        assert_eq!(super::text(&app, &games[7], Lang::En).unwrap(), "😈 Lan lost to Zed again (2–6)");
        let report = reports::build_at(&app, Period::Monthly, 1_711_929_599).unwrap().unwrap();
        assert_eq!(report.players[0].best_streak, 2);
        assert_eq!(report.players[0].nemesis.as_ref().map(|m| m.label()), Some("Zed 2–6".into()));
        let mut off = app;
        off.cfg.highlights.enabled = false;
        assert_eq!(super::text(&off, &games[7], Lang::En).unwrap(), "");
    }
}
