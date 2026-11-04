//! Smoke tests for the image renderers: every layout must produce a decodable WebP at the expected size.
use crate::{i18n::Lang, model::{Match, Participant, Rank}, render_assets::Assets, report_card, reports::{Period, PlayerLine, RankLine, Report}, scoreboard};
use std::collections::BTreeMap;

fn rank(tier: &str, division: &str, lp: i64) -> Option<Rank> { Some(Rank { queue: 440, tier: tier.into(), division: division.into(), lp }) }

fn player(id: u64, team: u16, win: bool, tracked: bool) -> Participant {
    let roles = ["TOP", "JUNGLE", "MIDDLE", "BOTTOM", "UTILITY"];
    Participant {
        id, name: format!("Player {id} with a rather long name"), champion: "Ahri".into(), team, role: roles[(id as usize - 1) % 5].into(), tracked, win,
        kills: id as i64, deaths: (id as i64 * 3) % 7, assists: 5 + id as i64, cs: 120 + id as i64 * 11, damage: 9_000 + id as i64 * 2_500, gold: 8_000 + id as i64 * 900,
        vision: 10 + id as i64, level: 9 + id as i64 % 9, damage_taken: 12_000 + id as i64 * 1_800, items: vec![3157, 3089, 0, 3020, 0, 0, 3340],
        rank_after: if tracked { rank("DIAMOND", "II", 40) } else { None }, lp_delta: tracked.then_some(if win { 19 } else { -17 }), ..Default::default()
    }
}

fn game(teams: u16, per_team: u64, remake: bool) -> Match {
    let mut participants = Vec::new();
    for t in 0..teams {
        for i in 0..per_team {
            let team = if teams == 2 { 100 + t * 100 } else { t + 1 };
            participants.push(player(t as u64 * per_team + i + 1, team, t == 0, i == 0));
        }
    }
    let objectives = (teams == 2).then(|| [("100".to_owned(), BTreeMap::from([("dragon".to_owned(), 3), ("tower".to_owned(), 7)])), ("200".to_owned(), BTreeMap::from([("baron".to_owned(), 1)]))].into()).unwrap_or_default();
    Match { id: "TEST_1".into(), queue: 440, started_at: 1_700_000_000, ended_at: 1_700_001_800, duration: 1800, remake, participants, objectives, ..Default::default() }
}

/// Decodes the WebP and checks it is a 2x render of the 1200 px layout.
fn assert_webp(bytes: &[u8]) {
    let image = webp::Decoder::new(bytes).decode().expect("output is a valid WebP");
    assert_eq!(image.width(), 2400);
    assert!(image.height() > 600 && image.height() < 12_000, "unexpected height {}", image.height());
    assert!(bytes.len() < 3 * 1024 * 1024, "image is {} bytes", bytes.len());
}

#[test]
fn scoreboard_renders_every_layout() {
    let art = Assets::default();
    for (name, teams, per_team, remake) in [("5v5", 2, 5, false), ("remake", 2, 5, true), ("arena", 8, 2, false), ("one team", 1, 5, false)] {
        for lang in Lang::ALL {
            let image = scoreboard::scoreboard(&game(teams, per_team, remake), lang, "Asia/Ho_Chi_Minh", true, &art).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_webp(&image);
        }
    }
}

#[test]
fn scoreboard_rejects_bad_input() {
    let art = Assets::default();
    assert!(scoreboard::scoreboard(&game(2, 5, false), Lang::En, "Not/AZone", false, &art).is_err());
    assert!(scoreboard::scoreboard(&Match { participants: Vec::new(), ..game(2, 5, false) }, Lang::En, "UTC", false, &art).is_err());
}

fn line(name: &str, games: i64, wins: i64, delta: Option<i64>) -> PlayerLine {
    PlayerLine {
        name: name.into(), games, wins, kills: games * 7, deaths: games * 5, assists: games * 9, cs: games * 180, damage: games * 21_000, vision: games * 30, minutes: games as f64 * 30.0,
        champions: vec![("Ahri".into(), games)], ranks: vec![RankLine { queue: 440, games, wins, delta, rank: rank("EMERALD", "I", 35) }],
        ..Default::default()
    }
}

#[test]
fn report_card_renders_every_block() {
    let art = Assets::default();
    let full = Report { period: Period::Weekly, start: 1_700_000_000, end: 1_700_600_000, timezone: "UTC".into(), matches: 12, partial_lp: false,
        players: vec![line("Climber", 10, 7, Some(64)), line("Flat", 4, 2, Some(0)), line("Dropper", 8, 3, Some(-41)), line("Unknown", 3, 1, None)] };
    let small = Report { players: vec![line("Only", 1, 1, Some(12))], ..full.clone() };
    for report in [full, small] {
        for lang in Lang::ALL {
            assert_webp(&report_card::card(&report, lang, &art).expect("report card"));
        }
    }
}

#[test]
fn monthly_report_adds_an_mvp_band() {
    let art = Assets::default();
    let players = vec![line("Climber", 40, 24, Some(64)), line("Dropper", 30, 12, Some(-41))];
    let weekly = Report { period: Period::Weekly, start: 1_700_000_000, end: 1_700_600_000, timezone: "UTC".into(), matches: 12, partial_lp: false, players };
    let monthly = Report { period: Period::Monthly, ..weekly.clone() };
    let image = |report: &Report| { let bytes = report_card::card(report, Lang::En, &art).expect("report card"); assert_webp(&bytes); webp::Decoder::new(&bytes).decode().expect("valid WebP").height() };
    assert_eq!(image(&monthly), image(&weekly) + 2 * 96, "the monthly card grows by the 96 px MVP band");
    // Streak, nemesis and victim honours fill a third row of tiles.
    let mut players = monthly.players.clone();
    players[0].best_streak = 6;
    players[1].nemesis = Some(crate::highlights::Matchup { champion: "Zed".into(), games: 5, wins: 1 });
    players[0].victim = Some(crate::highlights::Matchup { champion: "Lux".into(), games: 4, wins: 4 });
    let honours = Report { players, ..monthly.clone() };
    assert_eq!(honours.awards(Lang::En).len(), 9);
    assert_eq!(image(&honours), image(&monthly) + 2 * 64, "nine honours take three 64 px rows");
}


#[test]
fn header_pattern_preserves_clipped_pixels() {
    let art = memory_art(crate::render_assets::Wanted { champions: ["Ahri".into()].into(), splash: Some("Ahri".into()), ..Default::default() });
    for data in [art.image("champion:Ahri").unwrap(), art.image("splash:Ahri").unwrap()] {
        for (height, offset) in [(30, -24), (49, -34)] {
            let base = "<svg xmlns='http://www.w3.org/2000/svg' width='240' height='80'><rect width='240' height='80' fill='#010a13'/>";
            let original = format!("{base}<defs><clipPath id='hero'><rect width='240' height='{height}'/></clipPath></defs><image href='{data}' x='0' y='{offset}' width='240' height='142' preserveAspectRatio='xMidYMin slice' clip-path='url(#hero)'/></svg>");
            let pattern = format!("{base}<defs><pattern id='hero' patternUnits='userSpaceOnUse' width='240' height='{height}'><image href='{data}' x='0' y='{offset}' width='240' height='142' preserveAspectRatio='xMidYMin slice'/></pattern></defs><rect width='240' height='{height}' fill='url(#hero)'/></svg>");
            assert_eq!(crate::render::rasterize(pattern, 240, 80).unwrap(), crate::render::rasterize(original, 240, 80).unwrap());
        }
    }
}

// Opt-in offline memory workloads. They use the real renderer, full-size output,
// PNG icons with transparency and a shipped JPEG; no credentials or network.
fn memory_art(wanted: crate::render_assets::Wanted) -> Assets {
    use base64::{Engine, engine::general_purpose::STANDARD};
    fn icon(size: u32, seed: u8) -> String {
        let mut pixels = Vec::with_capacity((size * size * 4) as usize);
        for y in 0..size { for x in 0..size {
            pixels.extend_from_slice(&[(x as u8).wrapping_mul(3).wrapping_add(seed), (y as u8).wrapping_mul(5), (x ^ y) as u8, if x < 3 || y < 3 { 0 } else { 255 }]);
        } }
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, size, size);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().unwrap().write_image_data(&pixels).unwrap();
        }
        format!("data:image/png;base64,{}", STANDARD.encode(bytes))
    }
    let mut art = Assets::default();
    for (i, champion) in wanted.champions.into_iter().enumerate() {
        art.images.insert(format!("champion:{champion}"), icon(120, i as u8));
    }
    for item in wanted.items { art.images.insert(format!("item:{item}"), icon(64, item as u8)); }
    for spell in wanted.spells { art.images.insert(format!("spell:{spell}"), icon(64, spell as u8)); }
    for rune in wanted.runes { art.images.insert(format!("rune:{rune}"), icon(64, rune as u8)); }
    for tier in wanted.tiers { art.images.insert(format!("tier:{tier}"), icon(64, 42)); }
    if wanted.objectives {
        for kind in crate::render_assets::OBJECTIVES { for team in [100, 200] {
            art.images.insert(format!("objective:{kind}-{team}"), icon(64, team as u8));
        } }
    }
    if let Some(champion) = wanted.splash {
        art.images.insert(format!("splash:{champion}"), format!("data:image/jpeg;base64,{}", STANDARD.encode(include_bytes!("../docs/social-preview.jpg"))));
    }
    art
}

fn memory_scoreboard_fixture(teams: u16, per_team: u64) -> (Match, Assets) {
    let mut game = game(teams, per_team, false);
    for (i, p) in game.participants.iter_mut().enumerate() {
        p.champion = format!("Champion{i}");
        p.spells = vec![4, 7]; p.runes = vec![8005, 8100];
        p.rank_after = rank(if i % 2 == 0 { "DIAMOND" } else { "EMERALD" }, "II", 40);
        p.name = format!("Nguyễn Người chơi {i}");
    }
    let art = memory_art(crate::render_assets::Wanted::from_match(&game));
    (game, art)
}
fn memory_scoreboard(teams: u16, per_team: u64, case: &str) {
    let (game, art) = memory_scoreboard_fixture(teams, per_team);
    memory_rounds(case, |lang| scoreboard::scoreboard(&game, lang, "Asia/Ho_Chi_Minh", true, &art).unwrap());
}

fn memory_report_fixture(count: usize) -> (Report, Assets) {
    let players = (0..count).map(|i| {
        let mut p = line(&format!("Nguyễn Người chơi {i}"), 10 + i as i64, 5 + i as i64 / 2, if i % 3 == 2 { None } else { Some((i as i64 - count as i64 / 2) * 7) });
        p.champions = vec![(format!("Champion{}", i % 10), p.games)];
        p
    }).collect();
    let report = Report { period: Period::Monthly, start: 1_700_000_000, end: 1_700_600_000, timezone: "Asia/Ho_Chi_Minh".into(), matches: 55, partial_lp: true, players };
    let wanted = crate::render_assets::Wanted {
        champions: report.players.iter().flat_map(|p| p.champions.iter().map(|(c, _)| c.clone())).collect(),
        splash: report.players.iter().max_by_key(|p| p.lp().unwrap_or(i64::MIN)).and_then(|p| p.champions.first()).map(|(c, _)| c.clone()),
        tiers: ["EMERALD".to_owned()].into(), ..Default::default()
    };
    let art = memory_art(wanted);
    (report, art)
}
fn memory_report(count: usize, case: &str) {
    let (report, art) = memory_report_fixture(count);
    memory_rounds(case, |lang| report_card::card(&report, lang, &art).unwrap());
}

fn memory_rounds(case: &str, mut render: impl FnMut(Lang) -> Vec<u8>) {
    for round in 0..2 {
        for lang in Lang::ALL {
            let image = render(lang);
            assert!(image.starts_with(b"RIFF") && &image[8..12] == b"WEBP");
            if let Some(directory) = std::env::var_os("RIFT_RECAP_BENCH_OUTPUT") {
                std::fs::write(std::path::Path::new(&directory).join(format!("{case}-{}-{round}.webp", lang.code())), &image).unwrap();
            }
        }
    }
    // Optional Linux diagnostic after render buffers have been dropped; not a
    // production code path and never used to change renderer behaviour.
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        if let Some(line) = status.lines().find(|line| line.starts_with("VmRSS:")) { println!("AFTER_RENDER {line}"); }
    }
}

#[test]
#[ignore = "opt-in release-mode memory workload"]
fn memory_scoreboard_standard() { memory_scoreboard(2, 5, "standard"); }
#[test]
#[ignore = "opt-in release-mode memory workload"]
fn memory_scoreboard_arena() { memory_scoreboard(8, 2, "arena"); }
#[test]
#[ignore = "opt-in release-mode memory workload"]
fn memory_report_small() { memory_report(4, "small"); }
#[test]
#[ignore = "opt-in release-mode memory workload"]
fn memory_report_large() { memory_report(32, "large"); }

// Two command requests can overlap the single delivery worker. These bursts
// exercise the actual async blocking-worker boundary, not a custom render pool.
fn memory_burst(cases: [&'static str; 3]) {
    type Work = Box<dyn FnOnce() -> anyhow::Result<Vec<u8>> + Send>;
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        for lang in Lang::ALL {
            let gate = std::sync::Arc::new(tokio::sync::Barrier::new(3));
            let mut running = tokio::task::JoinSet::new();
            for (index, case) in cases.into_iter().enumerate() {
                let work: Work = match case {
                    "standard" | "arena" => {
                        let (game, art) = if case == "standard" { memory_scoreboard_fixture(2, 5) } else { memory_scoreboard_fixture(8, 2) };
                        Box::new(move || scoreboard::scoreboard(&game, lang, "Asia/Ho_Chi_Minh", true, &art))
                    }
                    "small" | "large" => {
                        let (report, art) = memory_report_fixture(if case == "small" { 4 } else { 32 });
                        Box::new(move || report_card::card(&report, lang, &art))
                    }
                    _ => panic!("unknown fixture"),
                };
                let gate = gate.clone();
                running.spawn(async move {
                    gate.wait().await;
                    let start = std::time::Instant::now();
                    let image = crate::render::blocking(work).await.unwrap();
                    println!("JOB_LATENCY_S {:.6}", start.elapsed().as_secs_f64());
                    let directory = std::env::var_os("RIFT_RECAP_BENCH_OUTPUT").unwrap();
                    std::fs::write(std::path::Path::new(&directory).join(format!("{index}-{case}-{}-0.webp", lang.code())), image).unwrap();
                });
            }
            while let Some(result) = running.join_next().await { result.unwrap(); }
        }
        // Measure retained pages while the real runtime and its blocking-worker
        // threads are still alive, as they are in the running service.
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let rss = status.lines().find(|line| line.starts_with("VmRSS:")).unwrap();
        println!("AFTER_BURST {rss}");
    });
}
#[test]
#[ignore = "opt-in realistic async render burst memory workload"]
fn memory_burst_squad() { memory_burst(["standard", "standard", "small"]); }
#[test]
#[ignore = "opt-in realistic async render burst memory workload"]
fn memory_burst_large_report() { memory_burst(["standard", "standard", "large"]); }
#[test]
#[ignore = "opt-in realistic async render burst memory workload"]
fn memory_burst_arena() { memory_burst(["arena", "arena", "arena"]); }
