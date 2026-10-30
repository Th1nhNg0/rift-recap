//! Daily/weekly/monthly report card in the scoreboard's style: a splash-art header with the group's LP,
//! the monthly MVP, honours, then a block of players who climbed and one of players who dropped.
use crate::{App, i18n::{Lang, Msg}, render::{Canvas, blocking, number, rasterize, short, tier_color}, render_assets::{Assets, Wanted},
    reports::{Mvp, PlayerLine, Report}, theme::{BG, BLUE, CREAM, GOLD, GOLD_DIM, PANEL, PANEL_DARK, RED, ROW_A, ROW_B, RULE, SOFT, TEAL}};
use anyhow::{Result, ensure};
use std::{fmt::Write, path::Path};

const W: u32 = 1200;
const HERO: f32 = 150.0;
const MVP_H: f32 = 96.0;
const HEAD_H: f32 = 42.0;
const SUB_H: f32 = 20.0;
const ROW: f32 = 54.0;
const TILE_H: f32 = 64.0;
const NAME_X: f32 = 90.0;
const RECORD_X: f32 = 452.0;
const KDA_X: f32 = 548.0;
const DAMAGE_X: f32 = 622.0;
const BAR_W: f32 = 100.0;
const CS_X: f32 = 786.0;
const VISION_X: f32 = 884.0;
const LP_X: f32 = 948.0;
const LP_SPAN: f32 = 150.0;
const GAIN: &str = TEAL;
const LOSS: &str = RED;
const NEUTRAL: &str = "#8a8f98";
const TILE_COLORS: [&str; 6] = [GOLD, RED, "#ff9f43", TEAL, "#4fd1e6", BLUE];

fn top_champion(p: &PlayerLine) -> Option<&str> { p.champions.first().map(|(c, _)| c.as_str()) }
fn portrait<'a>(art: &'a Assets, p: &PlayerLine) -> Option<&'a str> { top_champion(p).and_then(|c| art.image(&format!("champion:{c}"))) }
/// The group's best LP climber fronts the header.
fn featured(report: &Report) -> Option<&PlayerLine> {
    report.players.iter().max_by_key(|p| p.lp().unwrap_or(i64::MIN)).filter(|p| top_champion(p).is_some())
}

pub async fn image(app: &App, report: Report, lang: Lang) -> Result<Vec<u8>> {
    let tiers = report.players.iter().flat_map(|p| p.ranks.iter()).filter_map(|r| r.rank.as_ref()).map(|r| r.tier.clone()).collect();
    let wanted = Wanted {
        splash: featured(&report).and_then(top_champion).map(str::to_owned),
        champions: report.players.iter().filter_map(top_champion).map(str::to_owned).collect(), tiers, ..Default::default()
    };
    let art = Assets::load(wanted, app.cfg.database.parent().unwrap_or(Path::new(".")).join("artwork")).await;
    blocking(move || card(&report, lang, &art)).await
}

pub fn card(report: &Report, lang: Lang, art: &Assets) -> Result<Vec<u8>> {
    ensure!(!report.players.is_empty() && report.players.len() <= 100, "Unsupported report size");
    let awards = report.awards(lang);
    let mvp = report.mvp();
    let climbed: Vec<&PlayerLine> = report.players.iter().filter(|p| p.lp().is_some_and(|d| d >= 0)).collect();
    // Players arrive sorted by LP, highest first, so the smallest drop leads its block.
    let dropped: Vec<&PlayerLine> = report.players.iter().filter(|p| p.lp().is_some_and(|d| d < 0)).collect();
    let unknown: Vec<&PlayerLine> = report.players.iter().filter(|p| p.lp().is_none()).collect();
    let blocks = [(lang.t(Msg::BlockClimbed), GAIN, climbed), (lang.t(Msg::BlockDropped), LOSS, dropped), (lang.t(Msg::BlockUnknown), NEUTRAL, unknown)];
    let honours_h = if awards.is_empty() { 0.0 } else { HEAD_H + 2.0 * TILE_H };
    let height = HERO + honours_h + if mvp.is_some() { MVP_H } else { 0.0 } + blocks.iter().filter(|(_, _, rows)| !rows.is_empty()).map(|(_, _, rows)| HEAD_H + SUB_H + ROW * rows.len() as f32 + 6.0).sum::<f32>() + 8.0;
    ensure!(height <= 8000.0, "Report exceeds image bounds");
    let mut cv = Canvas { svg: format!("<svg xmlns='http://www.w3.org/2000/svg' width='{W}' height='{height}'><g font-family='Be Vietnam Pro,DejaVu Sans,sans-serif'>"), clips: 0 };
    let backdrop = cv.gradient(&[(0.0, "#0a1a28", 1.0), (0.5, BG, 1.0), (1.0, "#0a1420", 1.0)], (0.0, 0.0, 0.0, 1.0));
    cv.block(0.0, 0.0, W as f32, height, 0.0, &backdrop, "");
    header(&mut cv, report, lang, art);
    let mut y = HERO;
    if let Some(mvp) = mvp {
        mvp_band(&mut cv, y, report, &mvp, lang, art);
        y += MVP_H;
    }
    if !awards.is_empty() {
        cv.block(0.0, y, W as f32, HEAD_H, 0.0, PANEL, "");
        cv.hairline(0.0, W as f32, y, 0.6);
        cv.label(32.0, y + 27.0, 14.0, GOLD, 700, "start", 4.0, &lang.t(Msg::HonoursSection).to_uppercase());
        cv.line(0.0, y + HEAD_H, W as f32, y + HEAD_H, GOLD, 1.0);
        y += HEAD_H;
        let width = W as f32 / 3.0;
        for (index, award) in awards.iter().enumerate() {
            let (x, top) = (index as f32 % 3.0 * width, y + (index / 3) as f32 * TILE_H);
            let ink = if award.negative { RED } else { TILE_COLORS[index % TILE_COLORS.len()] };
            let player = &report.players[award.player];
            cv.block(x, top, width, TILE_H, 0.0, if (index + index / 3) % 2 == 0 { ROW_A } else { ROW_B }, "");
            cv.block(x, top, 4.0, TILE_H, 0.0, ink, "");
            cv.label(x + 24.0, top + 22.0, 10.5, ink, 700, "start", 2.0, &lang.t(award.title).to_uppercase());
            cv.label(x + 24.0, top + 44.0, 16.0, CREAM, 700, "start", 0.0, &short(&player.name, 18));
            cv.label(x + 24.0, top + 58.0, 11.5, SOFT, 400, "start", 0.0, &award.value);
            cv.picture(portrait(art, player), x + width - 74.0, top + 11.0, 42.0, 42.0, 0.0, "xMidYMid slice");
            cv.frame(x + width - 74.0, top + 11.0, 42.0, 42.0, ink);
        }
        y += 2.0 * TILE_H;
    }
    let max_damage = report.players.iter().map(|p| p.damage_per_game().round() as i64).max().unwrap_or(1).max(1);
    let widest = report.players.iter().filter_map(PlayerLine::lp).map(i64::abs).max().unwrap_or(0).max(1);
    for (title, ink, rows) in &blocks {
        if rows.is_empty() { continue; }
        cv.block(0.0, y, W as f32, HEAD_H, 0.0, PANEL, "");
        cv.hairline(0.0, W as f32, y, 0.6);
        cv.block(0.0, y, 4.0, HEAD_H, 0.0, ink, "");
        cv.label(32.0, y + 27.0, 16.0, ink, 700, "start", 3.0, title);
        let chip = 12.0 + rows.len().to_string().len() as f32 * 10.0 + 18.0;
        let after = 32.0 + title.chars().count() as f32 * 12.6 + 14.0;
        cv.block(after, y + 11.0, chip, 20.0, 0.0, BG, &format!("stroke='{GOLD}'"));
        cv.label(after + chip / 2.0, y + 25.5, 11.0, GOLD, 700, "middle", 1.0, &rows.len().to_string());
        cv.line(0.0, y + HEAD_H, W as f32, y + HEAD_H, GOLD, 1.0);
        y += HEAD_H;
        cv.block(0.0, y, W as f32, SUB_H, 0.0, PANEL_DARK, "");
        for (x, anchor, label) in [(RECORD_X, "middle", lang.t(Msg::ColRecord)), (KDA_X, "middle", "KDA"), (DAMAGE_X, "start", lang.t(Msg::ColDamageGame)), (CS_X, "middle", lang.t(Msg::ColCsMin)),
            (VISION_X, "middle", lang.t(Msg::ColVisionMin)), (LP_X, "start", lang.t(Msg::ColLpChange))] {
            cv.label(x, y + 14.0, 9.5, SOFT, 700, anchor, 1.0, label);
        }
        y += SUB_H;
        for (i, p) in rows.iter().enumerate() {
            row(&mut cv, p, y, i, lang, art, max_damage, widest);
            y += ROW;
        }
        y += 6.0;
    }
    cv.stroke(1.0, 1.0, W as f32 - 2.0, height - 2.0, 0.0, GOLD_DIM, 2.0, 1.0);
    cv.svg.push_str("</g></svg>");
    rasterize(cv.svg, W, height.ceil() as u32)
}

#[allow(clippy::too_many_arguments)]
fn row(cv: &mut Canvas, p: &PlayerLine, y: f32, index: usize, lang: Lang, art: &Assets, max_damage: i64, widest: i64) {
    cv.block(0.0, y, W as f32, ROW, 0.0, if index.is_multiple_of(2) { ROW_A } else { ROW_B }, "");
    cv.line(0.0, y + ROW, W as f32, y + ROW, RULE, 1.0);
    cv.picture(portrait(art, p), 32.0, y + 6.0, 42.0, 42.0, 0.0, "xMidYMid slice");
    cv.frame(32.0, y + 6.0, 42.0, 42.0, GOLD);
    cv.label(NAME_X, y + 23.0, 15.0, CREAM, 700, "start", 0.0, &short(&p.name, 22));
    if let Some(line) = p.ranks.iter().filter(|r| r.rank.is_some()).max_by_key(|r| r.games) && let Some(rank) = &line.rank {
        match art.image(&format!("tier:{}", rank.tier)) {
            Some(data) => cv.picture(Some(data), NAME_X - 2.0, y + 28.0, 28.0, 17.0, 0.0, "xMidYMid meet"),
            None => cv.polygon(&format!("{},{} {},{} {},{} {},{}", NAME_X + 12.0, y + 30.0, NAME_X + 20.0, y + 38.0, NAME_X + 12.0, y + 46.0, NAME_X + 4.0, y + 38.0), tier_color(&rank.tier)),
        }
        cv.runs(NAME_X + 30.0, y + 42.0, 12.0, "start", &[(&lang.rank_name(rank), tier_color(&rank.tier), 700), (&format!("  ·  {} LP  ·  {}", rank.lp, lang.queue(line.queue)), "#d6cfbd", 400)]);
    }
    cv.label(RECORD_X, y + 23.0, 15.0, CREAM, 700, "middle", 0.0, &format!("{} – {}", p.wins, p.games - p.wins));
    cv.label(RECORD_X, y + 42.0, 12.0, if p.wins * 2 >= p.games { GAIN } else { "#ff7b8f" }, 700, "middle", 0.0, &format!("{:.0}%", p.win_rate()));
    let average = |v: i64| v as f64 / p.games.max(1) as f64;
    cv.runs(KDA_X, y + 24.0, 15.0, "middle", &[(&format!("{:.1}", average(p.kills)), CREAM, 700), (" / ", GOLD_DIM, 400), (&format!("{:.1}", average(p.deaths)), "#ff7b8f", 700), (" / ", GOLD_DIM, 400), (&format!("{:.1}", average(p.assists)), CREAM, 700)]);
    let kda = p.kda();
    cv.label(KDA_X, y + 42.0, 11.5, if kda >= 3.0 { "#ff9f43" } else { SOFT }, 700, "middle", 0.0, &format!("{kda:.1} KDA"));
    let damage = p.damage_per_game().round() as i64;
    cv.label(DAMAGE_X, y + 23.0, 13.5, CREAM, 400, "start", 0.0, &number(damage));
    cv.block(DAMAGE_X, y + 35.0, BAR_W, 6.0, 0.0, BG, "stroke='#3a3326' stroke-width='.8'");
    cv.block(DAMAGE_X, y + 35.0, (damage as f32 / max_damage as f32 * BAR_W).clamp(3.0, BAR_W), 6.0, 0.0, BLUE, "");
    cv.label(CS_X, y + 32.0, 14.0, CREAM, 700, "middle", 0.0, &format!("{:.1}", p.cs_per_min()));
    cv.label(VISION_X, y + 32.0, 14.0, CREAM, 700, "middle", 0.0, &format!("{:.2}", p.vision_per_min()));
    match p.lp() {
        Some(0) => cv.label(LP_X, y + 33.0, 15.0, SOFT, 700, "start", 0.0, "0"),
        Some(delta) => {
            let length = (delta.unsigned_abs() as f32 / widest as f32 * LP_SPAN).round().max(6.0);
            let ink = if delta < 0 { LOSS } else { GAIN };
            let fill = cv.gradient(&[(0.0, ink, 0.55), (1.0, ink, 1.0)], (0.0, 0.0, 1.0, 0.0));
            cv.block(LP_X, y + 18.0, length, 18.0, 0.0, &fill, "");
            let _ = write!(cv.svg, "<rect x='{LP_X}' y='{}' width='{length}' height='18' fill='none' stroke='{ink}' stroke-opacity='.6'/>", y + 18.0);
            cv.label(LP_X + length + 10.0, y + 33.0, 15.0, if delta < 0 { "#ff7b8f" } else { "#4fe0d0" }, 700, "start", 0.0, &format!("{delta:+}"));
        }
        None => cv.label(LP_X, y + 32.0, 12.0, SOFT, 400, "start", 0.0, lang.t(Msg::LpUnknown)),
    }
}

/// Gold band between the hero and the honours: the monthly MVP and the champion behind it.
fn mvp_band(cv: &mut Canvas, y: f32, report: &Report, mvp: &Mvp, lang: Lang, art: &Assets) {
    let player = &report.players[mvp.player];
    let backdrop = cv.gradient(&[(0.0, "#241d12", 1.0), (0.42, "#2a2416", 1.0), (0.76, "#151d26", 1.0), (1.0, "#0b1621", 1.0)], (0.0, 0.0, 1.0, 0.0));
    cv.block(0.0, y, W as f32, MVP_H, 0.0, &backdrop, "");
    crown(cv, 34.0, y + 30.0, 20.0, GOLD);
    cv.label(79.0, y + 31.5, 11.5, GOLD, 700, "start", 2.6, &lang.f(Msg::MvpMonth, &[&report.month(lang)]).to_uppercase());
    cv.label(79.0, y + 64.0, 28.0, CREAM, 700, "start", 0.0, &short(&player.name, 30));
    let rating = format!("{:.2}", mvp.rating);
    cv.label(79.0, y + 84.0, 13.5, SOFT, 400, "start", 0.0, &lang.f(Msg::MvpLine, &[&rating, &player.games, &player.wins]));
    let (size, x) = (68.0, 1098.0);
    let top = y + (MVP_H - size) / 2.0;
    cv.picture(portrait(art, player), x, top, size, size, 0.0, "xMidYMid slice");
    cv.frame(x, top, size, size, GOLD);
    cv.block(0.0, y + MVP_H - 1.0, W as f32, 1.0, 0.0, GOLD, "fill-opacity='.6'");
}

/// Three-point crown drawn as one path so it stays sharp at any size.
fn crown(cv: &mut Canvas, x: f32, y: f32, h: f32, ink: &str) {
    let (w, base, shoulder, valley) = (h * 1.08, y + h, y + h * 0.4, y + h * 0.78);
    let _ = write!(cv.svg, "<path d='M{x} {base} L{x} {shoulder} L{} {valley} L{} {y} L{} {valley} L{} {shoulder} L{} {base} Z' fill='{ink}'/>",
        x + w * 0.25, x + w * 0.5, x + w * 0.75, x + w, x + w);
}

fn header(cv: &mut Canvas, report: &Report, lang: Lang, art: &Assets) {
    let net = report.net_lp();
    let ink = net.map_or(SOFT, |n| if n >= 0 { TEAL } else { "#ff7b8f" });
    let backdrop = cv.gradient(&[(0.0, "#0f2a3a", 1.0), (1.0, "#142b3a", 1.0)], (0.0, 0.0, 1.0, 1.0));
    cv.block(0.0, 0.0, W as f32, HERO, 0.0, &backdrop, "");
    if let Some(data) = featured(report).and_then(top_champion).and_then(|c| art.image(&format!("splash:{c}"))) {
        // Render only the visible header-sized pattern rather than creating a
        // full splash-sized isolation layer and clip mask.
        let _ = write!(cv.svg, "<defs><pattern id='hero' patternUnits='userSpaceOnUse' width='{W}' height='{HERO}'><image href='{data}' x='0' y='-170' width='{W}' height='708' preserveAspectRatio='xMidYMin slice'/></pattern></defs><rect width='{W}' height='{HERO}' fill='url(#hero)'/>");
    }
    let sides = cv.gradient(&[(0.0, BG, 0.92), (0.34, BG, 0.62), (0.52, BG, 0.0), (0.8, BG, 0.0), (1.0, BG, 0.72)], (0.0, 0.0, 1.0, 0.0));
    cv.block(0.0, 0.0, W as f32, HERO, 0.0, &sides, "");
    let floor = cv.gradient(&[(0.0, BG, 0.0), (0.7, BG, 0.0), (1.0, BG, 1.0)], (0.0, 0.0, 0.0, 1.0));
    cv.block(0.0, 0.0, W as f32, HERO, 0.0, &floor, "");
    let title = report.title(lang).to_uppercase();
    let gold = cv.gradient(&[(0.0, "#f9e7b0", 1.0), (0.5, GOLD, 1.0), (1.0, "#8c6a2e", 1.0)], (0.0, 0.0, 0.0, 1.0));
    // Long titles (a month with its year) shrink so they stay clear of the splash art.
    let size = if title.chars().count() > 16 { 38.0 } else { 46.0 };
    let title = short(&title, if size < 40.0 { 34 } else { 28 });
    let _ = write!(cv.svg, "<text x='34' y='94' font-size='{size}' font-weight='700' fill='#000' fill-opacity='.45' letter-spacing='3' transform='translate(2 3)'>{}</text>", crate::render::escape(&title));
    cv.label(34.0, 94.0, size, &gold, 700, "start", 3.0, &title);
    let sub = format!("{}  ·  {}", report.range(lang), lang.f(Msg::MatchesCount, &[&report.matches]));
    cv.label(36.0, 126.0, 14.0, "#e4dcc8", 400, "start", 1.5, &sub);
    // Group LP, framed in gold.
    cv.block(948.0, 24.0, 220.0, 100.0, 0.0, BG, &format!("fill-opacity='.72' stroke='{GOLD}' stroke-width='1.2'"));
    cv.stroke(954.0, 30.0, 208.0, 88.0, 0.0, GOLD_DIM, 0.8, 1.0);
    cv.label(1058.0, 54.0, 11.0, SOFT, 700, "middle", 3.0, lang.t(Msg::GroupLp));
    cv.label(1058.0, 100.0, 40.0, ink, 700, "middle", 0.0, &net.map_or_else(|| "—".to_owned(), |n| format!("{n:+} LP")));
    cv.hairline(0.0, W as f32, HERO - 1.0, 0.9);
}
