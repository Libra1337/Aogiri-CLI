//! When the paired phones are notified, and how loudly (docs/daemon-protocol.md,
//! "Notifications"). Four levels: `log` (nothing is sent), `silent` (into the
//! notification list without a sound), `normal` and `urgent` (shown at once,
//! even in quiet hours). Waiting for an approval and a session error are
//! always sent and do not count; everything else counts toward a daily
//! budget (20 by default) and is `silent` during quiet hours (23:00-08:00 by
//! default). Nothing is sent about the session a phone is looking at, and
//! notices close together about the same thing go as one.
//!
//! The text never carries a command or a file path: the phone shows those
//! once it has opened the session over the encrypted channel.

use chrono::{Local, NaiveDate, Timelike};
use serde_json::{json, Value};
use std::collections::HashMap;

/// The notice levels, quietest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Log,
    Silent,
    Normal,
    Urgent,
}

impl Level {
    /// The Web Push `Urgency` header for this level (RFC 8030 5.3).
    pub fn urgency(self) -> &'static str {
        match self {
            Level::Log | Level::Silent => "low",
            Level::Normal => "normal",
            Level::Urgent => "high",
        }
    }
}

/// What a notice is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A session waits for an approval (approval_request, action_deferred).
    Approval,
    /// A session's turn ended on an error.
    Error,
    /// A turn ended while nobody was at the computer.
    TurnDone,
    /// Everything else: dispatches, requirements, agents' proposals.
    Update,
}

impl Kind {
    /// Exempt from the daily budget and from quiet hours.
    fn exempt(self) -> bool {
        matches!(self, Kind::Approval | Kind::Error)
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Approval => "approval",
            Kind::Error => "error",
            Kind::TurnDone => "done",
            Kind::Update => "update",
        }
    }
}

/// The user's notification settings (`settings.json` key `push`).
#[derive(Debug, Clone, PartialEq)]
pub struct Rules {
    pub daily_budget: u32,
    /// Minutes after midnight; equal start and end: no quiet hours.
    pub quiet_start: u32,
    pub quiet_end: u32,
    /// "一轮做完" notices (when nobody is at the computer).
    pub turn_done: bool,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            daily_budget: 20,
            quiet_start: 23 * 60,
            quiet_end: 8 * 60,
            turn_done: true,
        }
    }
}

fn hhmm(text: &str) -> Option<u32> {
    let (h, m) = text.split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

fn format_hhmm(minutes: u32) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

impl Rules {
    pub fn from_json(value: &Value) -> Self {
        let mut rules = Self::default();
        if let Some(n) = value["daily_budget"].as_u64() {
            rules.daily_budget = n.min(200) as u32;
        }
        if let Some(start) = value["quiet_start"].as_str().and_then(hhmm) {
            rules.quiet_start = start;
        }
        if let Some(end) = value["quiet_end"].as_str().and_then(hhmm) {
            rules.quiet_end = end;
        }
        if let Some(on) = value["turn_done"].as_bool() {
            rules.turn_done = on;
        }
        rules
    }

    pub fn to_json(&self) -> Value {
        json!({
            "daily_budget": self.daily_budget,
            "quiet_start": format_hhmm(self.quiet_start),
            "quiet_end": format_hhmm(self.quiet_end),
            "turn_done": self.turn_done,
        })
    }

    /// Whether `minute` (after local midnight) is in the quiet hours.
    pub fn quiet_at(&self, minute: u32) -> bool {
        let (start, end) = (self.quiet_start, self.quiet_end);
        if start == end {
            return false;
        }
        if start < end {
            (start..end).contains(&minute)
        } else {
            minute >= start || minute < end
        }
    }
}

/// One notice to send, after the rules.
#[derive(Debug, Clone, PartialEq)]
pub struct Outgoing {
    pub level: Level,
    pub kind: Kind,
    /// Groups the notices of one thing on the phone: a later one replaces it.
    pub tag: String,
    pub title: String,
    pub body: String,
    /// The remote page to open on a tap.
    pub url: String,
    /// The session it is about, when one.
    pub session: Option<String>,
    /// Notices merged into this one (1: just itself).
    pub count: u32,
}

impl Outgoing {
    /// What the service worker shows (sw `push` handler): no secrets.
    pub fn payload(&self) -> Value {
        let mut payload = json!({
            "title": self.title,
            "body": self.body,
            "tag": self.tag,
            "url": self.url,
            "kind": self.kind.name(),
            "level": match self.level {
                Level::Log => "log",
                Level::Silent => "silent",
                Level::Normal => "normal",
                Level::Urgent => "urgent",
            },
        });
        if let Some(session) = &self.session {
            payload["session"] = json!(session);
        }
        payload
    }
}

/// How long notices about one thing wait for more of the same before going
/// (approvals and errors go at once: they are what the user is waiting for).
pub const MERGE_WINDOW_MS: u64 = 20_000;
/// The same for approvals and errors: short, they are what the user waits for.
pub const EXEMPT_MERGE_MS: u64 = 5_000;

/// A notice waiting to merge with later ones.
#[derive(Debug)]
struct Waiting {
    notice: Outgoing,
    due: u64,
}

/// The policy's state: today's count and the notices waiting to merge.
#[derive(Debug, Default)]
pub struct Policy {
    pub rules: Rules,
    day: Option<NaiveDate>,
    sent_today: u32,
    waiting: Vec<Waiting>,
    /// When each tag last went out, so a burst of the same thing merges.
    /// By tag, and whether exempt: an approval does not wait for a
    /// counted notice of the same session, nor the other way round.
    last_sent: HashMap<(String, bool), u64>,
}

/// What the clock says, for the tests' sake.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    pub ms: u64,
    pub day: NaiveDate,
    /// Minutes after local midnight.
    pub minute: u32,
}

impl Clock {
    pub fn now() -> Self {
        let local = Local::now();
        Self {
            ms: crate::store::now(),
            day: local.date_naive(),
            minute: local.hour() * 60 + local.minute(),
        }
    }
}

impl Policy {
    pub fn new(rules: Rules) -> Self {
        Self {
            rules,
            ..Default::default()
        }
    }

    fn roll(&mut self, clock: &Clock) {
        if self.day != Some(clock.day) {
            self.day = Some(clock.day);
            self.sent_today = 0;
        }
    }

    pub fn sent_today(&mut self, clock: &Clock) -> u32 {
        self.roll(clock);
        self.sent_today
    }

    /// Takes a notice. Returns what is to go now: an exempt one at once, or
    /// one that merged nothing (the first of its tag for a while); a later
    /// one of the same tag waits (`due`) and merges with what follows.
    pub fn offer(&mut self, notice: Outgoing, clock: &Clock) -> Vec<Outgoing> {
        self.roll(clock);
        if notice.level == Level::Log {
            return Vec::new();
        }
        if notice.kind.exempt() {
            // An approval replaces anything counted waiting about the same
            // thing; several approvals in a row go as one.
            self.waiting
                .retain(|w| w.notice.tag != notice.tag || w.notice.kind.exempt());
            if let Some(waiting) = self.waiting.iter_mut().find(|w| w.notice.tag == notice.tag) {
                let count = waiting.notice.count + notice.count;
                waiting.notice = Outgoing { count, ..notice };
                return Vec::new();
            }
            let recent = self
                .last_sent
                .get(&(notice.tag.clone(), true))
                .is_some_and(|at| clock.ms.saturating_sub(*at) < EXEMPT_MERGE_MS);
            if recent {
                self.waiting.push(Waiting {
                    notice,
                    due: clock.ms + EXEMPT_MERGE_MS,
                });
                return Vec::new();
            }
            self.last_sent
                .insert((notice.tag.clone(), notice.kind.exempt()), clock.ms);
            return vec![notice];
        }
        if let Some(waiting) = self.waiting.iter_mut().find(|w| w.notice.tag == notice.tag) {
            let count = waiting.notice.count + notice.count;
            waiting.notice = Outgoing { count, ..notice };
            return Vec::new();
        }
        let recent = self
            .last_sent
            .get(&(notice.tag.clone(), false))
            .is_some_and(|at| clock.ms.saturating_sub(*at) < MERGE_WINDOW_MS);
        if recent {
            self.waiting.push(Waiting {
                notice,
                due: clock.ms + MERGE_WINDOW_MS,
            });
            return Vec::new();
        }
        self.admit(notice, clock).into_iter().collect()
    }

    /// The waiting notices whose time has come.
    pub fn due(&mut self, clock: &Clock) -> Vec<Outgoing> {
        self.roll(clock);
        let (ready, rest): (Vec<Waiting>, Vec<Waiting>) = std::mem::take(&mut self.waiting)
            .into_iter()
            .partition(|w| w.due <= clock.ms);
        self.waiting = rest;
        ready
            .into_iter()
            .filter_map(|w| self.admit(w.notice, clock))
            .collect()
    }

    /// Holds `notice` for `ms` before it goes (an approval while someone is
    /// at the computer: the desktop may answer it first); a later one of
    /// the same tag and kind merges into it.
    pub fn hold(&mut self, notice: Outgoing, clock: &Clock, ms: u64) {
        self.roll(clock);
        if let Some(waiting) = self
            .waiting
            .iter_mut()
            .find(|w| w.notice.tag == notice.tag && w.notice.kind == notice.kind)
        {
            let count = waiting.notice.count + notice.count;
            waiting.notice = Outgoing { count, ..notice };
            return;
        }
        self.waiting.push(Waiting {
            notice,
            due: clock.ms + ms,
        });
    }

    /// Drops what waits about `tag` (the user looked at it meanwhile).
    pub fn forget(&mut self, tag: &str) {
        self.waiting.retain(|w| w.notice.tag != tag);
    }

    /// Drops a waiting approval about `tag`: it was answered.
    pub fn answered(&mut self, tag: &str) {
        self.waiting
            .retain(|w| !(w.notice.tag == tag && w.notice.kind == Kind::Approval));
    }

    /// Whether anything waits (the scheduler checks often).
    pub fn has_waiting(&self) -> bool {
        !self.waiting.is_empty()
    }

    /// The budget and quiet hours for a counted notice.
    fn admit(&mut self, mut notice: Outgoing, clock: &Clock) -> Option<Outgoing> {
        if notice.count > 1 {
            notice.body = format!("{}（另有 {} 条）", notice.body, notice.count - 1);
        }
        if notice.kind.exempt() {
            self.last_sent
                .insert((notice.tag.clone(), notice.kind.exempt()), clock.ms);
            return Some(notice);
        }
        if self.sent_today >= self.rules.daily_budget {
            return None;
        }
        self.sent_today += 1;
        if self.rules.quiet_at(clock.minute) && notice.level > Level::Silent {
            notice.level = Level::Silent;
        }
        self.last_sent
            .insert((notice.tag.clone(), notice.kind.exempt()), clock.ms);
        Some(notice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock(ms: u64, minute: u32) -> Clock {
        Clock {
            ms,
            day: NaiveDate::from_ymd_opt(2026, 10, 19).unwrap(),
            minute,
        }
    }

    fn notice(kind: Kind, tag: &str) -> Outgoing {
        Outgoing {
            level: if kind == Kind::Approval {
                Level::Urgent
            } else {
                Level::Normal
            },
            kind,
            tag: tag.to_string(),
            title: "t".to_string(),
            body: "b".to_string(),
            url: "/remote".to_string(),
            session: Some(tag.to_string()),
            count: 1,
        }
    }

    #[test]
    fn quiet_hours_wrap_around_midnight() {
        let rules = Rules::default();
        for (minute, quiet) in [
            (22 * 60 + 59, false),
            (23 * 60, true),
            (3 * 60, true),
            (7 * 60 + 59, true),
            (8 * 60, false),
            (12 * 60, false),
        ] {
            assert_eq!(rules.quiet_at(minute), quiet, "{minute}");
        }
        let day = Rules {
            quiet_start: 13 * 60,
            quiet_end: 14 * 60,
            ..Rules::default()
        };
        assert!(day.quiet_at(13 * 60 + 30) && !day.quiet_at(14 * 60));
        let none = Rules {
            quiet_start: 0,
            quiet_end: 0,
            ..Rules::default()
        };
        assert!(!none.quiet_at(0) && !none.quiet_at(23 * 60));
    }

    #[test]
    fn rules_read_their_settings_and_keep_defaults_for_the_rest() {
        let rules = Rules::from_json(
            &json!({ "daily_budget": 5, "quiet_start": "22:30", "quiet_end": "bad" }),
        );
        assert_eq!(
            (
                rules.daily_budget,
                rules.quiet_start,
                rules.quiet_end,
                rules.turn_done
            ),
            (5, 22 * 60 + 30, 8 * 60, true)
        );
        assert_eq!(Rules::from_json(&rules.to_json()), rules);
        assert_eq!(Rules::from_json(&Value::Null), Rules::default());
    }

    #[test]
    fn approvals_and_errors_go_past_the_budget_and_quiet_hours() {
        let mut policy = Policy::new(Rules {
            daily_budget: 1,
            ..Rules::default()
        });
        let night = clock(1_000, 2 * 60);
        assert_eq!(policy.offer(notice(Kind::Update, "a"), &night).len(), 1);
        assert!(
            policy.offer(notice(Kind::Update, "b"), &night).is_empty(),
            "budget spent"
        );
        let approval = policy.offer(notice(Kind::Approval, "c"), &night);
        assert_eq!(
            approval[0].level,
            Level::Urgent,
            "an approval rings at night"
        );
        let error = policy.offer(notice(Kind::Error, "d"), &night);
        assert_eq!(error.len(), 1);
        assert_eq!(
            policy.sent_today(&night),
            1,
            "approvals and errors do not count"
        );
    }

    #[test]
    fn counted_notices_are_silent_in_quiet_hours() {
        let mut policy = Policy::default();
        let sent = policy.offer(notice(Kind::TurnDone, "a"), &clock(0, 23 * 60 + 30));
        assert_eq!(sent[0].level, Level::Silent);
        let sent = policy.offer(notice(Kind::TurnDone, "b"), &clock(0, 9 * 60));
        assert_eq!(sent[0].level, Level::Normal);
    }

    #[test]
    fn the_budget_starts_again_the_next_day() {
        let mut policy = Policy::new(Rules {
            daily_budget: 1,
            ..Rules::default()
        });
        let today = clock(0, 600);
        assert_eq!(policy.offer(notice(Kind::Update, "a"), &today).len(), 1);
        assert!(policy.offer(notice(Kind::Update, "b"), &today).is_empty());
        let tomorrow = Clock {
            day: today.day.succ_opt().unwrap(),
            ..today
        };
        assert_eq!(policy.offer(notice(Kind::Update, "c"), &tomorrow).len(), 1);
    }

    #[test]
    fn a_burst_about_one_thing_goes_as_one() {
        let mut policy = Policy::default();
        assert_eq!(
            policy
                .offer(notice(Kind::TurnDone, "s1"), &clock(0, 600))
                .len(),
            1
        );
        // Two more within the window wait and merge.
        assert!(policy
            .offer(notice(Kind::TurnDone, "s1"), &clock(5_000, 600))
            .is_empty());
        assert!(policy
            .offer(notice(Kind::TurnDone, "s1"), &clock(8_000, 600))
            .is_empty());
        assert!(policy.due(&clock(20_000, 600)).is_empty(), "not due yet");
        let merged = policy.due(&clock(25_000, 600));
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].body, "b（另有 1 条）");
        assert_eq!(policy.sent_today(&clock(25_000, 600)), 2);
        // Another thing is not held back.
        assert_eq!(
            policy
                .offer(notice(Kind::TurnDone, "s2"), &clock(26_000, 600))
                .len(),
            1
        );
    }

    #[test]
    fn an_approval_replaces_what_waits_about_the_same_session() {
        let mut policy = Policy::default();
        policy.offer(notice(Kind::TurnDone, "s1"), &clock(0, 600));
        policy.offer(notice(Kind::TurnDone, "s1"), &clock(1_000, 600));
        assert_eq!(
            policy
                .offer(notice(Kind::Approval, "s1"), &clock(2_000, 600))
                .len(),
            1
        );
        assert!(policy.due(&clock(60_000, 600)).is_empty());
    }

    #[test]
    fn approvals_in_a_row_go_as_one_soon_after() {
        let mut policy = Policy::default();
        assert_eq!(
            policy
                .offer(notice(Kind::Approval, "s1"), &clock(0, 600))
                .len(),
            1
        );
        assert!(policy
            .offer(notice(Kind::Approval, "s1"), &clock(1_000, 600))
            .is_empty());
        assert!(policy
            .offer(notice(Kind::Approval, "s1"), &clock(2_000, 600))
            .is_empty());
        let merged = policy.due(&clock(EXEMPT_MERGE_MS + 1_000, 600));
        assert_eq!(
            (merged.len(), merged[0].body.as_str()),
            (1, "b（另有 1 条）")
        );
        assert_eq!(policy.sent_today(&clock(9_000, 600)), 0);
    }

    #[test]
    fn forgetting_a_tag_drops_what_waits_for_it() {
        let mut policy = Policy::default();
        policy.offer(notice(Kind::Update, "s1"), &clock(0, 600));
        policy.offer(notice(Kind::Update, "s1"), &clock(1_000, 600));
        policy.forget("s1");
        assert!(policy.due(&clock(60_000, 600)).is_empty());
    }

    #[test]
    fn a_held_approval_goes_unless_answered_first() {
        let mut policy = Policy::default();
        policy.hold(notice(Kind::Approval, "s1"), &clock(0, 600), 45_000);
        assert!(policy.due(&clock(30_000, 600)).is_empty());
        assert_eq!(policy.due(&clock(45_000, 600)).len(), 1);
        policy.hold(notice(Kind::Approval, "s1"), &clock(50_000, 600), 45_000);
        policy.offer(notice(Kind::TurnDone, "s1"), &clock(50_000, 600));
        policy.offer(notice(Kind::TurnDone, "s1"), &clock(51_000, 600));
        policy.answered("s1");
        // The approval went; what else waits about the session stays.
        let due = policy.due(&clock(120_000, 600));
        assert_eq!(
            due.iter().map(|n| n.kind).collect::<Vec<_>>(),
            vec![Kind::TurnDone]
        );
    }

    #[test]
    fn log_level_sends_nothing() {
        let mut policy = Policy::default();
        let mut quiet = notice(Kind::Update, "a");
        quiet.level = Level::Log;
        assert!(policy.offer(quiet, &clock(0, 600)).is_empty());
        assert_eq!(policy.sent_today(&clock(0, 600)), 0);
    }

    #[test]
    fn the_payload_has_no_more_than_the_notice() {
        let payload = notice(Kind::Approval, "s1").payload();
        assert_eq!(
            payload,
            json!({ "title": "t", "body": "b", "tag": "s1", "url": "/remote", "kind": "approval", "level": "urgent", "session": "s1" })
        );
    }
}
