mod config;
mod discord;
mod i18n;
mod llm;
mod model;
mod presentation;
mod reports;
mod render_assets;
#[cfg(test)]
mod render_tests;
#[cfg(test)]
mod report_tests;
mod scoreboard;
mod render;
mod report_card;
mod riot;
mod store;
mod theme;
mod tracker;
mod webp_encode;

use anyhow::{Result, bail, ensure};
use clap::{Parser, Subcommand};
use config::{Config, Secrets};
use i18n::Lang;
use serenity::all::{Client, GatewayIntents};
use std::{path::PathBuf, sync::Arc};
use tokio::{sync::Semaphore, task::JoinSet};
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
#[command(version, about = "Self-hosted League of Legends squad tracker and Discord bot. Defaults to offline configuration checking.")]
struct Cli {
    #[arg(long, default_value = "config.toml", global = true)]
    config: PathBuf,
    #[arg(long, global = true)]
    env_file: Option<PathBuf>,
    #[command(subcommand)]
    mode: Option<Mode>,
}
#[derive(Subcommand)]
enum Mode {
    /// Validate config and initialize local storage. No network or Discord writes.
    Check,
    /// Read-only Riot account and Discord token checks. No gateway or messages.
    Probe,
    /// Render a recent real match to a WebP image; --post sends one preview per language without a gateway.
    RenderScoreboard {
        #[arg(long, default_value = "data/scoreboard.webp")]
        output: PathBuf,
        #[arg(long)]
        match_id: Option<String>,
        #[arg(long)]
        post: bool,
        /// vi, en, or all (one file per language, suffixed -vi/-en). Defaults to the configured language.
        #[arg(long)]
        language: Option<String>,
    },
    /// Render a daily/weekly/monthly report card to a WebP image (and print the text version). Uses stored
    /// matches, or fetches recent real matches read-only when none are stored. Never posts.
    RenderReport {
        #[arg(long, default_value = "weekly")]
        period: String,
        #[arg(long, default_value = "data/report.webp")]
        output: PathBuf,
        /// vi, en, or all (one file per language, suffixed -vi/-en). Defaults to the configured language.
        #[arg(long)]
        language: Option<String>,
        /// Upper bound on matches fetched from Riot for a preview without stored matches.
        #[arg(long, default_value_t = 15)]
        max_matches: usize,
    },
    /// Print the exact JSON the LLM receives for one match (a stored one, or the newest real match).
    /// Read-only: no LLM call, no network writes, useful for tuning `prompt.md`.
    CommentaryContext {
        #[arg(long)]
        match_id: Option<String>,
    },
    /// Run tracking. Discord sends are enabled unless --no-discord is supplied.
    Run {
        #[arg(long)]
        no_discord: bool,
        /// Run a single sweep; requires --no-discord. Still updates local state.
        #[arg(long, requires = "no_discord")]
        once: bool,
        /// Explicitly install/update this bot's guild commands (writes to Discord).
        #[arg(long, conflicts_with = "no_discord")]
        register_commands: bool,
    },
}
pub struct App {
    pub cfg: Config,
    pub store: store::Store,
    pub riot: riot::Riot,
    pub llm: Option<llm::Commentator>,
}
impl App {
    pub fn channel(&self, rank: bool) -> Result<u64> {
        let key = if rank { "channel_rank" } else { "channel_games" };
        match self.store.setting(key)? {
            Some(s) => s.parse().map_err(|_| anyhow::anyhow!("Invalid channel override in database")),
            None => Ok(if rank { self.cfg.discord.rank_channel_id } else { self.cfg.discord.games_channel_id }),
        }
    }
    fn load(cfg: Config, secrets: &Secrets) -> Result<Arc<Self>> {
        let store = store::Store::open(&cfg.database)?;
        store.bind("platform", &cfg.riot.platform)?;
        if cfg.discord.guild_id != 0 { store.bind("guild", &cfg.discord.guild_id.to_string())?; }
        store.seed(&cfg.players_file)?;
        let riot = riot::Riot::new(&secrets.riot, &cfg.riot.platform, cfg.riot.request_interval_ms)?;
        let llm = llm::Commentator::optional(&cfg.llm, secrets);
        Ok(Arc::new(Self { cfg, store, riot, llm }))
    }
}
fn log_targets() -> tracing_subscriber::filter::Targets {
    tracing_subscriber::filter::Targets::new().with_target("rift_recap", tracing::Level::INFO)
}
#[tokio::main]
async fn main() {
    // Intentionally exclude dependency HTTP/gateway logs, which can contain private data.
    tracing_subscriber::registry().with(log_targets()).with(tracing_subscriber::fmt::layer().with_target(false)).init();
    if let Err(error) = execute().await {
        eprintln!("Error: {error}");
        std::process::exit(1);
    }
}
async fn execute() -> Result<()> {
    let cli = Cli::parse();
    let (cfg, secrets) = Config::load(&cli.config, cli.env_file.as_deref())?;
    let app = App::load(cfg, &secrets)?;
    match cli.mode.unwrap_or(Mode::Check) {
        Mode::Check => {
            info!(players = app.store.players()?.len(), llm_active = app.llm.is_some(), "Configuration and local storage ready; no network requests made");
            Ok(())
        }
        Mode::Probe => {
            let mut failed = false;
            if let Some(player) = app.store.players()?.first() {
                match app.riot.resolve(player).await {
                    Ok(Some(_)) => info!("Riot credentials and account routing accepted (read-only probe)"),
                    Ok(None) => { tracing::warn!("Riot accepted the request but the first configured account was not found"); failed = true; }
                    Err(error) => { tracing::warn!(%error, "Riot read-only probe failed"); failed = true; }
                }
            } else { bail!("Add a player to the initial roster before probing Riot"); }
            if let Err(error) = discord::probe(&secrets.discord).await { tracing::warn!(%error, "Discord read-only probe failed"); failed = true; }
            ensure!(!failed, "One or more read-only credential probes failed");
            Ok(())
        }
        Mode::RenderScoreboard { output, match_id, post, language } => {
            ensure!(output.extension().and_then(|s| s.to_str()).is_some_and(|s| s.eq_ignore_ascii_case(render::IMAGE_EXT)), "Scoreboard output must be a WebP file");
            if post { app.cfg.validate_discord(&secrets)?; }
            let game = render::preview_match(&app, match_id.as_deref()).await?;
            let langs = languages(&app, language.as_deref())?;
            let directory = output.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
            std::fs::create_dir_all(directory)?;
            let stem = output.file_stem().and_then(|s| s.to_str()).unwrap_or("scoreboard");
            let path = |lang: Lang| {
                let suffix = if langs.len() > 1 { format!("-{}", lang.code()) } else { String::new() };
                directory.join(format!("{stem}{suffix}.{}", render::IMAGE_EXT))
            };
            for &lang in &langs {
                let image = render::image(&app, game.clone(), lang, true).await?;
                std::fs::write(path(lang), &image)?;
                info!(match_id = %game.id, language = lang.code(), "Saved real-match preview images");
                if post { discord::post_preview(&app, &secrets.discord, image, lang).await?; }
            }
            Ok(())
        }
        Mode::RenderReport { period, output, language, max_matches } => {
            let period = reports::Period::parse(&period).ok_or_else(|| anyhow::anyhow!("--period must be daily, weekly or monthly"))?;
            let langs = languages(&app, language.as_deref())?;
            let (start, end) = reports::window(&app, period)?;
            let mut matches = app.store.matches(start, end)?;
            if matches.is_empty() { matches = render::preview_matches(&app, start, max_matches).await?; }
            let report = reports::compute(&app, period, start, end, &matches)?.ok_or_else(|| anyhow::anyhow!("No tracked games in this period"))?;
            let directory = output.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
            std::fs::create_dir_all(directory)?;
            let stem = output.file_stem().and_then(|s| s.to_str()).unwrap_or("report");
            for &lang in &langs {
                let suffix = if langs.len() > 1 { format!("-{}", lang.code()) } else { String::new() };
                std::fs::write(directory.join(format!("{stem}-{}{suffix}.{}", period.key(), render::IMAGE_EXT)), report_card::image(&app, report.clone(), lang).await?)?;
                std::fs::write(directory.join(format!("{stem}-{}{suffix}.txt", period.key())), report.text(lang))?;
                info!(period = period.key(), language = lang.code(), "Saved report card and text");
            }
            Ok(())
        }
        Mode::CommentaryContext { match_id } => {
            let game = render::preview_match(&app, match_id.as_deref()).await?;
            let recent = app.store.recent_commentary(&game.id, 6)?;
            println!("{}", llm::context(&game, app.cfg.llm.send_names, &recent));
            Ok(())
        }
        Mode::Run { no_discord, once, register_commands } => {
            if once { return tracker::Tracker::new(app).run(true).await; }
            let mut tasks = JoinSet::new();
            let mut manager = None;
            if !no_discord {
                app.cfg.validate_discord(&secrets)?;
                ensure!(app.channel(false)? != 0, "Configure a game channel before running Discord mode");
                let handler = discord::Handler { app: app.clone(), slots: Semaphore::new(2) };
                let mut client = Client::builder(&secrets.discord, GatewayIntents::GUILDS).event_handler(handler).await.map_err(|_| anyhow::anyhow!("Cannot initialize Discord client"))?;
                discord::validate_channels(&app, &client.http).await?;
                if register_commands { discord::register(&client.http, app.cfg.discord.guild_id).await?; }
                manager = Some(client.shard_manager.clone());
                tasks.spawn(discord::delivery_worker(app.clone(), client.http.clone()));
                tasks.spawn(discord::commentary_worker(app.clone(), client.http.clone()));
                tasks.spawn(reports::scheduler(app.clone()));
                tasks.spawn(async move { client.start().await.map_err(|_| anyhow::anyhow!("Discord gateway stopped unexpectedly")) });
            }
            tasks.spawn(tracker::Tracker::new(app).run(false));
            let outcome = tokio::select! {
                signal = shutdown_signal() => signal,
                worker = tasks.join_next() => match worker {
                    Some(Ok(Err(error))) => Err(error),
                    _ => Err(anyhow::anyhow!("A background worker stopped unexpectedly")),
                }
            };
            info!("Shutting down; pending work remains in SQLite");
            if let Some(manager) = manager { manager.shutdown_all().await; }
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
            outcome
        }
    }
}
async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result?, _ = term.recv() => () }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
/// `--language`: one language, `all`, or the configured default.
fn languages(app: &App, code: Option<&str>) -> Result<Vec<Lang>> {
    Ok(match code {
        None => vec![app.cfg.language],
        Some(code) if code.eq_ignore_ascii_case("all") => Lang::ALL.to_vec(),
        Some(code) => vec![Lang::parse(code).ok_or_else(|| anyhow::anyhow!("--language must be vi, en or all"))?],
    })
}

#[cfg(test)]
mod logging_tests {
    #[test]
    fn only_application_info_and_above_are_enabled() {
        let filter = super::log_targets();
        for target in ["rift_recap", "rift_recap::discord", "rift_recap::riot"] {
            for level in [tracing::Level::INFO, tracing::Level::WARN, tracing::Level::ERROR] {
                assert!(filter.would_enable(target, &level));
            }
            for level in [tracing::Level::DEBUG, tracing::Level::TRACE] {
                assert!(!filter.would_enable(target, &level));
            }
        }
        // Never expose dependency HTTP/gateway logs, including error-level events.
        for target in ["reqwest", "hyper", "serenity", "serenity::gateway", "rustls"] {
            assert!(!filter.would_enable(target, &tracing::Level::ERROR));
        }
    }
}
