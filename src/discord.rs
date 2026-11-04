use crate::{App, highlights, i18n::{Lang, Msg}, model::{Match, Notice, now}, presentation as view, render, reports, tracker};
use anyhow::{Result, bail, ensure};
use serenity::{all::*, async_trait};
use std::{fmt, sync::Arc, time::Duration};
use tokio::{sync::Semaphore, time::sleep};
use tracing::{info, warn};

pub struct Handler { pub app: Arc<App>, pub slots: Semaphore }
fn mentions() -> CreateAllowedMentions { CreateAllowedMentions::new().all_users(false).all_roles(false).everyone(false).replied_user(false) }
async fn scoreboard_image(app: &App, game: &Match) -> Option<Vec<u8>> {
    match render::image(app, game.clone(), app.cfg.language, false).await {
        Ok(image) => Some(image),
        Err(error) => { warn!(%error, "Scoreboard image unavailable; retaining text scoreboard fallback"); None }
    }
}
/// Streak, milestone and nemesis lines for a game; a failure only drops them.
fn highlight_text(app: &App, game: &Match) -> String {
    highlights::text(app, game, app.cfg.language).unwrap_or_else(|error| { warn!(%error, "Highlights unavailable"); String::new() })
}
fn safe_http<T>(value: serenity::Result<T>) -> Result<T> {
    value.map_err(|error| {
        // Discord's status and error text describe the rejection only; they never echo credentials.
        if let serenity::Error::Http(HttpError::UnsuccessfulRequest(r)) = &error {
            warn!(status = r.status_code.as_u16(), code = r.error.code, message = %r.error.message, "Discord rejected the request");
        } else {
            warn!(%error, "Discord request failed");
        }
        anyhow::anyhow!("Discord request failed; check token, permissions and connectivity")
    })
}

/// An already-localized message meant for the invoking member. Any other error is logged and
/// replaced by a generic localized message, so internal details never reach Discord.
#[derive(Debug)]
struct Denied(String);
impl fmt::Display for Denied { fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { f.write_str(&self.0) } }
impl std::error::Error for Denied {}
fn deny(text: impl Into<String>) -> anyhow::Error { anyhow::Error::new(Denied(text.into())) }
fn reply_error(error: &anyhow::Error, lang: Lang, fallback: Msg) -> String {
    match error.downcast_ref::<Denied>() {
        Some(denied) => denied.0.clone(),
        None => { warn!(%error, "Discord interaction failed"); lang.t(fallback).into() }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _: Context, _: Ready) { info!("Discord gateway ready"); }
    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let lang = self.app.cfg.language;
        match interaction {
            Interaction::Command(cmd) => {
                if cmd.guild_id.map(|g| g.get()) != Some(self.app.cfg.discord.guild_id) {
                    let _ = cmd.create_response(&ctx.http, CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().content(lang.t(Msg::WrongGuild)).ephemeral(true).allowed_mentions(mentions()))).await;
                    return;
                }
                // Results and ranks are meant for the group (as in the original bot); settings stay private.
                let public = matches!(cmd.data.name.as_str(), "lastgame" | "rank" | "version" | "nemesis");
                let deferred = if public { cmd.defer(&ctx.http).await } else { cmd.defer_ephemeral(&ctx.http).await };
                if deferred.is_err() { return; }
                let Ok(_slot) = self.slots.try_acquire() else {
                    let _ = cmd.edit_response(&ctx.http, EditInteractionResponse::new().content(lang.t(Msg::Busy)).allowed_mentions(mentions())).await; return;
                };
                let builder = match self.command(&cmd, &ctx.http).await {
                    Ok(reply) => reply.builder(lang),
                    Err(error) => EditInteractionResponse::new().content(reply_error(&error, lang, Msg::CommandFailed)).allowed_mentions(mentions()),
                };
                if cmd.edit_response(&ctx.http, builder).await.is_err() { warn!("Could not complete Discord command response"); }
            }
            Interaction::Autocomplete(cmd) => {
                if cmd.guild_id.map(|g| g.get()) != Some(self.app.cfg.discord.guild_id) { return; }
                // Removing players is for administrators; naming one in /nemesis is for everyone.
                let allowed = cmd.data.name == "nemesis" || cmd.member.as_ref().and_then(|m| m.permissions).is_some_and(|p| p.administrator());
                let mut response = CreateAutocompleteResponse::new();
                if let (true, Some(focused)) = (allowed, cmd.data.autocomplete()) {
                    let query = focused.value.to_lowercase();
                    for id in self.app.store.players().unwrap_or_default().iter().map(|p| p.riot_id()).filter(|id| id.to_lowercase().contains(&query)).take(25) {
                        response = response.add_string_choice(id.clone(), id);
                    }
                }
                let _ = cmd.create_response(&ctx.http, CreateInteractionResponse::Autocomplete(response)).await;
            }
            _ => (),
        }
    }
}
struct Reply { text: String, file: Option<(String, Vec<u8>)> }
impl Reply {
    fn text(text: impl Into<String>) -> Self { Self { text: text.into(), file: None } }
    fn builder(self, lang: Lang) -> EditInteractionResponse {
        let mut builder = EditInteractionResponse::new().allowed_mentions(mentions());
        if self.text.is_empty() && self.file.is_none() {
            return builder.content(lang.t(Msg::NoData));
        }
        if self.text.encode_utf16().count() > 1900 {
            builder = builder.content(lang.t(Msg::FullAttached)).new_attachment(CreateAttachment::bytes(self.text.into_bytes(), "response.txt"));
        } else { builder = builder.content(self.text); }
        if let Some((name, data)) = self.file { builder = builder.new_attachment(CreateAttachment::bytes(data, name)); }
        builder
    }
}
impl Handler {
    async fn command(&self, cmd: &CommandInteraction, http: &Http) -> Result<Reply> {
        let lang = self.app.cfg.language;
        let opts = &cmd.data.options;
        match cmd.data.name.as_str() {
            "lastgame" => {
                let Some(game) = self.app.store.get_match(None)? else { return Ok(Reply::text(lang.t(Msg::NoGames))); };
                let image = scoreboard_image(&self.app, &game).await;
                let text = match self.app.store.setting(&format!("commentary/{}", game.id))? {
                    Some(text) => text,
                    // Imported or never-commented games get their AI commentary on first request.
                    None if self.app.llm.is_some() && !game.remake => match ai_text(&self.app, &game).await {
                        Ok(text) => { self.app.store.commentary_done(&game.id, Some(&text))?; text }
                        Err(error) => { warn!(%error, "Optional LLM commentary unavailable"); String::new() }
                    },
                    None => String::new(),
                };
                // Commentary is optional; the text summary only stands in when the image could not be drawn.
                let text = if text.is_empty() && image.is_none() { view::commentary(&game, lang) } else { text };
                let text = highlights::join(&text, &highlight_text(&self.app, &game));
                Ok(Reply { text, file: image.map(|data| (format!("scoreboard.{}", render::IMAGE_EXT), data)) })
            }
            "players" => {
                let players = self.app.store.players()?;
                let lines: Vec<String> = players.iter().enumerate().map(|(i, p)| format!("{}. {} · {}", i + 1, view::plain(p.name()), view::plain(&p.riot_id()))).collect();
                Ok(Reply::text(format!("{}\n{}", lang.f(Msg::RosterTitle, &[&players.len()]), lines.join("\n"))))
            }
            "nemesis" => {
                let wanted = str_opt(opts, "player").map(|w| w.trim().to_lowercase());
                let roster: Vec<_> = self.app.store.players()?.into_iter().filter(|p| !p.puuid.is_empty())
                    .filter(|p| wanted.as_ref().is_none_or(|w| p.riot_id().to_lowercase() == *w || p.name().to_lowercase() == *w)).collect();
                if roster.is_empty() { return Err(deny(lang.t(Msg::PlayerNotTracked))); }
                let matchups = highlights::matchups(&self.app)?;
                let mut lines = vec![lang.f(Msg::NemesisTitle, &[&NEMESIS_GAMES, &self.app.cfg.retention_days])];
                for player in roster {
                    let row = matchups.get(&player.puuid);
                    let worst: Vec<String> = row.map(|m| m.worst(NEMESIS_GAMES)).unwrap_or_default().into_iter().filter(|m| m.rate() < 50.0).take(3).map(|m| m.label()).collect();
                    let mut text = if worst.is_empty() { lang.t(Msg::NemesisNone).to_owned() } else { worst.join(" · ") };
                    if let Some(victim) = row.and_then(|m| m.victim(NEMESIS_GAMES)) { text = format!("{text} | {}", lang.f(Msg::NemesisVictim, &[&victim.label()])); }
                    lines.push(format!("**{}** — {text}", view::plain(player.name())));
                }
                Ok(Reply::text(lines.join("\n")))
            }
            "version" => Ok(Reply::text(format!("rift-recap v{}", env!("CARGO_PKG_VERSION")))),
            "rank" => {
                let mut lines = Vec::new();
                for player in self.app.store.players()? {
                    if player.puuid.is_empty() { lines.push(format!("{} · {}", view::plain(player.name()), lang.t(Msg::AccountUnresolved))); continue; }
                    let text = match self.app.riot.ranks(&player.puuid).await {
                        Ok(ranks) => {
                            for rank in &ranks { self.app.store.snapshot(&player.puuid, rank, now())?; }
                            if ranks.is_empty() { lang.t(Msg::Unranked).into() }
                            else { ranks.iter().map(|r| format!("{}: {}", lang.queue(r.queue), lang.rank(r))).collect::<Vec<_>>().join(" · ") }
                        }
                        Err(_) => lang.t(Msg::RankUnavailable).into(),
                    };
                    lines.push(format!("{} — {text}", view::plain(player.name())));
                }
                Ok(Reply::text(lines.join("\n")))
            }
            "config" => {
                let permissions = cmd.member.as_ref().and_then(|m| m.permissions).unwrap_or_else(Permissions::empty);
                if !(permissions.administrator() || permissions.manage_guild()) { return Err(deny(lang.t(Msg::ManageServerRequired))); }
                let sub = opts.first().ok_or_else(|| anyhow::anyhow!("Missing subcommand"))?;
                let CommandDataOptionValue::SubCommand(opts) = &sub.value else { bail!("Invalid subcommand"); };
                let guild = self.app.cfg.discord.guild_id;
                match sub.name.as_str() {
                    "channel" => {
                        let channel = channel_opt(opts).ok_or_else(|| anyhow::anyhow!("Channel option missing"))?;
                        validate_channel(http, guild, channel).await.map_err(|p| deny(p.message(lang)))?;
                        self.app.store.set("channel_games", &channel.to_string())?;
                        Ok(Reply::text(lang.f(Msg::ChannelSet, &[&format!("<#{channel}>")])))
                    }
                    "rankchannel" => {
                        if let Some(channel) = channel_opt(opts) {
                            validate_channel(http, guild, channel).await.map_err(|p| deny(p.message(lang)))?;
                            self.app.store.set("channel_rank", &channel.to_string())?;
                        }
                        if let Some(enabled) = opts.iter().find_map(|o| if let CommandDataOptionValue::Boolean(v) = o.value { Some(v) } else { None }) {
                            self.app.store.set("rank_enabled", if enabled { "1" } else { "0" })?;
                        }
                        let enabled = self.app.store.setting("rank_enabled")?.map(|v| v == "1").unwrap_or(self.app.cfg.reports.enabled);
                        let channel = match self.app.channel(true)? { 0 => lang.t(Msg::NotSet).to_owned(), id => format!("<#{id}>") };
                        Ok(Reply::text(lang.f(Msg::ReportStatus, &[&lang.t(if enabled { Msg::Enabled } else { Msg::Disabled }), &channel])))
                    }
                    "addplayer" => {
                        if !permissions.administrator() { return Err(deny(lang.t(Msg::AdminRequired))); }
                        let mut player = tracker::parse_player(str_opt(opts, "riot_id").unwrap_or(""), str_opt(opts, "nickname").unwrap_or(""))
                            .map_err(|_| deny(lang.t(Msg::BadRiotId)))?;
                        let roster = self.app.store.players()?;
                        if roster.len() >= 100 { return Err(deny(lang.t(Msg::RosterFull))); }
                        if roster.iter().any(|p| p.riot_id().to_lowercase() == player.riot_id().to_lowercase()) { return Err(deny(lang.t(Msg::PlayerExists))); }
                        let riot = |error: anyhow::Error| { warn!(%error, "Riot lookup for a new player failed"); deny(lang.t(Msg::RiotUnavailable)) };
                        player.puuid = self.app.riot.resolve(&player).await.map_err(riot)?.ok_or_else(|| deny(lang.t(Msg::PlayerNotFound)))?;
                        if roster.iter().any(|p| p.puuid == player.puuid) { return Err(deny(lang.t(Msg::PlayerExists))); }
                        // Baseline before adding, to avoid replaying pre-subscription games.
                        let ids = self.app.riot.history(&player.puuid, self.app.cfg.riot.history_count).await.map_err(riot)?;
                        self.app.store.add_player(&player)?;
                        self.app.store.baseline(&player, &ids)?;
                        // An immediate snapshot gives the first game and report a known LP baseline.
                        match self.app.riot.ranks(&player.puuid).await {
                            Ok(ranks) => for rank in &ranks { self.app.store.snapshot(&player.puuid, rank, now())?; },
                            Err(error) => warn!(%error, "Initial rank snapshot unavailable; next refresh will retry"),
                        }
                        Ok(Reply::text(lang.f(Msg::PlayerAdded, &[&view::plain(&player.riot_id()), &(roster.len() + 1)])))
                    }
                    "removeplayer" => {
                        if !permissions.administrator() { return Err(deny(lang.t(Msg::AdminRequired))); }
                        let id = str_opt(opts, "riot_id").unwrap_or("");
                        if !self.app.store.remove_player(id)? { return Err(deny(lang.t(Msg::PlayerNotTracked))); }
                        Ok(Reply::text(lang.f(Msg::PlayerRemoved, &[&view::plain(id.trim()), &self.app.store.players()?.len()])))
                    }
                    "rankreport" => {
                        let period = str_opt(opts, "period").and_then(reports::Period::parse).unwrap_or(reports::Period::Daily);
                        if self.app.channel(true)? == 0 { return Err(deny(lang.t(Msg::RankChannelMissing))); }
                        reports::refresh_ranks(&self.app).await?;
                        if let Some(report) = reports::build(&self.app, period)? {
                            self.app.store.queue_report(&format!("manual/{}", cmd.id.get()), &reports::notice(report, lang))?;
                            Ok(Reply::text(lang.t(Msg::ReportQueued)))
                        } else { Ok(Reply::text(lang.t(Msg::NoGamesInPeriod))) }
                    }
                    _ => bail!("Unknown configuration command"),
                }
            }
            _ => bail!("Unknown command"),
        }
    }
}
/// Games against a champion before `/nemesis` lists it.
const NEMESIS_GAMES: i64 = 5;
fn str_opt<'a>(opts: &'a [CommandDataOption], name: &str) -> Option<&'a str> {
    opts.iter().find(|o| o.name == name).and_then(|o| if let CommandDataOptionValue::String(s) = &o.value { Some(s.as_str()) } else { None })
}
fn channel_opt(opts: &[CommandDataOption]) -> Option<u64> {
    opts.iter().find_map(|o| if let CommandDataOptionValue::Channel(c) = o.value { Some(c.get()) } else { None })
}

/// Why a channel cannot receive tracker messages. `Display` is English for logs.
pub enum ChannelProblem { Unreachable, NotText, MissingPermissions(Permissions) }
const REQUIRED: [(Permissions, Msg); 3] = [
    (Permissions::VIEW_CHANNEL, Msg::PermViewChannel), (Permissions::SEND_MESSAGES, Msg::PermSendMessages), (Permissions::ATTACH_FILES, Msg::PermAttachFiles),
];
impl ChannelProblem {
    fn message(&self, lang: Lang) -> String {
        match self {
            Self::Unreachable => lang.t(Msg::ChannelUnreachable).into(),
            Self::NotText => lang.t(Msg::ChannelNotText).into(),
            Self::MissingPermissions(missing) => {
                let names: Vec<&str> = REQUIRED.iter().filter(|(p, _)| missing.contains(*p)).map(|(_, m)| lang.t(*m)).collect();
                lang.f(Msg::ChannelMissingPerms, &[&names.join(", ")])
            }
        }
    }
}
impl fmt::Display for ChannelProblem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { f.write_str(&self.message(Lang::En)) }
}
pub async fn validate_channel(http: &Http, guild: u64, id: u64) -> Result<(), ChannelProblem> {
    let reachable = |_| ChannelProblem::Unreachable;
    let Channel::Guild(channel) = ChannelId::new(id).to_channel(http).await.map_err(reachable)? else { return Err(ChannelProblem::NotText); };
    if channel.guild_id.get() != guild || !matches!(channel.kind, ChannelType::Text | ChannelType::News) { return Err(ChannelProblem::NotText); }
    // Catch missing channel permissions now rather than as endless delivery retries.
    let me = http.get_current_user().await.map_err(reachable)?.id;
    let server = http.get_guild(GuildId::new(guild)).await.map_err(reachable)?;
    let member = server.id.member(http, me).await.map_err(reachable)?;
    let required = REQUIRED.iter().fold(Permissions::empty(), |all, (p, _)| all | *p);
    let missing = required - server.user_permissions_in(&channel, &member);
    if missing.is_empty() { Ok(()) } else { Err(ChannelProblem::MissingPermissions(missing)) }
}
pub async fn validate_channels(app: &App, http: &Http) -> Result<()> {
    for channel in [app.channel(false)?, app.channel(true)?] {
        if channel != 0 { validate_channel(http, app.cfg.discord.guild_id, channel).await.map_err(|p| anyhow::anyhow!("Configured channel unusable: {p}"))?; }
    }
    Ok(())
}

/// English is Discord's default text; other languages are added as client-locale localizations.
fn localize<T>(builder: T, msg: Msg, set: impl Fn(T, &'static str, &'static str) -> T) -> T {
    Lang::ALL.into_iter().filter(|l| *l != Lang::En).fold(builder, |b, lang| set(b, lang.discord_locale(), lang.t(msg)))
}
fn command(name: &str, msg: Msg) -> CreateCommand {
    localize(CreateCommand::new(name).description(Lang::En.t(msg)).dm_permission(false), msg, |b, locale, text| b.description_localized(locale, text))
}
fn option(kind: CommandOptionType, name: &str, msg: Msg) -> CreateCommandOption {
    localize(CreateCommandOption::new(kind, name, Lang::En.t(msg)), msg, |b, locale, text| b.description_localized(locale, text))
}
fn choice(option: CreateCommandOption, value: &str, msg: Msg) -> CreateCommandOption {
    let locales: Vec<_> = Lang::ALL.into_iter().filter(|l| *l != Lang::En).map(|l| (l.discord_locale(), l.t(msg))).collect();
    option.add_string_choice_localized(Lang::En.t(msg), value, locales)
}
pub async fn register(http: &Http, guild: u64) -> Result<()> {
    use CommandOptionType::{Boolean, Channel, String as Text, SubCommand};
    // Registration runs before the gateway reports the application id, so look it up first.
    http.set_application_id(safe_http(http.get_current_application_info().await)?.id);
    let channel = || option(Channel, "channel", Msg::OptChannel).channel_types(vec![ChannelType::Text, ChannelType::News]);
    let riot_id = || option(Text, "riot_id", Msg::OptRiotId).required(true).max_length(90);
    let period = choice(choice(choice(option(Text, "period", Msg::OptPeriod).required(true), "daily", Msg::ChoiceDaily), "weekly", Msg::ChoiceWeekly), "monthly", Msg::ChoiceMonthly);
    let config = command("config", Msg::CmdConfig).default_member_permissions(Permissions::MANAGE_GUILD)
        .add_option(option(SubCommand, "channel", Msg::CmdChannel).add_sub_option(channel().required(true)))
        .add_option(option(SubCommand, "rankchannel", Msg::CmdRankchannel).add_sub_option(option(Boolean, "enabled", Msg::OptEnabled)).add_sub_option(channel()))
        .add_option(option(SubCommand, "addplayer", Msg::CmdAddplayer).add_sub_option(riot_id()).add_sub_option(option(Text, "nickname", Msg::OptNickname).max_length(64)))
        .add_option(option(SubCommand, "removeplayer", Msg::CmdRemoveplayer).add_sub_option(riot_id().set_autocomplete(true)))
        .add_option(option(SubCommand, "rankreport", Msg::CmdRankreport).add_sub_option(period));
    let nemesis = command("nemesis", Msg::CmdNemesis).add_option(option(Text, "player", Msg::OptPlayer).max_length(90).set_autocomplete(true));
    for command in [command("lastgame", Msg::CmdLastgame), command("rank", Msg::CmdRank), command("players", Msg::CmdPlayers), command("version", Msg::CmdVersion), nemesis, config] {
        safe_http(GuildId::new(guild).create_command(http, command).await)?;
    }
    info!("Guild tracker commands installed; unrelated commands were not cleared");
    Ok(())
}

pub async fn delivery_worker(app: Arc<App>, http: Arc<Http>) -> Result<()> {
    loop {
        if let Some(delivery) = app.store.next_delivery()? {
            if deliver(&app, &http, &delivery).await.is_err() {
                warn!("Discord delivery failed; retained for retry");
                app.store.defer_delivery(delivery.id)?;
            }
        } else { sleep(Duration::from_secs(2)).await; }
    }
}
async fn deliver(app: &App, http: &Http, delivery: &crate::store::Delivery) -> Result<()> {
    let lang = app.cfg.language;
    match &delivery.notice {
        Notice::Result(m) => {
            let channel = app.channel(false)?; ensure!(channel != 0, "Game channel missing");
            // The result is the image; commentary joins it later. Text stands in only when the image failed.
            // Highlights ride along as the message text and stay when the commentary is added.
            let mut create = CreateMessage::new().allowed_mentions(mentions());
            let highlights = highlight_text(app, m);
            match scoreboard_image(app, m).await {
                Some(image) => {
                    create = create.add_file(CreateAttachment::bytes(image, format!("scoreboard.{}", render::IMAGE_EXT)));
                    if !highlights.is_empty() { create = create.content(highlights); }
                }
                None => create = create.content(highlights::join(&view::commentary(m, lang), &highlights)),
            }
            let message = safe_http(ChannelId::new(channel).send_message(http, create).await)?;
            app.store.ack(delivery.id, Some((&m.id, channel, message.id.get())))?;
        }
        Notice::Report { text, report } => {
            let channel = app.channel(true)?; ensure!(channel != 0, "Rank channel missing");
            // The card is rendered at delivery, like result scoreboards; text is the fallback.
            let card = match report {
                Some(report) => match crate::report_card::image(app, report.clone(), lang).await {
                    Ok(image) => Some((report.title(lang), report.range(lang), image)),
                    Err(error) => { warn!(%error, "Report card unavailable; sending text"); None }
                },
                None => None,
            };
            let builder = match card {
                Some((title, range, image)) => CreateMessage::new().content(format!("**{title}** · {range}")).add_file(CreateAttachment::bytes(image, format!("report.{}", render::IMAGE_EXT))),
                None if text.encode_utf16().count() <= 1900 => CreateMessage::new().content(text),
                None => CreateMessage::new().content(lang.t(Msg::ReportAttached)).add_file(CreateAttachment::bytes(text.as_bytes(), "report.txt")),
            };
            safe_http(ChannelId::new(channel).send_message(http, builder.allowed_mentions(mentions())).await)?;
            app.store.ack(delivery.id, None)?;
        }
    }
    Ok(())
}

/// The AI commentary for a game, bounded to one Discord message.
async fn ai_text(app: &App, game: &Match) -> Result<String> {
    let llm = app.llm.as_ref().ok_or_else(|| anyhow::anyhow!("LLM is not enabled"))?;
    let recent = app.store.recent_commentary(&game.id, 6)?;
    let highlights: Vec<String> = highlight_text(app, game).lines().map(str::to_owned).collect();
    Ok(view::limit(&llm.enhance(game, app.cfg.language, &recent, &highlights).await?, 2000))
}

pub async fn commentary_worker(app: Arc<App>, http: Arc<Http>) -> Result<()> {
    if app.llm.is_none() { std::future::pending::<()>().await; return Ok(()); }
    loop {
        if let Some(game) = app.store.next_commentary()? {
            let text = ai_text(&app, &game).await;
            if let Err(error) = &text { warn!(%error, "Optional LLM commentary unavailable"); }
            if let Ok(text) = text {
                if let Some((channel, message)) = app.store.message(&game.id)? {
                    // Only content changes; the scoreboard image stays attached.
                    let content = highlights::join(&text, &highlight_text(&app, &game));
                    if ChannelId::new(channel).edit_message(&http, MessageId::new(message), EditMessage::new().content(&content).allowed_mentions(mentions())).await.is_ok() {
                        app.store.commentary_done(&game.id, Some(&text))?;
                    } else { app.store.commentary_done(&game.id, None)?; warn!("Optional commentary edit failed; factual result retained"); }
                } else { app.store.commentary_done(&game.id, None)?; }
            } else { app.store.commentary_done(&game.id, None)?; }
        } else { sleep(Duration::from_secs(5)).await; }
    }
}

/// Explicit one-message preview; never starts a gateway or drains the outbox.
pub async fn post_preview(app: &App, token: &str, image: Vec<u8>, lang: Lang) -> Result<()> {
    let channel = app.channel(false)?;
    ensure!(channel != 0, "Configure a game channel before posting a preview");
    let http = Http::new(token);
    validate_channel(&http, app.cfg.discord.guild_id, channel).await.map_err(|p| anyhow::anyhow!("Preview channel unusable: {p}"))?;
    let message = safe_http(ChannelId::new(channel).send_message(&http, CreateMessage::new()
        .content(lang.t(Msg::PreviewPost))
        .add_file(CreateAttachment::bytes(image, format!("scoreboard.{}", render::IMAGE_EXT)))
        .allowed_mentions(mentions())).await)?;
    info!(message_id = message.id.get(), "Posted one scoreboard preview; no gateway started");
    Ok(())
}

pub async fn probe(token: &str) -> Result<()> {
    if token.is_empty() { return Ok(()); }
    // REST read only: no gateway, application-command writes or messages.
    safe_http(Http::new(token).get_current_user().await)?;
    info!("Discord credentials accepted (read-only probe)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_command_definition_and_reply() {
        let cmd = command("version", Msg::CmdVersion);
        let value = serde_json::to_value(&cmd).unwrap();
        assert_eq!(value["name"], "version");
        assert_eq!(value["description"], "Show bot version");
        assert_eq!(value["description_localizations"]["vi"], "Xem phiên bản bot");

        let reply = Reply::text(format!("rift-recap v{}", env!("CARGO_PKG_VERSION")));
        assert_eq!(reply.text, format!("rift-recap v{}", env!("CARGO_PKG_VERSION")));
        assert!(reply.file.is_none());
    }

    #[test]
    fn command_visibility_matches_specification() {
        for public in ["lastgame", "rank", "version", "nemesis"] {
            assert!(matches!(public, "lastgame" | "rank" | "version" | "nemesis"), "{public} should be public");
        }
        for private in ["players", "config"] {
            assert!(!matches!(private, "lastgame" | "rank" | "version" | "nemesis"), "{private} should be private");
        }
    }
}
