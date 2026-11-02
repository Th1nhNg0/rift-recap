use crate::model::{Match, Notice, Player, Rank, Snapshot, now};
use anyhow::{Context, Result, ensure};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{fs, io::{Read, Write}, path::Path, sync::{Mutex, MutexGuard}};

pub struct Store(Mutex<Connection>);
pub struct Delivery { pub id: i64, pub notice: Notice }
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) { fs::create_dir_all(parent).context("Cannot create data directory")?; }
        let db = Connection::open(path).context("Cannot open SQLite database")?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!(version <= 1, "Database is newer than this application");
        if version == 0 {
            let tables: i64 = db.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'", [], |r| r.get(0))?;
            ensure!(tables == 0, "Unrecognized database; use a fresh file, not a legacy database");
        }
        db.execute_batch("
            PRAGMA journal_mode=WAL;
            PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS players(key TEXT PRIMARY KEY,data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS baseline(puuid TEXT,match_id TEXT,at INTEGER,PRIMARY KEY(puuid,match_id));
            CREATE TABLE IF NOT EXISTS seen(id TEXT PRIMARY KEY,at INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS matches(id TEXT PRIMARY KEY,ended_at INTEGER NOT NULL,data TEXT NOT NULL,commented INTEGER NOT NULL DEFAULT 0);
            CREATE INDEX IF NOT EXISTS matches_time ON matches(ended_at);
            CREATE TABLE IF NOT EXISTS ranks(puuid TEXT,queue INTEGER,at INTEGER,data TEXT NOT NULL,PRIMARY KEY(puuid,queue,at));
            CREATE TABLE IF NOT EXISTS raw_matches(id TEXT PRIMARY KEY,ended_at INTEGER NOT NULL,data BLOB NOT NULL);
            CREATE INDEX IF NOT EXISTS raw_matches_time ON raw_matches(ended_at);
            DROP TABLE IF EXISTS pending;
            CREATE TABLE IF NOT EXISTS outbox(id INTEGER PRIMARY KEY,unique_key TEXT UNIQUE,at INTEGER NOT NULL,data TEXT NOT NULL,retry_at INTEGER NOT NULL DEFAULT 0,attempts INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS messages(game_id TEXT PRIMARY KEY,channel_id TEXT NOT NULL,message_id TEXT NOT NULL,finished INTEGER NOT NULL,at INTEGER NOT NULL);
            DROP TABLE IF EXISTS timelines;
            PRAGMA user_version=1;
        ")?;
        Ok(Self(Mutex::new(db)))
    }
    fn db(&self) -> Result<MutexGuard<'_, Connection>> { self.0.lock().map_err(|_| anyhow::anyhow!("Database lock poisoned")) }
    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self.db()?.query_row("SELECT value FROM settings WHERE key=?", [key], |r| r.get(0)).optional()?)
    }
    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.db()?.execute("INSERT INTO settings VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value])?;
        Ok(())
    }
    pub fn bind(&self, key: &str, value: &str) -> Result<()> {
        if let Some(old) = self.setting(key)? { ensure!(old == value, "Database platform/guild binding differs; use a separate database"); }
        else { self.set(key, value)?; }
        Ok(())
    }
    pub fn seed(&self, path: &Path) -> Result<()> {
        if self.setting("roster_seeded")?.is_some() { return Ok(()); }
        let text = fs::read_to_string(path).context("Cannot read initial player roster")?;
        let mut players: Vec<Player> = serde_json::from_str(&text).map_err(|_| anyhow::anyhow!("Invalid player roster JSON"))?;
        ensure!(players.len() <= 100, "Maximum roster size is 100");
        let mut db = self.db()?;
        let tx = db.transaction()?;
        for p in &mut players {
            p.validate()?;
            p.puuid.clear(); p.baselined = false;
            tx.execute("INSERT INTO players VALUES(?,?)", params![p.riot_id().to_lowercase(), encode(p)?])?;
        }
        tx.execute("INSERT INTO settings VALUES('roster_seeded','1')", [])?;
        tx.commit()?;
        Ok(())
    }
    pub fn players(&self) -> Result<Vec<Player>> {
        let db = self.db()?;
        let mut q = db.prepare("SELECT data FROM players ORDER BY key")?;
        let rows = q.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|s| decode(&s?)).collect()
    }
    pub fn add_player(&self, p: &Player) -> Result<()> {
        p.validate()?;
        let db = self.db()?;
        let count: i64 = db.query_row("SELECT COUNT(*) FROM players", [], |r| r.get(0))?;
        ensure!(count < 100, "Maximum roster size reached");
        let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM players WHERE key=?)", [p.riot_id().to_lowercase()], |r| r.get(0))?;
        ensure!(!exists, "Player already tracked");
        db.execute("INSERT INTO players VALUES(?,?)", params![p.riot_id().to_lowercase(), encode(p)?])?;
        Ok(())
    }
    // A stale tracking sweep must not recreate an administratively removed player.
    pub fn update_player(&self, p: &Player) -> Result<()> {
        self.db()?.execute("UPDATE players SET data=? WHERE key=?", params![encode(p)?, p.riot_id().to_lowercase()])?;
        Ok(())
    }
    pub fn remove_player(&self, id: &str) -> Result<bool> {
        Ok(self.db()?.execute("DELETE FROM players WHERE key=?", [id.trim().to_lowercase()])? > 0)
    }
    pub fn baseline(&self, p: &Player, ids: &[String]) -> Result<()> {
        let mut db = self.db()?;
        let tx = db.transaction()?;
        for id in ids { tx.execute("INSERT OR IGNORE INTO baseline VALUES(?,?,?)", params![p.puuid, id, now()])?; }
        let mut p = p.clone(); p.baselined = true;
        tx.execute("UPDATE players SET data=? WHERE key=?", params![encode(&p)?, p.riot_id().to_lowercase()])?;
        tx.commit()?;
        Ok(())
    }
    pub fn skip(&self, puuid: &str, id: &str) -> Result<bool> {
        Ok(self.db()?.query_row("SELECT EXISTS(SELECT 1 FROM seen WHERE id=?1) OR EXISTS(SELECT 1 FROM baseline WHERE puuid=?2 AND match_id=?1)", params![id, puuid], |r| r.get(0))?)
    }
    pub fn seen(&self, id: &str) -> Result<bool> {
        Ok(self.db()?.query_row("SELECT EXISTS(SELECT 1 FROM seen WHERE id=?)", [id], |r| r.get(0))?)
    }
    pub fn mark_seen(&self, id: &str) -> Result<()> {
        self.db()?.execute("INSERT OR IGNORE INTO seen VALUES(?,?)", params![id, now()])?;
        Ok(())
    }
    pub fn snapshot(&self, puuid: &str, rank: &Rank, at: i64) -> Result<()> {
        self.db()?.execute("INSERT OR REPLACE INTO ranks VALUES(?,?,?,?)", params![puuid, rank.queue, at, encode(rank)?])?;
        Ok(())
    }
    pub fn rank_at(&self, puuid: &str, queue: u16, at: i64) -> Result<Option<Snapshot>> {
        let row: Option<(i64, String)> = self.db()?.query_row("SELECT at,data FROM ranks WHERE puuid=? AND queue=? AND at<=? ORDER BY at DESC LIMIT 1", params![puuid, queue, at], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        row.map(|(at, text)| Ok(Snapshot { at, rank: decode(&text)? })).transpose()
    }
    pub fn first_rank(&self, puuid: &str, queue: u16, start: i64, end: i64) -> Result<Option<Snapshot>> {
        let row: Option<(i64, String)> = self.db()?.query_row("SELECT at,data FROM ranks WHERE puuid=? AND queue=? AND at>? AND at<=? ORDER BY at LIMIT 1", params![puuid, queue, start, end], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        row.map(|(at, text)| Ok(Snapshot { at, rank: decode(&text)? })).transpose()
    }
    /// Unix time of the last game whose post-game rank lookup succeeded for this account, or `None`
    /// before the first one. A snapshot older than this may predate games the bot never accounted
    /// for, so a match only trusts a before-rank taken at or after it. Unlike a process start time
    /// it survives restarts.
    pub fn rank_checked(&self, puuid: &str) -> Result<Option<i64>> {
        Ok(self.db()?.query_row("SELECT value FROM settings WHERE key=?", [format!("rank-checked/{puuid}")], |r| r.get::<_, String>(0)).optional()?.and_then(|t| t.parse().ok()))
    }
    /// Record a confirmed rank lookup. The marker never moves backwards, so a late-arriving old
    /// match cannot widen the window for a newer one.
    pub fn mark_rank_checked(&self, puuid: &str, at: i64) -> Result<()> {
        self.db()?.execute("INSERT INTO settings(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE CAST(excluded.value AS INTEGER)>CAST(settings.value AS INTEGER)", params![format!("rank-checked/{puuid}"), at])?;
        Ok(())
    }
    pub fn finish(&self, m: &Match) -> Result<()> {
        let mut db = self.db()?; let tx = db.transaction()?;
        let inserted = tx.execute("INSERT OR IGNORE INTO seen VALUES(?,?)", params![m.id, now()])?;
        if inserted > 0 {
            tx.execute("INSERT INTO matches(id,ended_at,data) VALUES(?,?,?)", params![m.id, m.ended_at, encode(m)?])?;
            tx.execute("INSERT OR IGNORE INTO outbox(unique_key,at,data) VALUES(?,?,?)", params![format!("result/{}", m.id), now(), encode(&Notice::Result(m.clone()))?])?;
        }
        tx.commit()?; Ok(())
    }
    /// Keep the untouched Riot payload (gzip) for a match this store already holds, so later
    /// features can derive new statistics without refetching or a schema migration. Best effort:
    /// a game nobody tracks has no row in `matches`, and its payload is dropped.
    pub fn save_raw(&self, m: &Match, raw: &Value) -> Result<()> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&serde_json::to_vec(raw)?)?;
        let blob = encoder.finish()?;
        self.db()?.execute("INSERT OR REPLACE INTO raw_matches(id,ended_at,data) SELECT id,ended_at,? FROM matches WHERE id=?", params![blob, m.id])?;
        Ok(())
    }
    /// The stored Riot payload for one match, or `None` when it was never kept.
    pub fn raw_match(&self, id: &str) -> Result<Option<Value>> {
        let row: Option<Vec<u8>> = self.db()?.query_row("SELECT data FROM raw_matches WHERE id=?", [id], |r| r.get(0)).optional()?;
        let Some(blob) = row else { return Ok(None) };
        let mut text = Vec::new();
        GzDecoder::new(blob.as_slice()).read_to_end(&mut text)?;
        Ok(Some(serde_json::from_slice(&text).map_err(|_| anyhow::anyhow!("Invalid stored match payload"))?))
    }
    /// Silently drop a game that no longer concerns any tracked player.
    pub fn discard(&self, id: &str) -> Result<()> {
        let mut db = self.db()?; let tx = db.transaction()?;
        tx.execute("INSERT OR IGNORE INTO seen VALUES(?,?)", params![id, now()])?;
        tx.commit()?; Ok(())
    }
    pub fn get_match(&self, id: Option<&str>) -> Result<Option<Match>> {
        let row: Option<String> = if let Some(id) = id {
            self.db()?.query_row("SELECT data FROM matches WHERE id=?", [id], |r| r.get(0)).optional()?
        } else { self.db()?.query_row("SELECT data FROM matches ORDER BY ended_at DESC LIMIT 1", [], |r| r.get(0)).optional()? };
        row.map(|s| decode(&s)).transpose()
    }
    pub fn matches(&self, start: i64, end: i64) -> Result<Vec<Match>> {
        let db = self.db()?;
        let mut q = db.prepare("SELECT data FROM matches WHERE ended_at>? AND ended_at<=? ORDER BY ended_at")?;
        q.query_map(params![start, end], |r| r.get::<_, String>(0))?.map(|s| decode(&s?)).collect()
    }
    /// Visit normalized matches in the same order as `matches`, retaining only
    /// one decoded row. The callback must not re-enter this store: its database
    /// mutex is held until the scan returns.
    pub fn visit_matches(&self, start: i64, end: i64, mut visit: impl FnMut(&Match)) -> Result<()> {
        let db = self.db()?;
        let mut q = db.prepare("SELECT data FROM matches WHERE ended_at>? AND ended_at<=? ORDER BY ended_at")?;
        let rows = q.query_map(params![start, end], |r| r.get::<_, String>(0))?;
        for row in rows {
            let game: Match = decode(&row?)?;
            visit(&game);
        }
        Ok(())
    }
    pub fn queue_report(&self, key: &str, notice: &Notice) -> Result<()> {
        self.db()?.execute("INSERT OR IGNORE INTO outbox(unique_key,at,data) VALUES(?,?,?)", params![key, now(), encode(notice)?])?;
        Ok(())
    }
    /// Queues a date's reports and marks the date done in one transaction.
    pub fn schedule_reports(&self, day: &str, notices: &[(String, Notice)]) -> Result<()> {
        let mut db = self.db()?; let tx = db.transaction()?;
        let old: Option<String> = tx.query_row("SELECT value FROM settings WHERE key='last_report'", [], |r| r.get(0)).optional()?;
        if old.as_deref() != Some(day) {
            for (key, notice) in notices {
                tx.execute("INSERT OR IGNORE INTO outbox(unique_key,at,data) VALUES(?,?,?)", params![key, now(), encode(notice)?])?;
            }
            tx.execute("INSERT OR REPLACE INTO settings VALUES('last_report',?)", [day])?;
        }
        tx.commit()?; Ok(())
    }
    pub fn next_delivery(&self) -> Result<Option<Delivery>> {
        let row: Option<(i64, String)> = self.db()?.query_row("SELECT id,data FROM outbox WHERE retry_at<=? ORDER BY id LIMIT 1", [now()], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        row.map(|(id, s)| Ok(Delivery { id, notice: decode(&s)? })).transpose()
    }
    pub fn defer_delivery(&self, id: i64) -> Result<()> {
        self.db()?.execute("UPDATE outbox SET attempts=attempts+1,retry_at=? + MIN(900, 5 * (1 << MIN(attempts,7))) WHERE id=?", params![now(), id])?;
        Ok(())
    }
    pub fn ack(&self, id: i64, game: Option<(&str, u64, u64)>) -> Result<()> {
        let mut db = self.db()?; let tx = db.transaction()?;
        if let Some((gid, channel, message)) = game {
            tx.execute("INSERT INTO messages VALUES(?,?,?,?,?) ON CONFLICT(game_id) DO UPDATE SET channel_id=excluded.channel_id,message_id=excluded.message_id,finished=excluded.finished,at=excluded.at", params![gid, channel.to_string(), message.to_string(), 1, now()])?;
        }
        tx.execute("DELETE FROM outbox WHERE id=?", [id])?;
        tx.commit()?; Ok(())
    }
    pub fn message(&self, id: &str) -> Result<Option<(u64, u64)>> {
        let row: Option<(String, String)> = self.db()?.query_row("SELECT channel_id,message_id FROM messages WHERE game_id=?", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        row.map(|(c, m)| Ok((c.parse()?, m.parse()?))).transpose()
    }
    pub fn next_commentary(&self) -> Result<Option<Match>> {
        let row: Option<String> = self.db()?.query_row("SELECT m.data FROM matches m JOIN messages d ON d.game_id=m.id WHERE m.commented=0 AND d.finished=1 AND m.ended_at>? ORDER BY m.ended_at DESC LIMIT 1", [now() - 10800], |r| r.get(0)).optional()?;
        row.map(|s| decode(&s)).transpose()
    }
    pub fn commentary_done(&self, id: &str, text: Option<&str>) -> Result<()> {
        let mut db = self.db()?; let tx = db.transaction()?;
        if let Some(text) = text { tx.execute("INSERT OR REPLACE INTO settings VALUES(?,?)", params![format!("commentary/{id}"), text])?; }
        tx.execute("UPDATE matches SET commented=1 WHERE id=?", [id])?;
        tx.commit()?; Ok(())
    }
    /// AI lines of the newest commented games (excluding `skip`), so the model can avoid repeating itself.
    pub fn recent_commentary(&self, skip: &str, limit: usize) -> Result<Vec<String>> {
        let db = self.db()?;
        let mut q = db.prepare("SELECT s.value FROM settings s JOIN matches m ON s.key='commentary/' || m.id WHERE m.id<>? ORDER BY m.ended_at DESC LIMIT ?")?;
        Ok(q.query_map(params![skip, limit as i64], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn prune(&self, days: u32) -> Result<()> {
        let cutoff = now() - i64::from(days) * 86400;
        let mut db = self.db()?; let tx = db.transaction()?;
        // Outbox JSON references old matches; preserve history until its notification is acknowledged.
        tx.execute("DELETE FROM matches WHERE ended_at<? AND NOT EXISTS(SELECT 1 FROM outbox WHERE unique_key='result/' || matches.id)", [cutoff])?;
        // Raw payloads follow the same retention as the matches they belong to.
        tx.execute("DELETE FROM raw_matches WHERE ended_at<? OR id NOT IN(SELECT id FROM matches)", [cutoff])?;
        tx.execute("DELETE FROM ranks WHERE at<? AND (puuid,queue,at) NOT IN (SELECT puuid,queue,MAX(at) FROM ranks GROUP BY puuid,queue)", [cutoff])?;
        tx.execute("DELETE FROM baseline WHERE at<?", [cutoff])?;
        tx.execute("DELETE FROM seen WHERE at<? AND NOT EXISTS(SELECT 1 FROM outbox WHERE unique_key='result/' || seen.id)", [cutoff])?;
        tx.execute("DELETE FROM messages WHERE at<? AND NOT EXISTS(SELECT 1 FROM outbox WHERE unique_key='result/' || messages.game_id)", [cutoff])?;
        tx.execute("DELETE FROM settings WHERE key LIKE 'commentary/%' AND substr(key,12) NOT IN (SELECT id FROM matches)", [])?;
        tx.commit()?;
        db.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")?;
        Ok(())
    }
}
fn encode<T: Serialize>(value: &T) -> Result<String> { Ok(serde_json::to_string(value)?) }
fn decode<T: DeserializeOwned>(text: &str) -> Result<T> { serde_json::from_str(text).map_err(|_| anyhow::anyhow!("Invalid stored data")) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_binding_and_rank_history() -> Result<()> {
        let store = Store::open(Path::new(":memory:"))?;
        store.bind("platform", "euw1")?;
        store.bind("platform", "euw1")?;
        assert!(store.bind("platform", "na1").is_err());
        assert_eq!(store.setting("platform")?.as_deref(), Some("euw1"));
        let mut rank = Rank { queue: 420, tier: "GOLD".into(), division: "II".into(), lp: 10 };
        for at in 1..=100 {
            rank.lp = at;
            store.snapshot("player", &rank, at)?;
        }
        assert!(store.rank_at("player", 420, 0)?.is_none());
        assert_eq!(store.rank_at("player", 420, 75)?.unwrap().rank.lp, 75);
        assert_eq!(store.first_rank("player", 420, 50, 90)?.unwrap().at, 51);
        assert!(store.rank_at("player", 440, 100)?.is_none());
        Ok(())
    }

    #[test]
    fn match_and_delivery_are_atomic_and_idempotent() -> Result<()> {
        let store = Store::open(Path::new(":memory:"))?;
        let game = Match { id: "TEST_1".into(), queue: 420, started_at: now() - 1800, ended_at: now(), duration: 1800,
            remake: false, participants: Vec::new(), ..Default::default() };
        store.finish(&game)?;
        store.finish(&game)?;
        assert!(store.seen(&game.id)?);
        assert_eq!(store.get_match(None)?.unwrap().id, game.id);
        assert_eq!(store.matches(game.started_at, game.ended_at)?.len(), 1);
        let delivery = store.next_delivery()?.unwrap();
        assert!(matches!(&delivery.notice, Notice::Result(m) if m.id == game.id));
        store.ack(delivery.id, Some((&game.id, 123, 456)))?;
        assert!(store.next_delivery()?.is_none());
        assert_eq!(store.message(&game.id)?, Some((123, 456)));
        assert_eq!(store.next_commentary()?.unwrap().id, game.id);
        store.commentary_done(&game.id, Some("commentary"))?;
        assert!(store.next_commentary()?.is_none());
        assert_eq!(store.recent_commentary("different-game", 6)?, ["commentary"]);
        Ok(())
    }

    #[test]
    fn rank_marker_only_moves_forward() -> Result<()> {
        let store = Store::open(Path::new(":memory:"))?;
        assert!(store.rank_checked("puuid")?.is_none());
        store.mark_rank_checked("puuid", 200)?;
        assert_eq!(store.rank_checked("puuid")?, Some(200));
        // A late-arriving old match must not widen the window for a newer one.
        store.mark_rank_checked("puuid", 100)?;
        assert_eq!(store.rank_checked("puuid")?, Some(200));
        store.mark_rank_checked("puuid", 300)?;
        assert_eq!(store.rank_checked("puuid")?, Some(300));
        // Another account is unaffected.
        assert!(store.rank_checked("other")?.is_none());
        Ok(())
    }

    #[test]
    fn raw_payload_is_kept_only_for_stored_matches() -> Result<()> {
        let store = Store::open(Path::new(":memory:"))?;
        let game = Match { id: "TEST_RAW".into(), ended_at: now(), duration: 1800, ..Default::default() };
        let raw = serde_json::json!({"metadata":{"matchId":"TEST_RAW"},"info":{"gameDuration":1800,"participants":[{"participantId":1,"challenges":{"soloKills":2}}]}});
        // A game the store never accepted keeps no payload.
        store.save_raw(&game, &raw)?;
        assert!(store.raw_match(&game.id)?.is_none());
        store.finish(&game)?;
        store.save_raw(&game, &raw)?;
        assert_eq!(store.raw_match(&game.id)?.unwrap(), raw);
        assert!(store.raw_match("MISSING")?.is_none());
        Ok(())
    }
    #[test]
    fn failed_match_scan_releases_database_lock() -> Result<()> {
        let store = Store::open(Path::new(":memory:"))?;
        store.db()?.execute("INSERT INTO matches(id,ended_at,data) VALUES('broken',10,'not-json')", [])?;
        assert!(store.visit_matches(0, 20, |_| panic!("invalid row must not reach callback")).is_err());
        store.set("after_scan", "available")?;
        assert_eq!(store.setting("after_scan")?.as_deref(), Some("available"));
        let mut count = 0;
        store.visit_matches(10, 20, |_| count += 1)?;
        assert_eq!(count, 0);
        Ok(())
    }
}
