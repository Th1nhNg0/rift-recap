//! Opt-in offline memory workload using a real SQLite history and production report path.
use crate::{App, config::Config, i18n::Lang, reports::{self, Period}, riot::Riot, store::Store};

#[test]
#[ignore = "opt-in release-mode SQLite report memory workload"]
fn memory_stored_reports() {
    let database = std::path::PathBuf::from(std::env::var_os("RIFT_RECAP_BENCH_DATABASE").expect("provide an isolated fixture database"));
    let end = 1_711_929_599; // 2024-03-31 23:59:59 UTC
    let mut cfg = Config { database: database.clone(), ..Config::default() };
    cfg.reports.timezone = "UTC".into();
    let app = App { cfg, store: Store::open(&database).unwrap(), riot: Riot::new("offline-fixture-not-a-key", "vn2", 1250).unwrap(), llm: None };
    for round in 0..2 {
        for period in [Period::Daily, Period::Weekly, Period::Monthly] {
            let report = reports::build_at(&app, period, end).unwrap().expect("fixture contains tracked games");
            assert!(report.matches > 0 && !report.players.is_empty());
            let output = std::path::PathBuf::from(std::env::var_os("RIFT_RECAP_BENCH_OUTPUT").expect("provide an isolated output directory"));
            std::fs::write(output.join(format!("{}-{round}.json", period.key())), serde_json::to_vec(&report).unwrap()).unwrap();
            for lang in Lang::ALL {
                std::fs::write(output.join(format!("{}-{}-{round}.txt", period.key(), lang.code())), report.text(lang)).unwrap();
            }
        }
    }
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        if let Some(line) = status.lines().find(|line| line.starts_with("VmRSS:")) { println!("AFTER_REPORT {line}"); }
    }
}

#[test]
fn streamed_reports_match_materialized_windows() {
    use crate::model::{Match, Participant, Player, Rank};
    use chrono::{Datelike, TimeZone};
    let end = 1_711_929_599;
    let store = Store::open(std::path::Path::new(":memory:")).unwrap();
    for i in 0..4 {
        store.add_player(&Player { game_name: format!("Player{i}"), tag_line: "TEST".into(), nickname: format!("Nguyễn {i}"), puuid: format!("puuid{i}"), baselined: true }).unwrap();
        for queue in [420, 440] {
            if i == 3 { continue; }
            for (at, lp) in [(end - if i == 1 { 5 } else { 32 } * 86400, 35), (end - 60, 40 + i), (end + 1, 99)] {
                store.snapshot(&format!("puuid{i}"), &Rank { queue, tier: "EMERALD".into(), division: "II".into(), lp }, at).unwrap();
            }
        }
    }
    let mut app = App { cfg: Config::default(), store, riot: Riot::new("offline-fixture-not-a-key", "vn2", 1250).unwrap(), llm: None };
    // Include exact endpoints, remakes, empty tracked sets, removed players,
    // both ranked queues, unranked games and champion ties.
    for (i, at) in [end - 32 * 86400, end - 30 * 86400, end - 7 * 86400, end - 86400, end - 60, end, end + 1].into_iter().enumerate() {
        let participants = (0..4).map(|p| Participant {
            id: p + 1, puuid: format!("puuid{p}"), name: format!("Old name{p}"), tracked: i != 2, champion: if i % 2 == 0 { "Ahri" } else { "Lux" }.into(), team: 100, win: i % 2 == 0,
            kills: 5, deaths: 3, assists: 9, damage: 12_000, cs: 150, vision: 25, ..Default::default()
        }).collect();
        app.store.finish(&Match { id: format!("TEST_{i}"), queue: [420, 440, 450][i % 3], started_at: at - 1800, ended_at: at, duration: 1800, remake: i == 1, participants, ..Default::default() }).unwrap();
    }
    app.store.remove_player("Player2#TEST").unwrap();
    for zone in ["UTC", "Asia/Ho_Chi_Minh", "America/New_York"] {
        app.cfg.reports.timezone = zone.into();
        let tz: chrono_tz::Tz = zone.parse().unwrap();
        let local_end = chrono::DateTime::from_timestamp(end, 0).unwrap().with_timezone(&tz);
        for period in [Period::Daily, Period::Weekly, Period::Monthly] {
            let start = match period {
                Period::Daily => end - 86400,
                Period::Weekly => end - 7 * 86400,
                Period::Monthly => tz.with_ymd_and_hms(local_end.year(), local_end.month(), 1, 0, 0, 0).earliest().unwrap().timestamp(),
            };
            let games = app.store.matches(start, end).unwrap();
            let expected = reports::compute(&app, period, start, end, &games).unwrap();
            let actual = reports::build_at(&app, period, end).unwrap();
            assert_eq!(serde_json::to_vec(&actual).unwrap(), serde_json::to_vec(&expected).unwrap(), "{zone} {}", period.key());
            if let (Some(actual), Some(expected)) = (actual, expected) {
                assert!(actual.players.iter().all(|p| p.name != "Nguyễn 2"));
                for lang in Lang::ALL { assert_eq!(actual.text(lang), expected.text(lang)); }
            }
        }
    }
    assert!(reports::build_at(&app, Period::Daily, end - 90 * 86400).unwrap().is_none());
}

fn sample(name: &str, games: i64, wins: i64, kills: i64, deaths: i64, assists: i64, damage: i64, cs: i64, vision: i64) -> crate::reports::PlayerLine {
    crate::reports::PlayerLine { name: name.into(), games, wins, kills, deaths, assists, damage, cs, vision, minutes: games as f64 * 30.0, ..Default::default() }
}
fn report_of(period: Period, players: Vec<crate::reports::PlayerLine>) -> crate::reports::Report {
    crate::reports::Report { period, start: 1_711_670_400, end: 1_711_929_599, timezone: "UTC".into(), matches: 1, players, partial_lp: false }
}

#[test]
fn mvp_is_a_monthly_award() {
    let players = vec![sample("Solo", 20, 12, 120, 60, 160, 500_000, 5_000, 400)];
    assert!(report_of(Period::Monthly, players.clone()).mvp().is_some());
    assert!(report_of(Period::Weekly, players.clone()).mvp().is_none());
    assert!(report_of(Period::Daily, players).mvp().is_none());
}

#[test]
fn mvp_rating_is_relative_and_normalised() {
    // Identical players: every ratio is 1.0 before scaling, so every rating is exactly 1.00.
    let alpha = sample("Alpha", 40, 20, 200, 100, 300, 800_000, 12_000, 600);
    let twins = report_of(Period::Monthly, vec![alpha.clone(), alpha.clone()]);
    let ratings = twins.mvp_ratings();
    assert_eq!(ratings.len(), 2);
    assert!(ratings.iter().all(|(_, rating)| (rating - 1.0).abs() < 1e-9));
    assert_eq!(twins.mvp().unwrap().player, 0, "an exact tie goes to the earlier name");
    // The same line at 1.5x volume keeps every per-game ratio, so games break the tie.
    let busier = sample("Bravo", 60, 30, 300, 150, 450, 1_200_000, 18_000, 900);
    assert_eq!(report_of(Period::Monthly, vec![alpha.clone(), busier]).mvp().unwrap().player, 1);
    // Double output on every metric: the MVP climbs above 1.00 and the other player drops below it.
    let carry = sample("Carry", 40, 20, 400, 100, 600, 1_600_000, 24_000, 1_200);
    let lopsided = report_of(Period::Monthly, vec![alpha, carry]);
    let mvp = lopsided.mvp().unwrap();
    assert_eq!(mvp.player, 1);
    assert!(mvp.rating > 1.0 && mvp.rating < 2.0, "rating {}", mvp.rating);
    assert!(lopsided.mvp_ratings().iter().any(|(index, rating)| *index == 0 && *rating < 1.0));
}

#[test]
fn mvp_requires_a_real_sample() {
    let regular = sample("Regular", 100, 50, 500, 500, 500, 1_500_000, 15_000, 1_000);
    let perfect = sample("Perfect", 19, 19, 380, 19, 380, 1_900_000, 19_000, 1_900);
    let grinder = sample("Grinder", 21, 10, 105, 126, 126, 315_000, 3_150, 210);
    let report = report_of(Period::Monthly, vec![regular, perfect, grinder]);
    let eligible: Vec<usize> = report.mvp_ratings().iter().map(|(index, _)| *index).collect();
    assert_eq!(eligible, vec![0, 2], "19 games is under a fifth of the busiest player's 100");
    // The floor is ten games even when the 20% share rule would allow fewer.
    let quiet = report_of(Period::Monthly, vec![sample("Quiet", 12, 6, 60, 60, 60, 180_000, 1_800, 120), sample("Rookie", 9, 9, 180, 9, 180, 900_000, 9_000, 900)]);
    assert_eq!(quiet.mvp_ratings().len(), 1);
    assert_eq!(quiet.mvp().unwrap().player, 0);
}

#[test]
fn scheduled_report_periods_cascade() {
    use chrono::NaiveDate;
    // Ordinary weekday (Wednesday): daily only.
    let wednesday = NaiveDate::from_ymd_opt(2024, 3, 27).unwrap();
    assert_eq!(reports::periods(wednesday, true), vec![Period::Daily]);
    assert_eq!(reports::periods(wednesday, false), vec![Period::Daily]);

    // Sunday (not month end): weekly also includes daily.
    let sunday = NaiveDate::from_ymd_opt(2024, 3, 24).unwrap();
    assert_eq!(reports::periods(sunday, true), vec![Period::Daily, Period::Weekly]);
    assert_eq!(reports::periods(sunday, false), vec![Period::Daily, Period::Weekly]);

    // Month end on a weekday (Tuesday): monthly includes daily and weekly.
    let month_end_tuesday = NaiveDate::from_ymd_opt(2024, 4, 30).unwrap();
    assert_eq!(reports::periods(month_end_tuesday, true), vec![Period::Daily, Period::Weekly, Period::Monthly]);
    assert_eq!(reports::periods(month_end_tuesday, false), vec![Period::Daily]);

    // Month end on Sunday: monthly includes daily and weekly.
    let month_end_sunday = NaiveDate::from_ymd_opt(2024, 3, 31).unwrap();
    assert_eq!(reports::periods(month_end_sunday, true), vec![Period::Daily, Period::Weekly, Period::Monthly]);
    assert_eq!(reports::periods(month_end_sunday, false), vec![Period::Daily, Period::Weekly]);
}
