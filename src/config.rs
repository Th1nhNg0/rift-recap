use anyhow::{Context, Result, bail, ensure};
use crate::i18n::Lang;
use serde::Deserialize;
use std::{collections::HashMap, env, fs, path::{Path, PathBuf}};

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub players_file: PathBuf,
    pub database: PathBuf,
    pub language: Lang,
    pub retention_days: u32,
    pub riot: Riot,
    pub discord: Discord,
    pub reports: Reports,
    pub llm: Llm,
}
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Riot {
    pub platform: String,
    pub history_interval_secs: u64,
    pub catch_up_hours: u32,
    pub history_count: u8,
    pub request_interval_ms: u64,
}
#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Discord {
    pub guild_id: u64,
    pub games_channel_id: u64,
    pub rank_channel_id: u64,
}
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Reports {
    pub enabled: bool,
    /// Monthly report on the last day of each month.
    pub monthly: bool,
    pub timezone: String,
    pub hour: u32,
    pub minute: u32,
}
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Llm {
    pub enabled: bool,
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub timeout_secs: u64,
    /// Optional Markdown file with the commentary persona and style; the built-in style is used when unset.
    pub prompt_file: Option<PathBuf>,
    /// Send tracked players' display names so the prompt can refer to them (e.g. inside jokes).
    pub send_names: bool,
}
// Deliberately no Debug/Serialize: never render credentials through logging.
pub struct Secrets {
    pub riot: String,
    pub discord: String,
    pub llm: String,
}
impl Default for Config {
    fn default() -> Self {
        Self { players_file: "players.json".into(), database: "data/tracker.db".into(), language: Lang::Vi, retention_days: 180,
            riot: Riot::default(), discord: Discord::default(), reports: Reports::default(), llm: Llm::default() }
    }
}
impl Default for Riot {
    fn default() -> Self {
        Self { platform: "vn2".into(), history_interval_secs: 60, catch_up_hours: 3, history_count: 20, request_interval_ms: 1250 }
    }
}
impl Default for Reports {
    fn default() -> Self { Self { enabled: true, monthly: true, timezone: "Asia/Ho_Chi_Minh".into(), hour: 23, minute: 59 } }
}
impl Default for Llm {
    fn default() -> Self { Self { enabled: false, provider: "openrouter".into(), model: String::new(), base_url: "https://openrouter.ai/api/v1".into(), timeout_secs: 15, prompt_file: None, send_names: false } }
}
impl Config {
    pub fn load(path: &Path, env_path: Option<&Path>) -> Result<(Self, Secrets)> {
        let text = fs::read_to_string(path).context("Cannot read configuration file")?;
        // TOML parser diagnostics can echo private values; report only a safe message.
        let mut cfg: Self = toml::from_str(&text).map_err(|_| anyhow::anyhow!("Invalid TOML configuration or unknown field"))?;
        cfg.riot.platform.make_ascii_lowercase();
        ensure!(region(&cfg.riot.platform).is_some(), "Unsupported Riot platform");
        ensure!(cfg.riot.history_interval_secs >= 15, "history_interval_secs must be at least 15");
        ensure!((1..=100).contains(&cfg.riot.history_count), "history_count must be 1..100");
        ensure!((50..=10_000).contains(&cfg.riot.request_interval_ms), "request_interval_ms must be 50..10000");
        ensure!((1..=72).contains(&cfg.riot.catch_up_hours), "catch_up_hours must be 1..72");
        ensure!((7..=3650).contains(&cfg.retention_days), "retention_days must be 7..3650");
        ensure!(cfg.reports.hour < 24 && cfg.reports.minute < 60, "Invalid report time");
        cfg.reports.timezone.parse::<chrono_tz::Tz>().map_err(|_| anyhow::anyhow!("Invalid report timezone"))?;
        let parent = path.parent().unwrap_or(Path::new("."));
        if cfg.database.is_relative() { cfg.database = parent.join(&cfg.database); }
        if cfg.players_file.is_relative() { cfg.players_file = parent.join(&cfg.players_file); }
        if let Some(file) = cfg.llm.prompt_file.as_mut().filter(|f| f.is_relative()) { *file = parent.join(&*file); }
        let mut values = HashMap::new();
        if let Some(path) = env_path {
            for item in dotenvy::from_path_iter(path).map_err(|_| anyhow::anyhow!("Cannot read dotenv file"))? {
                let (key, value) = item.map_err(|_| anyhow::anyhow!("Invalid dotenv syntax"))?;
                values.insert(key, value);
            }
        }
        let secret = |name: &str| env::var(name).ok().or_else(|| values.get(name).cloned()).unwrap_or_default().trim().to_owned();
        let secrets = Secrets { riot: secret("RIOT_API_KEY"), discord: secret("DISCORD_TOKEN"), llm: secret("LLM_API_KEY") };
        if secrets.riot.is_empty() { bail!("RIOT_API_KEY is required"); }
        Ok((cfg, secrets))
    }
    pub fn validate_discord(&self, secrets: &Secrets) -> Result<()> {
        ensure!(!secrets.discord.is_empty(), "DISCORD_TOKEN is required for Discord mode");
        ensure!(self.discord.guild_id != 0, "discord.guild_id is required");
        Ok(())
    }
}
pub fn region(platform: &str) -> Option<&'static str> {
    Some(match platform {
        "br1" | "la1" | "la2" | "na1" => "americas",
        "eun1" | "euw1" | "ru" | "tr1" | "me1" => "europe",
        "jp1" | "kr" => "asia",
        "oc1" | "ph2" | "sg2" | "th2" | "tw2" | "vn2" => "sea",
        _ => return None,
    })
}
