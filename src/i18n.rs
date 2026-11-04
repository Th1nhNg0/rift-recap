//! All user-facing text. Each message has exactly one entry per language, so adding a language
//! (a `Lang` variant plus one column per entry) is enforced by the compiler. Logs stay English.
use serde::Deserialize;
use std::fmt::Display;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang { Vi, En }

impl Lang {
    pub const ALL: [Lang; 2] = [Lang::Vi, Lang::En];
    pub fn code(self) -> &'static str { match self { Lang::Vi => "vi", Lang::En => "en" } }
    pub fn parse(code: &str) -> Option<Self> { Self::ALL.into_iter().find(|l| l.code().eq_ignore_ascii_case(code.trim())) }
    /// Language name for the optional LLM prompt.
    pub fn english_name(self) -> &'static str { match self { Lang::Vi => "Vietnamese", Lang::En => "English" } }
    /// Discord client locale used for slash-command localizations.
    pub fn discord_locale(self) -> &'static str { self.code() }

    pub fn t(self, msg: Msg) -> &'static str { msg.text()[self as usize] }
    /// Fills each `{}` placeholder of a message template in order.
    pub fn f(self, msg: Msg, args: &[&dyn Display]) -> String {
        let mut parts = self.t(msg).split("{}");
        let mut out = parts.next().unwrap_or_default().to_owned();
        for (part, arg) in parts.zip(args.iter().map(|a| a.to_string()).chain(std::iter::repeat(String::new()))) {
            out.push_str(&arg);
            out.push_str(part);
        }
        out
    }

    pub fn queue(self, id: u16) -> String {
        let vi = self == Lang::Vi;
        match id {
            400 => if vi { "Thường (Cấm Chọn)" } else { "Normal Draft" },
            420 => "Solo/Duo",
            430 => if vi { "Thường (Chọn Ẩn)" } else { "Normal Blind" },
            440 => "Flex",
            450 => "ARAM",
            490 => if vi { "Chơi Nhanh" } else { "Quickplay" },
            700 => "Clash",
            900 => "URF",
            1700 => if vi { "Đấu Trường" } else { "Arena" },
            _ => return self.f(Msg::QueueUnknown, &[&id]),
        }.into()
    }
    /// Longer queue name for image headers.
    pub fn queue_title(self, id: u16) -> String {
        let vi = self == Lang::Vi;
        match id {
            420 => if vi { "Xếp hạng Đơn/Đôi" } else { "Ranked Solo/Duo" },
            440 => if vi { "Xếp hạng Linh hoạt" } else { "Ranked Flex" },
            _ => return self.queue(id),
        }.into()
    }
    /// Match-V5 `teamPosition`; empty when unknown.
    pub fn role(self, role: &str) -> &'static str {
        let i = self as usize;
        match role {
            "TOP" => ["Đường trên", "Top"][i],
            "JUNGLE" => ["Rừng", "Jungle"][i],
            "MIDDLE" => ["Đường giữa", "Mid"][i],
            "BOTTOM" => ["Đường dưới", "Bot"][i],
            "UTILITY" => ["Hỗ trợ", "Support"][i],
            _ => "",
        }
    }
    /// League-V4 tier, e.g. EMERALD.
    pub fn tier(self, tier: &str) -> String {
        let i = self as usize;
        match tier {
            "IRON" => ["Sắt", "Iron"][i],
            "BRONZE" => ["Đồng", "Bronze"][i],
            "SILVER" => ["Bạc", "Silver"][i],
            "GOLD" => ["Vàng", "Gold"][i],
            "PLATINUM" => ["Bạch Kim", "Platinum"][i],
            "EMERALD" => ["Lục Bảo", "Emerald"][i],
            "DIAMOND" => ["Kim Cương", "Diamond"][i],
            "MASTER" => ["Cao Thủ", "Master"][i],
            "GRANDMASTER" => ["Đại Cao Thủ", "Grandmaster"][i],
            "CHALLENGER" => ["Thách Đấu", "Challenger"][i],
            other => return other.to_owned(),
        }.into()
    }
    /// Tier and division without LP; apex tiers have no division.
    pub fn rank_name(self, rank: &crate::model::Rank) -> String {
        if matches!(rank.tier.as_str(), "MASTER" | "GRANDMASTER" | "CHALLENGER") || rank.division.is_empty() { self.tier(&rank.tier) }
        else { format!("{} {}", self.tier(&rank.tier), rank.division) }
    }
    pub fn rank(self, rank: &crate::model::Rank) -> String { format!("{} · {} LP", self.rank_name(rank), rank.lp) }
    pub fn team(self, team: u16) -> String {
        match team { 100 => self.t(Msg::TeamBlue).into(), 200 => self.t(Msg::TeamRed).into(), _ => self.f(Msg::TeamNumber, &[&team]) }
    }
    /// Match-V5 team objective keys.
    pub fn objective(self, key: &str) -> String {
        let i = self as usize;
        match key {
            "baron" => "Baron",
            "champion" => ["Hạ gục", "Kills"][i],
            "dragon" => ["Rồng", "Dragons"][i],
            "horde" => ["Sâu Hư Không", "Void Grubs"][i],
            "inhibitor" => ["Nhà lính", "Inhibitors"][i],
            "riftHerald" => ["Sứ Giả", "Rift Herald"][i],
            "tower" => ["Trụ", "Towers"][i],
            "atakhan" => "Atakhan",
            other => return other.to_owned(),
        }.into()
    }
    /// Timeline monster type and optional subtype, e.g. DRAGON/FIRE_DRAGON.
    pub fn monster(self, kind: &str, subtype: &str) -> String {
        let i = self as usize;
        let dragon = match subtype {
            "AIR_DRAGON" => Some(["Rồng Gió", "Cloud Drake"]),
            "EARTH_DRAGON" => Some(["Rồng Đất", "Mountain Drake"]),
            "FIRE_DRAGON" => Some(["Rồng Lửa", "Infernal Drake"]),
            "WATER_DRAGON" => Some(["Rồng Nước", "Ocean Drake"]),
            "HEXTECH_DRAGON" => Some(["Rồng Hextech", "Hextech Drake"]),
            "CHEMTECH_DRAGON" => Some(["Rồng Hóa Kỹ", "Chemtech Drake"]),
            "ELDER_DRAGON" => Some(["Rồng Ngàn Tuổi", "Elder Dragon"]),
            _ => None,
        };
        if let Some(names) = dragon { return names[i].into(); }
        match kind {
            "DRAGON" => ["Rồng", "Dragon"][i],
            "BARON_NASHOR" => "Baron Nashor",
            "RIFTHERALD" => ["Sứ Giả", "Rift Herald"][i],
            "HORDE" => ["Sâu Hư Không", "Void Grub"][i],
            "ATAKHAN" => "Atakhan",
            other => return other.to_owned(),
        }.into()
    }
    pub fn building(self, kind: &str, lane: &str) -> String {
        let i = self as usize;
        let building = match kind { "TOWER_BUILDING" => ["Trụ", "Tower"][i], "INHIBITOR_BUILDING" => ["Nhà lính", "Inhibitor"][i], other => other };
        let lane = match lane { "TOP_LANE" => ["đường trên", "top"][i], "MID_LANE" => ["đường giữa", "mid"][i], "BOT_LANE" => ["đường dưới", "bot"][i], _ => "" };
        if lane.is_empty() { building.into() } else { format!("{building} ({lane})") }
    }
}

macro_rules! catalog {
    ($($key:ident: $vi:literal, $en:literal;)*) => {
        #[derive(Clone, Copy, Debug)]
        pub enum Msg { $($key),* }
        impl Msg {
            /// Indexed by `Lang as usize`.
            const fn text(self) -> [&'static str; 2] { match self { $(Msg::$key => [$vi, $en]),* } }
        }
    };
}

catalog! {
    // Shared vocabulary
    Win: "Thắng", "Win";
    Loss: "Thua", "Loss";
    TeamBlue: "Đội xanh", "Blue team";
    TeamRed: "Đội đỏ", "Red team";
    TeamNumber: "Đội {}", "Team {}";
    QueueUnknown: "Hàng chờ {}", "Queue {}";
    LpUnknown: "LP chưa rõ", "LP change unknown";
    Unranked: "Chưa xếp hạng", "Unranked";
    RankUnavailable: "Không lấy được hạng", "Rank unavailable";
    CsPerMin: "{} CS/phút", "{} CS/min";
    VisionPerMin: "{} tầm nhìn/phút", "{} vision/min";
    KillParticipation: "{}% tham gia hạ gục", "{}% KP";
    DamageShort: "{} sát thương", "{} DMG";

    // Text results and commentary
    RemakeExcluded: "Trận chơi lại; không tính vào thống kê.", "Remake; excluded from statistics.";
    Standout: "Nổi bật", "Standout";
    KeyPlayer: "Nhân vật chính", "Key player";
    LeastImpact: "Mờ nhạt nhất", "Least impact";
    ReasonTeamDamage: "{} sát thương đội", "{} of team damage";
    ReasonKp: "tham gia {} số mạng", "{} kill participation";
    ReasonObjective: "{}k sát thương mục tiêu", "{}k objective damage";
    ReasonTaken: "chịu {}k sát thương", "{}k damage taken";
    ReasonFewDeaths: "chỉ chết {} lần", "only {} death(s)";
    ReasonDeaths: "chết {} lần", "{} deaths";
    ReasonSteady: "ổn định, không có chỉ số vượt trội", "steady, no single standout stat";
    ReasonLowest: "đóng góp tổng thể thấp nhất đội", "lowest overall contribution on the team";

    // Timeline events

    // Discord messages
    WrongGuild: "Bot này chỉ hoạt động trong máy chủ đã cấu hình.", "This bot only works in its configured server.";
    Busy: "Bot đang bận; vui lòng thử lại sau giây lát.", "Busy; please try again shortly.";
    CommandFailed: "Có lỗi xảy ra; vui lòng thử lại sau.", "Something went wrong; please try again later.";
    NoData: "Chưa có dữ liệu.", "No data yet.";
    FullAttached: "Nội dung đầy đủ trong tệp đính kèm.", "Full response attached.";
    NoGames: "Chưa có trận nào được ghi nhận.", "No tracked games yet.";
    RosterTitle: "Đang theo dõi {} người chơi:", "Tracking {} players:";
    AccountUnresolved: "Chưa tìm được tài khoản", "Account not resolved";
    ManageServerRequired: "Cần quyền Quản lý máy chủ.", "Manage Server permission required.";
    AdminRequired: "Chỉ quản trị viên mới dùng được lệnh này.", "Administrator permission required.";
    ChannelSet: "Đã cập nhật kênh trận đấu: {}", "Game channel updated: {}";
    ReportStatus: "Báo cáo tự động: {} · kênh: {}", "Automatic reports: {} · channel: {}";
    Enabled: "đang bật", "enabled";
    Disabled: "đã tắt", "disabled";
    NotSet: "chưa đặt", "not set";
    BadRiotId: "Riot ID không hợp lệ. Dùng dạng Tên#Tag.", "Invalid Riot ID. Use Name#Tag.";
    PlayerNotFound: "Không tìm thấy Riot ID này. Kiểm tra lại tên và tag.", "Riot ID not found. Check the name and tag.";
    PlayerExists: "Người chơi này đã có trong danh sách theo dõi.", "This player is already tracked.";
    RosterFull: "Danh sách theo dõi đã đủ 100 người.", "The roster is full (100 players).";
    PlayerAdded: "Đã thêm {}. Đang theo dõi {} người chơi.", "Added {}. Tracking {} players.";
    PlayerRemoved: "Đã xóa {}. Đang theo dõi {} người chơi.", "Removed {}. Tracking {} players.";
    PlayerNotTracked: "Người chơi này không có trong danh sách theo dõi.", "This player is not tracked.";
    RankChannelMissing: "Hãy đặt kênh báo cáo trước (/config rankchannel).", "Configure a report channel first (/config rankchannel).";
    ReportQueued: "Đã xếp hàng báo cáo.", "Report queued.";
    NoGamesInPeriod: "Chưa có trận nào trong kỳ.", "No games in this period.";
    RiotUnavailable: "Riot tạm thời không khả dụng; vui lòng thử lại sau.", "Riot is temporarily unavailable; please try again later.";
    ReportAttached: "Báo cáo đính kèm.", "Report attached.";
    ChannelNotText: "Kênh phải là kênh văn bản hoặc thông báo trong máy chủ này.", "The channel must be a text or announcement channel in this server.";
    ChannelMissingPerms: "Bot thiếu quyền trong kênh này: {}", "The bot lacks permissions in this channel: {}";
    ChannelUnreachable: "Bot không truy cập được kênh này.", "The bot cannot access this channel.";
    PermViewChannel: "Xem kênh", "View Channel";
    PermSendMessages: "Gửi tin nhắn", "Send Messages";
    PermAttachFiles: "Đính kèm tập tin", "Attach Files";

    // Slash-command descriptions (English is Discord's default; others are localizations)
    CmdLastgame: "Xem trận gần nhất", "Most recent tracked match";
    CmdRank: "Hạng hiện tại của mọi người", "Current ranks of tracked players";
    CmdPlayers: "Danh sách người chơi đang theo dõi", "Tracked player roster";
    CmdVersion: "Xem phiên bản bot", "Show bot version";
    CmdConfig: "Cài đặt bot", "Bot settings";
    CmdChannel: "Đặt kênh đăng trận đấu", "Set the game channel";
    CmdRankchannel: "Bật/tắt hoặc đổi kênh báo cáo hạng", "Enable/disable or redirect rank reports";
    CmdAddplayer: "Thêm người chơi (chỉ quản trị viên)", "Track an account (administrator)";
    CmdRemoveplayer: "Xóa người chơi (chỉ quản trị viên)", "Stop tracking an account (administrator)";
    CmdNemesis: "Tướng khắc chế từng người chơi", "Champions each player loses to most";
    OptPlayer: "Người chơi (không bắt buộc)", "Optional tracked player";
    CmdRankreport: "Đăng báo cáo hạng ngay", "Post a rank report now";
    OptChannel: "Kênh văn bản", "Target text channel";
    OptEnabled: "Bật báo cáo tự động", "Enable automatic reports";
    OptRiotId: "Tên#Tag", "Name#Tag";
    OptNickname: "Tên hiển thị (không bắt buộc)", "Optional display name";
    OptPeriod: "Khoảng thời gian", "Report window";
    ChoiceDaily: "Hằng ngày", "Daily";
    ChoiceWeekly: "Hằng tuần", "Weekly";

    // Reports
    DailyReport: "Tổng kết ngày", "Daily report";
    WeeklyReport: "Tổng kết tuần", "Weekly report";
    DistinctMatches: "Số trận riêng biệt: {}", "Distinct matches: {}";
    RankSection: "Hạng (Solo/Duo và Flex)", "Rank changes (Solo/Duo and Flex)";
    RankNoData: "Chưa có dữ liệu hạng", "Rank unavailable";
    PartialLp: "* LP chỉ tính từ mốc đầu tiên được ghi nhận; có thể thiếu đầu kỳ.", "* LP uses the first observed baseline; the beginning of the period may be missing.";
    StatsSection: "Thống kê (không tính chơi lại)", "Statistics (remakes excluded)";
    GamesWins: "{} trận / {} thắng", "{} G / {} W";
    DamagePerGame: "{} sát thương/trận", "{} DMG/game";
    SmallSample: "ít trận", "small sample";
    MonthlyReport: "Tổng kết tháng {}", "Monthly report · {}";
    MonthFormat: "%m/%Y", "%B %Y";
    ReportRangeFormat: "%d/%m/%Y", "%d %b %Y";
    MatchesCount: "{} trận", "{} matches";
    GroupLp: "TỔNG LP NHÓM", "GROUP LP";
    MvpMonth: "MVP tháng {}", "MVP of {}";
    MvpLine: "{} điểm · {} trận · {} thắng", "{} rating · {} games · {} wins";
    HonoursSection: "Danh hiệu", "Honours";
    AwKills: "Hạ gục nhiều nhất", "Most kills";
    AwDeaths: "Chết nhiều nhất", "Most deaths";
    AwDamage: "Sát thương cao nhất", "Top damage";
    AwFarm: "Vua farm", "Farm king";
    AwVision: "Mắt thần", "Eagle eye";
    AwWinRate: "Tỷ lệ thắng cao nhất", "Best win rate";
    FmtKills: "{} hạ gục", "{} kills";
    FmtDeaths: "{} lần chết", "{} deaths";
    FmtWinRate: "{}% thắng ({} trận)", "{}% wins ({} games)";
    ChoiceMonthly: "Hằng tháng", "Monthly";
    AwStreak: "Chuỗi thắng dài nhất", "Longest win streak";
    AwNemesis: "Khắc tinh của tháng", "Nemesis of the month";
    AwVictim: "Con mồi yêu thích", "Favourite victim";
    FmtStreak: "{} trận thắng liên tiếp", "{} wins in a row";
    FmtMatchup: "{} · {}–{}", "{} · {}–{}";

    // Highlights under a game's scoreboard
    HlWinStreak: "🔥 {} đang có chuỗi {} trận thắng", "🔥 {} is on a {}-game win streak";
    HlLossStreak: "📉 {} đã thua {} trận liên tiếp, ai đó hỏi thăm đi", "📉 {} has lost {} in a row, someone check on them";
    HlWinStreakEnded: "🧊 {} đứt chuỗi {} trận thắng", "🧊 {}'s {}-game win streak is over";
    HlLossStreakEnded: "🌤️ {} đã ngắt chuỗi {} trận thua", "🌤️ {} snapped a {}-game losing streak";
    HlPromoted: "🎉 {} lên {} ({})", "🎉 {} reached {} ({})";
    HlDemoted: "💀 {} rơi xuống {} ({})", "💀 {} dropped to {} ({})";
    HlPenta: "🏆 PENTAKILL! {} với {}", "🏆 PENTAKILL by {} on {}";
    HlFirstQuadra: "⚡ Quadra kill đầu tiên được ghi nhận của {} ({})", "⚡ First recorded quadra kill for {} ({})";
    HlChampionGames: "🎖️ {}: trận thứ {} với {}", "🎖️ {}: game {} on {}";
    HlBestKills: "📈 Kỷ lục mới của {}: {} hạ gục", "📈 New personal best for {}: {} kills";
    HlBestDamage: "📈 Kỷ lục mới của {}: {} sát thương", "📈 New personal best for {}: {} damage";
    HlNemesis: "😈 {} lại thua {} ({}–{})", "😈 {} lost to {} again ({}–{})";

    // Nemesis command
    NemesisTitle: "😈 Khắc tinh (tối thiểu {} trận đối đầu, trong {} ngày gần nhất)", "😈 Nemeses (at least {} games against, last {} days)";
    NemesisNone: "chưa đủ trận", "not enough games yet";
    NemesisVictim: "🍖 con mồi: {}", "🍖 favourite victim: {}";

    // Images
    ImgVictory: "CHIẾN THẮNG", "VICTORY";
    ImgDefeat: "THẤT BẠI", "DEFEAT";
    ImgResult: "KẾT QUẢ", "RESULT";
    ImgRemake: "CHƠI LẠI", "REMAKE";
    ImgWin: "THẮNG", "WIN";
    ImgLoss: "THUA", "LOSS";
    ImgPreview: "XEM TRƯỚC · TRẬN THẬT", "PREVIEW · REAL MATCH";
    PreviewPost: "**Xem trước bảng điểm từ một trận đấu thật.** Không khởi động gateway hay LLM.", "**Scoreboard preview from a real match.** No gateway session or LLM was started.";
    ImgMvp: "MVP", "MVP";
    ImgAce: "ACE", "ACE";
    ImgWorst: "CHƠI TỆ", "WORST";
    ColRecord: "TRẬN", "RECORD";
    ColDamageGame: "SÁT THƯƠNG / TRẬN", "DAMAGE / GAME";
    ColCsMin: "CS / PHÚT", "CS / MIN";
    ColVisionMin: "TẦM NHÌN / PHÚT", "VISION / MIN";
    ColLpChange: "THAY ĐỔI LP", "LP CHANGE";
    BlockClimbed: "TĂNG HẠNG", "CLIMBED";
    BlockDropped: "TỤT HẠNG", "DROPPED";
    BlockUnknown: "CHƯA RÕ LP", "LP UNKNOWN";
    ImgGold: "VÀNG", "GOLD";
    ImgAvgRank: "HẠNG TRUNG BÌNH", "AVG RANK";
    ImgPerfectKda: "KDA hoàn hảo", "Perfect KDA";
    ImgDamage: "SÁT THƯƠNG", "DAMAGE";
    ImgTaken: "NHẬN VÀO", "TAKEN";
    ImgVision: "TẦM NHÌN", "VISION";
    ImgItems: "TRANG BỊ", "ITEMS";
    ImgDateFormat: "%d/%m/%Y  ·  %H:%M %Z", "%d %b %Y  ·  %H:%M %Z";
}
