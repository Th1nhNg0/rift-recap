//! Monthly LP race video: the themed line chart rendered with resvg and encoded by ffmpeg.
//!
//! Frames are built as SVG at 1280x720, rasterised in parallel with resvg, and piped as raw
//! RGBA to an ffmpeg child process. The LP series follow the same rules as the monthly card:
//! only queues played in the window, each measured from its last snapshot at or before the
//! window start (or the first one inside it).
use crate::{App, i18n::{Lang, Msg}, model::{Rank, Snapshot}, render::{Canvas, escape}, reports::{self, Period}, theme::{BG, BLUE, CREAM, GOLD, GOLD_DIM, RED, RULE, SOFT, TEAL}};
use anyhow::{Context, Result, ensure};
use rayon::prelude::*;
use std::{collections::{BTreeSet, HashMap}, fmt::Write, io::Write as _, path::Path, process::{Command, Stdio}};

pub(crate) const W: u32 = 1280;
pub(crate) const H: u32 = 720;
/// Raster scale: 1.0 is 1280x720, 1.5 is 1920x1080.
const SCALE: f32 = 1.0;
const FPS: u32 = 30;
const SECONDS: u32 = 60;
const FRAMES: usize = (FPS * SECONDS) as usize;
const HOLD: usize = 66;
const FINAL: usize = 150;
const SAMPLES: usize = 3000;
const SMOOTH_SECS: f64 = 4.0 * 3600.0;
const PLOT_LEFT: f32 = 90.0;
const PLOT_RIGHT: f32 = 1000.0;
const PLOT_TOP: f32 = 120.0;
const PLOT_BOTTOM: f32 = 600.0;
const LP_MIN: f64 = -260.0;
const LP_MAX: f64 = 470.0;
const COLORS: [&str; 10] = [GOLD, RED, "#ff9f43", TEAL, "#4fd1e6", BLUE, CREAM, SOFT, "#ff7b8f", "#7cb8ff"];

struct PlayerRace { name: String, color: &'static str, events: Vec<(i64, i64)> }
struct Race { players: Vec<PlayerRace>, start: i64, end: i64, match_ends: Vec<i64>, tz: chrono_tz::Tz, lang: Lang, month: String }

struct Grid { t0: i64, t1: i64, times: Vec<f64>, raw: Vec<Vec<f64>>, smooth: Vec<Vec<f64>> }
#[derive(Clone, Copy, PartialEq)]
enum Phase { Motion, Peak, Low, Final }
struct Timeline { walls: Vec<i64>, phases: Vec<Phase> }
struct FrameData { labels: Vec<Vec<f32>>, peak: (i64, i64, usize), low: (i64, i64, usize) }

fn absolute(rank: &Rank) -> Option<i64> { rank.absolute() }

fn collect(app: &App) -> Result<Race> {
    let (start, end) = reports::window(app, Period::Monthly)?;
    let tz = app.cfg.reports.timezone.parse::<chrono_tz::Tz>().context("Invalid reports.timezone")?;
    let roster = app.store.players()?;
    let mut played: HashMap<(String, u16), i64> = HashMap::new();
    let mut games = 0i64;
    let mut match_ends = Vec::new();
    app.store.visit_matches(start, end, |m| {
        if m.remake || !matches!(m.queue, 420 | 440) { return; }
        let tracked: Vec<_> = m.participants.iter().filter(|p| p.tracked && roster.iter().any(|r| r.puuid == p.puuid)).collect();
        if tracked.is_empty() { return; }
        games += 1;
        match_ends.push(m.ended_at);
        for p in tracked { *played.entry((p.puuid.clone(), m.queue)).or_default() += 1; }
    })?;
    ensure!(games > 0, "No tracked games in this month");

    let mut by_player: HashMap<String, Vec<(u16, Vec<(i64, i64)>)>> = HashMap::new();
    for ((puuid, queue), _) in played {
        let base = match app.store.rank_at(&puuid, queue, start)? {
            Some(snapshot) => Some(snapshot),
            None => app.store.first_rank(&puuid, queue, start, end)?,
        };
        let Some(base) = base else { continue; };
        let Some(base_lp) = absolute(&base.rank) else { continue; };
        let mut events = Vec::new();
        for Snapshot { at, rank } in app.store.rank_series(&puuid, queue, start, end)? {
            if let Some(lp) = absolute(&rank) { events.push((at, lp - base_lp)); }
        }
        if !events.is_empty() { by_player.entry(puuid).or_default().push((queue, events)); }
    }

    let mut players: Vec<PlayerRace> = Vec::new();
    for (puuid, queues) in by_player {
        let Some(roster_player) = roster.iter().find(|p| p.puuid == puuid) else { continue; };
        let mut times = BTreeSet::new();
        for (_, events) in &queues { for (at, _) in events { times.insert(*at); } }
        let mut cursors = vec![0usize; queues.len()];
        let mut held = vec![0i64; queues.len()];
        let mut merged = Vec::with_capacity(times.len());
        for at in times {
            for (index, (_, events)) in queues.iter().enumerate() {
                while cursors[index] < events.len() && events[cursors[index]].0 <= at {
                    held[index] = events[cursors[index]].1;
                    cursors[index] += 1;
                }
            }
            merged.push((at, held.iter().sum::<i64>()));
        }
        if merged.is_empty() { continue; }
        players.push(PlayerRace { name: roster_player.name().to_owned(), color: COLORS[0], events: merged });
    }
    ensure!(!players.is_empty(), "No rank history in this month");

    ensure!(players.len() <= COLORS.len(), "Too many players for the race palette");
    players.sort_by(|a, b| b.events.last().map(|e| e.1).cmp(&a.events.last().map(|e| e.1)).then_with(|| a.name.cmp(&b.name)));
    for (index, player) in players.iter_mut().enumerate() { player.color = COLORS[index]; }

    let lang = lang_of(app);
    let month = chrono::DateTime::from_timestamp(end, 0).unwrap_or_default()
        .with_timezone(&tz).format(lang.t(Msg::MonthFormat)).to_string();
    match_ends.sort_unstable();
    Ok(Race { players, start, end, match_ends, tz, lang, month })
}

fn lang_of(app: &App) -> Lang { app.cfg.language }

impl Grid {
    fn new(race: &Race) -> Self {
        let t0 = race.start as f64;
        let t1 = race.end as f64;
        let times: Vec<f64> = (0..SAMPLES).map(|i| t0 + (t1 - t0) * i as f64 / (SAMPLES - 1) as f64).collect();
        let mut raw = Vec::with_capacity(race.players.len());
        let mut smooth = Vec::with_capacity(race.players.len());
        for player in &race.players {
            let mut values = Vec::with_capacity(SAMPLES);
            let mut cursor = 0usize;
            let mut held = 0.0f64;
            for &t in &times {
                while cursor < player.events.len() && (player.events[cursor].0 as f64) <= t {
                    held = player.events[cursor].1 as f64;
                    cursor += 1;
                }
                values.push(held);
            }
            let dt = (t1 - t0) / (SAMPLES - 1) as f64;
            smooth.push(gaussian(&values, SMOOTH_SECS / dt, dt));
            raw.push(values);
        }
        Grid { t0: race.start, t1: race.end, times, raw, smooth }
    }

    fn value(&self, player: usize, wall: f64) -> f64 {
        let position = (wall - self.t0 as f64) / (self.t1 - self.t0) as f64 * (SAMPLES - 1) as f64;
        let index = position.clamp(0.0, (SAMPLES - 1) as f64) as usize;
        let frac = position - index as f64;
        if index + 1 < SAMPLES { self.smooth[player][index] * (1.0 - frac) + self.smooth[player][index + 1] * frac } else { self.smooth[player][index] }
    }

}

/// Causal-looking but symmetric gaussian; the last three hours blend back to the real close.
fn gaussian(values: &[f64], sigma: f64, dt: f64) -> Vec<f64> {
    let radius = (sigma * 3.0).ceil() as isize;
    let mut kernel = Vec::with_capacity((radius * 2 + 1) as usize);
    let mut total = 0.0;
    for i in -radius..=radius {
        let w = (-0.5 * (i as f64 / sigma).powi(2)).exp();
        kernel.push(w);
        total += w;
    }
    for w in &mut kernel { *w /= total; }
    let n = values.len();
    let mut out = vec![0.0; n];
    for i in 0..n {
        let mut sum = 0.0;
        for (k, w) in kernel.iter().enumerate() {
            let j = (i as isize + k as isize - radius).clamp(0, (n - 1) as isize) as usize;
            sum += values[j] * w;
        }
        out[i] = sum;
    }
    let blend = (3.0 * 3600.0 / dt).round().max(1.0) as usize;
    let start = n.saturating_sub(blend);
    for i in start..n {
        let w = (i - start) as f64 / (n - 1 - start).max(1) as f64;
        out[i] = out[i] * (1.0 - w) + values[i] * w;
    }
    out[n - 1] = values[n - 1];
    out
}

impl Timeline {
    fn new(grid: &Grid, players: &[PlayerRace]) -> (Self, (i64, i64, usize), (i64, i64, usize)) {
        let mut peak = (grid.t0, i64::MIN, 0usize);
        let mut low = (grid.t0, i64::MAX, 0usize);
        for (index, _) in players.iter().enumerate() {
            for (i, &value) in grid.raw[index].iter().enumerate() {
                let at = grid.times[i] as i64;
                if value as i64 > peak.1 { peak = (at, value as i64, index); }
                if (value as i64) < low.1 { low = (at, value as i64, index); }
            }
        }
        let mut keys = vec![(peak.0, Phase::Peak), (low.0, Phase::Low)];
        keys.sort_by_key(|k| k.0);
        let mut points = vec![grid.t0];
        points.extend(keys.iter().map(|k| k.0));
        points.push(grid.t1);
        let spans: Vec<f64> = points.windows(2).map(|w| (w[1] - w[0]).max(1) as f64).collect();
        let motion = FRAMES - 2 * HOLD - FINAL;
        let total: f64 = spans.iter().sum();
        let mut counts: Vec<usize> = spans.iter().map(|s| (motion as f64 * s / total).round() as usize).collect();
        let assigned: usize = counts.iter().sum();
        *counts.last_mut().unwrap() += motion.saturating_sub(assigned);

        let mut walls = Vec::with_capacity(FRAMES);
        let mut phases = Vec::with_capacity(FRAMES);
        for (k, count) in counts.iter().enumerate() {
            for i in 0..*count {
                let frac = if *count <= 1 { 1.0 } else { i as f64 / (*count - 1) as f64 };
                let wall = points[k] + ((points[k + 1] - points[k]) as f64 * frac) as i64;
                walls.push(wall);
                phases.push(Phase::Motion);
            }
            if k < keys.len() {
                for _ in 0..HOLD { walls.push(points[k + 1]); phases.push(keys[k].1); }
            }
        }
        for _ in 0..FINAL { walls.push(points[points.len() - 1]); phases.push(Phase::Final); }
        walls.truncate(FRAMES);
        phases.truncate(FRAMES);
        (Timeline { walls, phases }, peak, low)
    }
}

fn spread(mut values: Vec<f32>, gap: f32, low: f32, high: f32) -> Vec<f32> {
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|a, b| values[*a].total_cmp(&values[*b]));
    for pair in order.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if values[b] - values[a] < gap { values[b] = values[a] + gap; }
    }
    if let Some(&last) = order.last() {
        let over = values[last] - high;
        if over > 0.0 { for v in &mut values { *v -= over; } }
    }
    for pair in order.windows(2).rev() {
        let (a, b) = (pair[0], pair[1]);
        if values[b] - values[a] < gap { values[a] = values[b] - gap; }
    }
    for v in &mut values { *v = v.max(low); }
    values
}

fn py(value: f64) -> f32 {
    PLOT_TOP + ((LP_MAX - value) / (LP_MAX - LP_MIN)) as f32 * (PLOT_BOTTOM - PLOT_TOP)
}
fn px(wall: f64, race: &Race) -> f32 {
    PLOT_LEFT + ((wall - race.start as f64) / (race.end - race.start) as f64) as f32 * (PLOT_RIGHT - PLOT_LEFT)
}

fn labels(race: &Race, grid: &Grid, tl: &Timeline) -> Vec<Vec<f32>> {
    let n = race.players.len();
    let mut raw = vec![vec![0.0f32; n]; FRAMES];
    for (f, &wall) in tl.walls.iter().enumerate() {
        let values: Vec<f32> = (0..n).map(|j| py(grid.value(j, wall as f64))).collect();
        raw[f] = spread(values, 17.0, 150.0, 570.0);
    }
    let mut eased = raw.clone();
    for f in 1..FRAMES {
        for j in 0..n { eased[f][j] = eased[f - 1][j] + (raw[f][j] - eased[f - 1][j]) * 0.3; }
    }
    eased[FRAMES - 1] = raw[FRAMES - 1].clone();
    eased
}

fn date(race: &Race, wall: i64) -> String {
    chrono::DateTime::from_timestamp(wall, 0).unwrap_or_default().with_timezone(&race.tz).format("%d/%m/%Y").to_string()
}

fn frame_svg(race: &Race, grid: &Grid, tl: &Timeline, frames: &FrameData, f: usize) -> String {
    let lang = race.lang;
    let wall = tl.walls[f];
    let phase = tl.phases[f];
    let mut cv = Canvas { svg: format!("<svg xmlns='http://www.w3.org/2000/svg' width='{W}' height='{H}'><g font-family='Be Vietnam Pro,DejaVu Sans,sans-serif'>"), clips: 0 };
    let backdrop = cv.gradient(&[(0.0, "#0a1a28", 1.0), (0.5, BG, 1.0), (1.0, "#0a1420", 1.0)], (0.0, 0.0, 0.0, 1.0));
    cv.block(0.0, 0.0, W as f32, H as f32, 0.0, &backdrop, "");
    cv.stroke(4.0, 4.0, W as f32 - 8.0, H as f32 - 8.0, 0.0, GOLD_DIM, 1.5, 1.0);

    let gold = cv.gradient(&[(0.0, "#f9e7b0", 1.0), (0.5, GOLD, 1.0), (1.0, "#8c6a2e", 1.0)], (0.0, 0.0, 0.0, 1.0));
    let title = lang.f(Msg::RaceTitle, &[&race.month]).to_uppercase();
    cv.label(31.0, 59.0, 34.0, "#000", 700, "start", 0.0, &title);
    let _ = write!(cv.svg, "<text x='30' y='56' font-size='34' font-weight='700' fill='{gold}'>{}</text>", escape(&title));

    cv.block(1050.0, 16.0, 200.0, 74.0, 0.0, BG, "fill-opacity='.72' stroke='#c8aa6e' stroke-width='1.2'");
    cv.stroke(1056.0, 22.0, 188.0, 62.0, 0.0, GOLD_DIM, 0.8, 1.0);
    cv.label(1150.0, 38.0, 8.5, SOFT, 700, "middle", 1.4, lang.t(Msg::RaceGroup));
    let group = (0..race.players.len()).map(|j| grid.value(j, wall as f64).round() as i64).sum::<i64>();
    let group_ink = if group > 0 { TEAL } else if group < 0 { RED } else { SOFT };
    let group_text = if group == 0 { "0 LP".to_owned() } else { format!("{group:+} LP") };
    cv.label(1150.0, 74.0, 26.0, group_ink, 700, "middle", 0.0, &group_text);
    cv.line(0.0, 98.0, W as f32, 98.0, GOLD, 0.8);

    for value in (-200..=400).step_by(100) {
        let y = py(value as f64);
        cv.line(PLOT_LEFT, y, PLOT_RIGHT, y, if value == 0 { GOLD_DIM } else { RULE }, if value == 0 { 0.9 } else { 0.7 });
        cv.label(PLOT_LEFT - 8.0, y + 3.0, 9.0, SOFT, 400, "end", 0.0, &value.to_string());
    }
    for day in [1, 8, 15, 22, 29] {
        let at = race.start + (day - 1) * 86400;
        let x = px(at as f64, race);
        cv.line(x, PLOT_TOP, x, PLOT_BOTTOM, RULE, 0.6);
        cv.label(x, PLOT_BOTTOM + 20.0, 9.0, SOFT, 400, "middle", 0.0, &format!("{day}/{}", chrono::DateTime::from_timestamp(at, 0).unwrap_or_default().with_timezone(&race.tz).format("%m")));
    }

    let x_now = px(wall as f64, race);
    let sampled: Vec<usize> = (0..SAMPLES).step_by(5).collect();
    for (j, player) in race.players.iter().enumerate() {
        let mut path = String::new();
        for &i in &sampled {
            if grid.times[i] > wall as f64 { break; }
            let x = px(grid.times[i], race);
            let y = py(grid.smooth[j][i]);
            let _ = write!(path, "{}{x:.1},{y:.1} ", if path.is_empty() { "M" } else { "L" });
        }
        if !path.is_empty() {
            let _ = write!(cv.svg, "<path d='{path}' fill='none' stroke='{}' stroke-width='1.6' stroke-linecap='round' stroke-linejoin='round' opacity='.96'/>", player.color);
        }
        let dot_y = py(grid.value(j, wall as f64));
        let _ = write!(cv.svg, "<circle cx='{x_now:.1}' cy='{dot_y:.1}' r='3.2' fill='{}' stroke='{BG}' stroke-width='.8'/>", player.color);
    }

    let hide = match phase { Phase::Peak => Some(frames.peak.2), Phase::Low => Some(frames.low.2), _ => None };
    let spread_now = (0..race.players.len()).map(|j| grid.value(j, wall as f64)).fold(f64::MIN, f64::max)
        - (0..race.players.len()).map(|j| grid.value(j, wall as f64)).fold(f64::MAX, f64::min);
    for (j, player) in race.players.iter().enumerate() {
        if hide == Some(j) { continue; }
        let value = grid.value(j, wall as f64);
        let ly = frames.labels[f][j];
        let lx = x_now + 12.0;
        let gap = (py(value) - ly).abs();
        if gap > 1.5 && spread_now > 4.0 && gap < 130.0 {
            let _ = write!(cv.svg, "<line x1='{:.1}' y1='{:.1}' x2='{:.1}' y2='{:.1}' stroke='{}' stroke-width='.9' stroke-opacity='.5'/>", x_now + 4.0, py(value), lx - 7.0, ly, player.color);
        }
        let _ = write!(cv.svg, "<rect x='{:.1}' y='{:.1}' width='3' height='7' fill='{}'/>", lx - 8.0, ly - 3.5, player.color);
        cv.label(lx, ly + 3.4, 9.5, player.color, 700, "start", 0.0, &player.name);
        let width = player.name.chars().count() as f32 * 5.2 + 6.0;
        let rounded = value.round() as i64;
        let text = if rounded == 0 { "0".to_owned() } else { format!("{rounded:+}") };
        cv.label(lx + width, ly + 3.4, 9.5, if rounded > 0 { TEAL } else if rounded < 0 { RED } else { SOFT }, 700, "start", 0.0, &text);
    }

    if phase == Phase::Peak || phase == Phase::Low {
        let (at, value, player) = if phase == Phase::Peak { frames.peak } else { frames.low };
        let kicker = if phase == Phase::Peak { lang.t(Msg::RacePeak) } else { lang.t(Msg::RaceLow) };
        let line = format!("{}  ·  {value:+} LP  ·  {}", race.players[player].name, date(race, at));
        cv.block(430.0, 24.0, 3.0, 52.0, 0.0, GOLD, "");
        cv.label(442.0, 43.0, 8.5, GOLD, 700, "start", 0.6, kicker);
        cv.label(442.0, 66.0, 14.0, CREAM, 700, "start", 0.0, &line);

        let cx = px(at as f64, race);
        let cy = py(value as f64);
        let _ = write!(cv.svg, "<circle cx='{cx:.1}' cy='{cy:.1}' r='5' fill='none' stroke='{}' stroke-width='1.6'/><circle cx='{cx:.1}' cy='{cy:.1}' r='1.4' fill='{CREAM}'/>", race.players[player].color);
        cv.label(cx + 8.0, cy + 3.0, 9.0, race.players[player].color, 700, "start", 0.0, &format!("{value:+}"));
        let start = tl.phases.iter().position(|p| *p == phase).unwrap_or(0);
        let t = (f.saturating_sub(start)) as f32 / HOLD as f32;
        for ring in 0..3 {
            let p = (t * 1.4 - ring as f32 * 0.25).rem_euclid(1.0);
            let _ = write!(cv.svg, "<circle cx='{cx:.1}' cy='{cy:.1}' r='{:.1}' fill='none' stroke='{}' stroke-width='1' stroke-opacity='{:.2}'/>", 6.0 + 14.0 * p, race.players[player].color, 0.45 * (1.0 - p));
        }
    } else {
        let games = race.match_ends.partition_point(|&t| t <= wall);
        let line = format!("{}  ·  {}  ·  {} {}", date(race, wall), lang.f(Msg::MatchesCount, &[&games]), race.players.len(), if lang == Lang::Vi { "người chơi" } else { "players" });
        cv.label(31.0, 80.0, 11.0, "#e4dcc8", 400, "start", 0.2, &line);
    }

    let progress = (wall - race.start) as f32 / (race.end - race.start).max(1) as f32;
    cv.block(30.0, 666.0, 1220.0, 3.0, 0.0, RULE, "");
    if progress > 0.002 { cv.block(30.0, 666.0, 1220.0 * progress.clamp(0.0, 1.0), 3.0, 0.0, GOLD, "fill-opacity='.85'"); }
    cv.svg.push_str("</g></svg>");
    cv.svg
}

fn output_size() -> (u32, u32) { ((W as f32 * SCALE) as u32, (H as f32 * SCALE) as u32) }

fn rasterize(svg: &str) -> Result<Vec<u8>> {
    let options = resvg::usvg::Options { font_family: "Be Vietnam Pro".into(), fontdb: crate::render::embedded_fonts(), ..Default::default() };
    let tree = resvg::usvg::Tree::from_str(svg, &options).map_err(|_| anyhow::anyhow!("Cannot parse race frame SVG"))?;
    let (width, height) = output_size();
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height).ok_or_else(|| anyhow::anyhow!("Cannot allocate race frame"))?;
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(SCALE, SCALE), &mut pixmap.as_mut());
    Ok(pixmap.take())
}

/// Render the current month's race to `output` (1280x720, 30 fps, 60 s).
pub fn render(app: &App, lang: Lang, output: &Path) -> Result<()> {
    let mut race = collect(app)?;
    race.lang = lang;
    race.month = chrono::DateTime::from_timestamp(race.end, 0).unwrap_or_default().with_timezone(&race.tz).format(lang.t(Msg::MonthFormat)).to_string();
    let grid = Grid::new(&race);
    let (timeline, peak, low) = Timeline::new(&grid, &race.players);
    let frames = FrameData { labels: labels(&race, &grid, &timeline), peak, low };
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) { std::fs::create_dir_all(parent)?; }

    let (width, height) = output_size();
    let child = Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgba", "-s", &format!("{width}x{height}"), "-r", &FPS.to_string(), "-i", "-",
            "-c:v", "libx264", "-preset", "medium", "-crf", "20", "-pix_fmt", "yuv420p", "-movflags", "+faststart"])
        .arg(output)
        .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped())
        .spawn().context("Cannot start ffmpeg; install ffmpeg to render race videos")?;
    let mut child = child;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("Cannot open ffmpeg stdin"))?;
    // Cap workers so peak memory stays bounded on large hosts; a small VPS still uses all its cores.
    let pool = rayon::ThreadPoolBuilder::new().num_threads(rayon::current_num_threads().min(8)).build()?;
    for chunk in (0..FRAMES).collect::<Vec<_>>().chunks(16) {
        let rendered: Vec<Vec<u8>> = pool.install(|| chunk.par_iter().map(|&f| {
            rasterize(&frame_svg(&race, &grid, &timeline, &frames, f))
        }).collect::<Result<_>>())?;
        for frame in rendered { stdin.write_all(&frame)?; }
    }
    drop(stdin);
    let out = child.wait_with_output()?;
    ensure!(out.status.success(), "ffmpeg failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    Ok(())
}

pub fn caption(app: &App, lang: Lang) -> Result<String> {
    let (_, end) = reports::window(app, Period::Monthly)?;
    let tz = app.cfg.reports.timezone.parse::<chrono_tz::Tz>()?;
    let month = chrono::DateTime::from_timestamp(end, 0).unwrap_or_default().with_timezone(&tz).format(lang.t(Msg::MonthFormat)).to_string();
    Ok(lang.f(Msg::RaceCaption, &[&month]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spread_keeps_labels_apart() {
        let out = spread(vec![100.0, 101.0, 103.0], 17.0, 0.0, 300.0);
        let mut sorted = out.clone();
        sorted.sort_by(f32::total_cmp);
        for pair in sorted.windows(2) { assert!(pair[1] - pair[0] >= 17.0); }
    }

    #[test]
    fn gaussian_pins_the_close_and_smooths_steps() {
        let mut values = vec![0.0; 8];
        values.extend([50.0; 8]);
        let out = gaussian(&values, 2.0, 3600.0);
        assert_eq!(out[15], 50.0);
        assert!(out[7] > 0.0 && out[7] < 50.0);
    }
}
