//! Notifications on the paired phones. A phone's browser gives the daemon
//! its Web Push subscription over the encrypted channel, the Aogiri app its
//! APNs or FCM device token; to notify it, the daemon asks the relay, which
//! signs the request for the push service (VAPID, APNs, FCM) and passes it
//! on without keeping it. A browser's message is encrypted here; APNs and
//! FCM get generic text and ids only (`Outgoing::native_payload`).

pub use crate::push_policy::{Answer, Kind, Level};
use crate::{
    hub::{lock, Hub},
    push_policy::{Clock, Outgoing, Policy, Rules},
    relay::Delivery,
    store::{write_private, Store},
};
use base64::Engine;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

const FILE: &str = "push.json";
/// The daemon setting with the rules (`push_policy::Rules`).
const SETTING: &str = "push";

pub struct Push {
    path: PathBuf,
    /// `{device, endpoint, keys: {p256dh, auth}}`, one per browser,
    /// `{device, provider: "apns", token, environment}` or
    /// `{device, provider: "fcm", token}`, one per Aogiri app, or
    /// `{device, provider: "getui", client_id}`.
    subscriptions: Mutex<Vec<Value>>,
    policy: Mutex<Policy>,
    /// The session each connected device has on screen now and when it
    /// said so (`push_viewing`): nothing about it is sent to that device.
    viewing: Mutex<HashMap<String, (String, u64)>>,
}

impl Push {
    pub fn load(dir: &Path, store: &Store) -> Self {
        let path = dir.join(FILE);
        let subscriptions = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        Self {
            path,
            subscriptions: Mutex::new(subscriptions),
            policy: Mutex::new(Policy::new(Rules::from_json(&store.setting(SETTING)))),
            viewing: Mutex::new(HashMap::new()),
        }
    }

    /// Keeps `subscription` (a browser's `PushSubscription.toJSON()`, the
    /// Aogiri app's APNs or FCM device token, or a Getui client id) for
    /// `device`, replacing an earlier one for the same endpoint or token.
    pub fn subscribe(&self, device: &str, subscription: &Value) -> Result<(), String> {
        let provider = subscription["provider"].as_str();
        let entry = if provider == Some("getui") {
            let client_id = subscription["client_id"].as_str().unwrap_or_default();
            if client_id.is_empty()
                || client_id.len() > 64
                || !client_id.chars().all(|c| c.is_ascii_alphanumeric())
            {
                return Err("push_subscribe requires a getui client_id".to_string());
            }
            json!({ "device": device, "provider": "getui", "client_id": client_id })
        } else if provider == Some("apns") {
            // iOS hands the token as bytes; the app sends them as hex.
            let token = subscription["token"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let environment = subscription["environment"].as_str().unwrap_or("production");
            if token.len() < 32
                || token.len() > 200
                || !token.len().is_multiple_of(2)
                || !token.chars().all(|c| c.is_ascii_hexdigit())
                || !matches!(environment, "production" | "sandbox")
            {
                return Err("push_subscribe requires an APNs device token (hex) and environment production or sandbox".to_string());
            }
            json!({ "device": device, "provider": "apns", "token": token, "environment": environment })
        } else if provider == Some("fcm") {
            let token = subscription["token"].as_str().unwrap_or_default();
            if token.len() < 16
                || token.len() > 512
                || !token
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
            {
                return Err("push_subscribe requires an FCM registration token".to_string());
            }
            json!({ "device": device, "provider": "fcm", "token": token })
        } else if let Some(other) = provider {
            return Err(format!("push_subscribe: unknown provider {other}"));
        } else {
            let endpoint = subscription["endpoint"].as_str().unwrap_or_default();
            let keys = &subscription["keys"];
            if !endpoint.starts_with("https://")
                || keys["p256dh"].as_str().is_none()
                || keys["auth"].as_str().is_none()
            {
                return Err("push_subscribe requires a push subscription".to_string());
            }
            json!({
                "device": device,
                "endpoint": endpoint,
                "keys": { "p256dh": keys["p256dh"], "auth": keys["auth"] },
            })
        };
        let id = key(&entry).to_string();
        let mut list = lock(&self.subscriptions);
        list.retain(|s| key(s) != id);
        list.push(entry);
        self.save(&list)
    }

    /// Drops the subscription with this endpoint, device token or Getui
    /// client id.
    pub fn unsubscribe(&self, id: &str) -> Result<(), String> {
        let mut list = lock(&self.subscriptions);
        // An APNs token is kept in lower case; the app may send it in upper.
        list.retain(|s| {
            key(s) != id && !(s["provider"] == "apns" && key(s).eq_ignore_ascii_case(id))
        });
        self.save(&list)
    }

    /// A revoked device's browsers get nothing more.
    pub fn forget_device(&self, device: &str) {
        let mut list = lock(&self.subscriptions);
        list.retain(|s| s["device"] != device);
        let _ = self.save(&list);
    }

    fn save(&self, list: &[Value]) -> Result<(), String> {
        write_private(&self.path, format!("{:#}\n", json!(list)).as_bytes())
            .map_err(|error| error.to_string())
    }
}

/// What identifies a subscription: its endpoint, device token or Getui
/// client id.
fn key(subscription: &Value) -> &str {
    subscription["endpoint"]
        .as_str()
        .or(subscription["token"].as_str())
        .or(subscription["client_id"].as_str())
        .unwrap_or_default()
}

/// The push service a subscription goes through, for `push_tested`:
/// `apns`, `apns-sandbox`, `fcm`, `getui` or a browser push service's host.
fn service(subscription: &Value) -> String {
    match subscription["provider"].as_str() {
        Some("apns") if subscription["environment"] == "sandbox" => "apns-sandbox".to_string(),
        Some(provider) => provider.to_string(),
        None => key(subscription)
            .split('/')
            .nth(2)
            .unwrap_or_default()
            .to_string(),
    }
}

/// `push_test`: a test notification to `device`'s browsers, sent now, with
/// each push service's answer (passed on by the relay): the phone can tell
/// whether a notification left, and through which service. Not counted.
pub fn test(hub: &Arc<Hub>, device: &str) -> Value {
    let list: Vec<Value> = lock(&hub.push.subscriptions)
        .iter()
        .filter(|s| s["device"] == device)
        .cloned()
        .collect();
    let payload = json!({
        "title": "Aogiri",
        "body": "测试通知：这台手机能收到电脑发来的通知。",
        "tag": "push-test",
        "url": "/remote",
        "kind": "test",
        "level": "normal",
    });
    // Generic already: the same for APNs and FCM.
    let native = payload.clone();
    let delivery = Delivery {
        urgency: "normal",
        ttl: 60,
        topic: None,
    };
    let results: Vec<Value> = list
        .iter()
        .map(|subscription| {
            let service = service(subscription);
            match hub.relay.push(subscription, &payload, &native, &delivery) {
                Ok(status) => json!({ "service": service, "status": status }),
                Err(error) => json!({ "service": service, "error": error }),
            }
        })
        .collect();
    json!({ "type": "push_tested", "results": results })
}

/// A notice for the paired phones, before the rules (`push_policy`).
pub struct Notice<'a> {
    pub kind: Kind,
    pub level: Level,
    /// Groups the notices of one thing: a later one replaces it on the phone.
    pub tag: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    /// The remote page to open on a tap; None: the session's page (with
    /// `session`), else the remote page.
    pub url: Option<&'a str>,
    /// The session it is about: not sent to a device that looks at it now.
    pub session: Option<&'a str>,
    /// The approval it is about, for the app's 允许/拒绝 buttons.
    pub answer: Option<Answer>,
}

/// Notifies the paired phones: the rules decide whether and how, and each
/// phone's browsers get it in the background unless the phone looks at the
/// session it is about.
pub fn notify(hub: &Arc<Hub>, notice: Notice) {
    let outgoing = outgoing(hub, notice);
    let ready = {
        let mut policy = lock(&hub.push.policy);
        policy.offer(outgoing, &Clock::now())
    };
    for outgoing in ready {
        deliver(hub, outgoing);
    }
}

fn outgoing(hub: &Hub, notice: Notice) -> Outgoing {
    // A session's page on this computer (a phone may pair several).
    let url = match (notice.url, notice.session) {
        (Some(url), _) => url.to_string(),
        (None, Some(session)) => match hub.relay.host_id() {
            Some(host) => format!("/remote?host={host}&session={session}"),
            None => format!("/remote?session={session}"),
        },
        (None, None) => "/remote".to_string(),
    };
    Outgoing {
        level: notice.level,
        kind: notice.kind,
        tag: notice.tag.to_string(),
        title: clip(notice.title, 80),
        body: clip(notice.body, 160),
        url,
        session: notice.session.map(str::to_string),
        answer: notice.answer,
        count: 1,
    }
}

/// Sends what waited to merge and is due now (from the scheduler tick).
pub fn tick(hub: &Arc<Hub>) {
    let ready = {
        let mut policy = lock(&hub.push.policy);
        if !policy.has_waiting() {
            return;
        }
        policy.due(&Clock::now())
    };
    for outgoing in ready {
        deliver(hub, outgoing);
    }
}

/// How long a phone's "I look at this session" lasts unless it says it
/// again: a page the phone froze (locked, in the background) cannot say it
/// stopped.
pub const VIEWING_TTL_MS: u64 = 90_000;

/// `push_viewing`: the session `device` shows on screen now, or none.
pub fn viewing(hub: &Hub, device: &str, session: Option<&str>) {
    let now = crate::store::now();
    match session.filter(|s| !s.is_empty()) {
        Some(session) => {
            lock(&hub.push.viewing).insert(device.to_string(), (session.to_string(), now));
            lock(&hub.push.policy).forget(session);
        }
        None => {
            lock(&hub.push.viewing).remove(device);
        }
    }
}

/// A device's last connection ended: it looks at nothing.
pub fn gone(hub: &Hub, device: &str) {
    lock(&hub.push.viewing).remove(device);
}

/// The devices that look at `session` now.
fn looking_at(hub: &Hub, session: &str) -> HashSet<String> {
    let now = crate::store::now();
    lock(&hub.push.viewing)
        .iter()
        .filter(|(_, (s, at))| s == session && now.saturating_sub(*at) < VIEWING_TTL_MS)
        .map(|(device, _)| device.clone())
        .collect()
}

/// Holds an approval notice for `ms` (someone is at the computer: the
/// desktop may answer it first).
pub fn hold(hub: &Arc<Hub>, notice: Notice, ms: u64) {
    let outgoing = outgoing(hub, notice);
    lock(&hub.push.policy).hold(outgoing, &Clock::now(), ms);
}

/// The approval waiting about `session` was answered: not sent.
pub fn answered(hub: &Hub, session: &str) {
    let mut policy = lock(&hub.push.policy);
    if policy.has_waiting() {
        policy.answered(session);
    }
}

/// How long an approval waits for the desktop while someone is at the
/// computer before the phones are told.
pub const DESKTOP_FIRST_MS: u64 = 45_000;

/// A session event: the phones hear about a request that waits, a turn
/// that ended on an error, and a turn that ended while nobody was at the
/// computer. Dispatch and requirement sessions tell their own story
/// (`dispatch`, `requirements`); an agent's failed turn becomes a report.
/// `failed`: the error that ended the turn just now; `ended`: a turn ended.
pub fn observe(hub: &Arc<Hub>, session: &str, event: &Value, ended: bool, failed: Option<&str>) {
    let kind = event["type"].as_str().unwrap_or_default();
    let waiting = matches!(kind, "approval_request" | "action_deferred");
    // A call that ran or was answered settles a held approval.
    if matches!(kind, "tool_output" | "action_decided") {
        answered(hub, session);
        return;
    }
    if !(waiting || ended || failed.is_some()) {
        return;
    }
    // Clients of a dispatch or requirement session get their own notices.
    if hub.dispatch.covers(session) || hub.requirements.covers(session) {
        return;
    }
    let Some(record) = hub.store.sessions().into_iter().find(|r| r.id == session) else {
        return;
    };
    let name = crate::hub::shown_title(&record, None).unwrap_or_else(|| "一个会话".to_string());
    let name = clip(&name, 40);
    if waiting {
        // An approval_request asks a person now; action_deferred was put on
        // the desk because nobody watched. A question from Claude Code
        // (questions) is asked the same way.
        let question = event["questions"].as_array().is_some_and(|q| !q.is_empty());
        // What the notification's 允许/拒绝 answer; a question is answered
        // in the app.
        let answer = match (kind, question) {
            (_, true) => None,
            ("approval_request", _) => event["call_id"]
                .as_str()
                .map(|id| Answer::Call(id.to_string())),
            _ => event["id"]
                .as_str()
                .map(|id| Answer::Action(id.to_string())),
        };
        let body = if question {
            format!("「{name}」在等你回答")
        } else {
            format!("「{name}」等你批准")
        };
        let notice = Notice {
            kind: Kind::Approval,
            level: Level::Urgent,
            tag: session,
            title: "等你处理",
            body: &body,
            url: None,
            session: Some(session),
            answer,
        };
        if kind == "approval_request" && crate::presence::at_computer() {
            hold(hub, notice, DESKTOP_FIRST_MS);
        } else {
            notify(hub, notice);
        }
        return;
    }
    if failed.is_some() {
        // An agent's failed turn is a report with its own notice.
        if record.agent.is_none() {
            let body = format!("「{name}」这一轮出错了，没有做完");
            notify(
                hub,
                Notice {
                    kind: Kind::Error,
                    level: Level::Urgent,
                    tag: session,
                    title: "会话出错",
                    body: &body,
                    url: None,
                    session: Some(session),
                    answer: None,
                },
            );
        }
        return;
    }
    // A turn ended. Only worth a notice when nobody is at the computer to
    // see it, and not for a session someone (the desktop, a phone) watches
    // right now: whoever watches it sees it end.
    let rules_on = lock(&hub.push.policy).rules.turn_done;
    if !rules_on || crate::presence::at_computer() || hub.watched(session) {
        return;
    }
    // A request of this turn still waits: its notice stands (a later one
    // with the same tag would replace it on the phone).
    if hub
        .store
        .open_actions()
        .iter()
        .any(|a| a.session_id == session)
    {
        return;
    }
    let body = format!("「{name}」这一轮做完了");
    notify(
        hub,
        Notice {
            kind: Kind::TurnDone,
            level: Level::Normal,
            tag: session,
            title: "本轮完成",
            body: &body,
            url: None,
            session: Some(session),
            answer: None,
        },
    );
}

fn deliver(hub: &Arc<Hub>, outgoing: Outgoing) {
    let looking = outgoing
        .session
        .as_deref()
        .map(|session| looking_at(hub, session))
        .unwrap_or_default();
    let payload = outgoing.payload();
    let native = outgoing.native_payload(hub.relay.host_id().as_deref());
    let delivery = Delivery {
        urgency: outgoing.level.urgency(),
        // An approval is stale after a while; the rest keep a day.
        ttl: if outgoing.kind == Kind::Approval {
            60 * 60
        } else {
            24 * 60 * 60
        },
        topic: topic(&outgoing.tag),
    };
    let mut list = lock(&hub.push.subscriptions).clone();
    // Relay v2: the account's phones and browsers registered with the
    // account (relay-protocol.md §7.6), told apart from the daemon's own by
    // `target`. A device that looks at the session is skipped by its
    // account authorization.
    let account = account_targets(hub);
    let looking_authz: HashSet<String> = {
        let accounts = hub.store.device_accounts();
        looking
            .iter()
            .filter_map(|device| accounts.get(device).map(|a| a.authz.clone()))
            .collect()
    };
    for target in account {
        if target["authz"]
            .as_str()
            .is_some_and(|authz| looking_authz.contains(authz))
        {
            continue;
        }
        let mut subscription = target["subscription"].clone();
        let duplicate = list.iter().any(|own| key(own) == key(&subscription));
        if duplicate || subscription.is_null() {
            continue;
        }
        subscription["target"] = target["id"].clone();
        list.push(subscription);
    }
    for subscription in list {
        if subscription["device"]
            .as_str()
            .is_some_and(|d| looking.contains(d))
        {
            continue;
        }
        let hub = Arc::clone(hub);
        let payload = payload.clone();
        let native = native.clone();
        let delivery = delivery.clone();
        // 410: the relay says the push service dropped the subscription.
        thread::spawn(
            move || match hub.relay.push(&subscription, &payload, &native, &delivery) {
                Ok(410) => match subscription["target"].as_str() {
                    Some(target) => {
                        let _ = lynshen_agent_core::account::forget_push_target(target);
                    }
                    None => {
                        let _ = hub.push.unsubscribe(key(&subscription));
                    }
                },
                Ok(status) if status >= 300 => {
                    lynshen_agent_core::log_warn!("daemon", "push refused", status = status);
                }
                Ok(_) => {}
                Err(error) => {
                    lynshen_agent_core::log_warn!("daemon", "push failed", error = error);
                }
            },
        );
    }
}

/// The account's push targets while the computer is on relay v2, read at
/// most once a minute (an error keeps the last list).
fn account_targets(hub: &Hub) -> Vec<Value> {
    static CACHE: Mutex<Option<(std::time::Instant, Vec<Value>)>> = Mutex::new(None);
    if !hub.relay.on_v2() {
        return Vec::new();
    }
    let mut cache = lock(&CACHE);
    if let Some((at, list)) = cache.as_ref() {
        if at.elapsed() < std::time::Duration::from_secs(60) {
            return list.clone();
        }
    }
    match lynshen_agent_core::account::push_targets() {
        Ok(list) => {
            *cache = Some((std::time::Instant::now(), list.clone()));
            list
        }
        Err(error) => {
            lynshen_agent_core::log_warn!(
                "daemon",
                "account push targets",
                error = error.to_string()
            );
            cache
                .as_ref()
                .map(|(_, list)| list.clone())
                .unwrap_or_default()
        }
    }
}

/// A push service topic for `tag` (RFC 8030 5.4: up to 32 base64url
/// characters): one notice not yet delivered gives way to a newer one.
fn topic(tag: &str) -> Option<String> {
    use sha2::{Digest, Sha256};
    if tag.is_empty() {
        return None;
    }
    let digest = Sha256::digest(tag.as_bytes());
    Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&digest[..18]))
}

/// The rules as the phone shows them, with today's count.
pub fn rules_json(hub: &Hub) -> Value {
    let mut policy = lock(&hub.push.policy);
    let mut rules = policy.rules.to_json();
    rules["sent_today"] = json!(policy.sent_today(&Clock::now()));
    json!({ "type": "push_rules", "rules": rules })
}

/// `push_rules_set`: changes the rules (kept in settings.json `push`).
pub fn set_rules(hub: &Hub, value: &Value) -> Result<Value, String> {
    let mut merged = lock(&hub.push.policy).rules.to_json();
    if let (Some(into), Some(from)) = (merged.as_object_mut(), value.as_object()) {
        for (k, v) in from {
            if matches!(
                k.as_str(),
                "daily_budget" | "quiet_start" | "quiet_end" | "turn_done"
            ) {
                into.insert(k.clone(), v.clone());
            }
        }
    }
    let rules = Rules::from_json(&merged);
    hub.store
        .set_setting(SETTING, rules.to_json())
        .map_err(|error| error.to_string())?;
    lock(&hub.push.policy).rules = rules;
    Ok(rules_json(hub))
}

fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_web_push_and_getui_subscriptions_per_device() {
        let dir = std::env::temp_dir().join(format!("lynshen-push-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let store = Store::open(dir.join("state")).unwrap();
        let push = Push::load(&dir, &store);
        let web = json!({ "endpoint": "https://web.push.apple.com/a", "keys": { "p256dh": "p", "auth": "a" } });
        let phone = json!({ "provider": "getui", "client_id": "abc123" });
        push.subscribe("d1", &web).unwrap();
        push.subscribe("d1", &phone).unwrap();
        push.subscribe("d2", &phone).unwrap(); // the phone signed in again
        for bad in [
            json!({ "provider": "getui" }),
            json!({ "provider": "getui", "client_id": "a/b" }),
            json!({ "endpoint": "http://x" }),
        ] {
            assert!(push.subscribe("d1", &bad).is_err());
        }
        let list = Push::load(&dir, &store).subscriptions.into_inner().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(
            list[1],
            json!({ "device": "d2", "provider": "getui", "client_id": "abc123" })
        );
        assert_eq!(service(&list[0]), "web.push.apple.com");
        assert_eq!(service(&list[1]), "getui");

        // The Aogiri app: an APNs token (hex, as sent: case does not
        // matter) with its environment, or an FCM token.
        let token = "A1B2C3D4E5F60718293A4B5C6D7E8F90A1B2C3D4E5F60718293A4B5C6D7E8F90";
        let iphone = json!({ "provider": "apns", "token": token, "environment": "sandbox" });
        let android =
            json!({ "provider": "fcm", "token": "dXNlcjEyMzQ1Njc4OTA:APA91bH-test_token" });
        push.subscribe("d3", &iphone).unwrap();
        push.subscribe("d3", &android).unwrap();
        push.subscribe("d3", &json!({ "provider": "apns", "token": token }))
            .unwrap();
        for bad in [
            json!({ "provider": "apns", "token": "zz".repeat(32) }),
            json!({ "provider": "apns", "token": format!("../{token}") }),
            json!({ "provider": "apns", "token": token, "environment": "staging" }),
            json!({ "provider": "fcm", "token": "short" }),
            json!({ "provider": "fcm", "token": "a token with spaces in it" }),
            json!({ "provider": "hms", "token": "abc" }),
        ] {
            assert!(push.subscribe("d3", &bad).is_err(), "{bad}");
        }
        let natives: Vec<Value> = lock(&push.subscriptions)
            .iter()
            .filter(|s| s["device"] == "d3")
            .cloned()
            .collect();
        assert_eq!(
            natives,
            vec![
                json!({ "device": "d3", "provider": "fcm", "token": "dXNlcjEyMzQ1Njc4OTA:APA91bH-test_token" }),
                json!({ "device": "d3", "provider": "apns", "token": token.to_ascii_lowercase(), "environment": "production" }),
            ]
        );
        assert_eq!(service(&natives[0]), "fcm");
        assert_eq!(service(&natives[1]), "apns");
        assert_eq!(service(&iphone), "apns-sandbox");
        push.unsubscribe(token).unwrap();
        assert_eq!(
            lock(&push.subscriptions)
                .iter()
                .filter(|s| s["device"] == "d3")
                .count(),
            1
        );
        push.forget_device("d3");

        push.unsubscribe("abc123").unwrap();
        assert_eq!(lock(&push.subscriptions).len(), 1);
        push.subscribe("d2", &phone).unwrap();
        push.forget_device("d1");
        assert_eq!(*lock(&push.subscriptions), vec![list[1].clone()]);
        let _ = fs::remove_dir_all(&dir);
    }
}
