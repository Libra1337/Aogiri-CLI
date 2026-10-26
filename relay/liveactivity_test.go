package main

import (
	"encoding/json"
	"net/http"
	"strconv"
	"strings"
	"testing"
	"time"
)

// A push-to-start token as current iOS hands it: 128 bytes.
var testActivityToken = strings.Repeat("0a1b2c3d", 32)

func waitingState() map[string]any {
	return map[string]any{
		"phase": "waiting", "title": "电脑想运行命令", "detail": "", "session": "s-1",
		"sessionTitle": "修登录页", "tasks": 1, "waiting": 1, "done": 0, "total": 0,
		"since": 1_800_000_000.5, "ember": "attention",
	}
}

func runningState() map[string]any {
	state := waitingState()
	state["phase"], state["title"], state["detail"], state["waiting"], state["ember"] = "running", "修登录页", "运行命令", 0, "thinking"
	state["done"], state["total"] = 3, 7
	return state
}

func activitySub() map[string]any {
	return map[string]any{"provider": "apns", "token": strings.ToUpper(testActivityToken), "environment": "sandbox"}
}

func TestALiveActivityStartGoesToAPNsWithItsHeadersAndBody(t *testing.T) {
	f, rec, _, _ := apnsFixture(t)
	now := time.Now().Unix()
	status, text := f.post(map[string]any{
		"subscription": activitySub(),
		"liveactivity": map[string]any{
			"event": "start", "content_state": waitingState(), "relevance_score": 100,
			"stale_date": now + 600, "timestamp": now - 3,
			"attributes_type": "ComputerActivityAttributes",
			"attributes":      map[string]any{"host": "h0st-1d", "hostName": "Chad 的 MacBook"},
			"alert":           map[string]any{"title": "电脑在等你批准", "body": "「修登录页」想运行命令"},
		},
		"ttl": 300,
	}, nil)
	if status != http.StatusNoContent {
		t.Fatalf("%d %s", status, text)
	}
	got := rec.last(t)
	h := got.r.Header
	if got.r.URL.Path != "/sandbox/3/device/"+testActivityToken ||
		h.Get("apns-topic") != "com.lynshen.aogiri.push-type.liveactivity" ||
		h.Get("apns-push-type") != "liveactivity" || h.Get("apns-priority") != "10" ||
		h.Get("apns-collapse-id") != "" || !strings.HasPrefix(h.Get("Authorization"), "bearer ") {
		t.Fatalf("request %s %v", got.r.URL, h)
	}
	if exp, _ := strconv.ParseInt(h.Get("apns-expiration"), 10, 64); exp < now+290 || exp > now+310 {
		t.Fatalf("expiration %q", h.Get("apns-expiration"))
	}
	var body map[string]map[string]any
	if err := json.Unmarshal(got.body, &body); err != nil || len(body) != 1 {
		t.Fatalf("body %s", got.body)
	}
	aps := body["aps"]
	want := []string{"timestamp", "event", "content-state", "stale-date", "relevance-score", "attributes-type", "attributes", "alert"}
	if len(aps) != len(want) {
		t.Fatalf("aps %v", aps)
	}
	for _, key := range want {
		if _, ok := aps[key]; !ok {
			t.Fatalf("aps lacks %s: %v", key, aps)
		}
	}
	if aps["event"] != "start" || aps["timestamp"] != float64(now-3) || aps["stale-date"] != float64(now+600) ||
		aps["relevance-score"] != float64(100) || aps["attributes-type"] != "ComputerActivityAttributes" {
		t.Fatalf("aps %v", aps)
	}
	state := aps["content-state"].(map[string]any)
	if len(state) != 11 || state["phase"] != "waiting" || state["sessionTitle"] != "修登录页" || state["since"] != 1_800_000_000.5 || state["detail"] != "" {
		t.Fatalf("content-state %v", state)
	}
	attributes := aps["attributes"].(map[string]any)
	if attributes["host"] != "h0st-1d" || attributes["hostName"] != "Chad 的 MacBook" {
		t.Fatalf("attributes %v", attributes)
	}
	alert := aps["alert"].(map[string]any)
	if alert["title"] != "电脑在等你批准" || alert["body"] != "「修登录页」想运行命令" {
		t.Fatalf("alert %v", alert)
	}
}

func TestLiveActivityPriorityTimestampAndEnd(t *testing.T) {
	f, rec, _, _ := apnsFixture(t)
	send := func(la map[string]any) (int, recorded) {
		t.Helper()
		status, text := f.post(map[string]any{"subscription": activitySub(), "liveactivity": la}, nil)
		if status != http.StatusNoContent {
			t.Fatalf("%v: %d %s", la, status, text)
		}
		return status, rec.last(t)
	}
	before := time.Now().Unix()
	// A plain step update: priority 5, timestamp set by the relay, short expiry.
	_, got := send(map[string]any{"event": "update", "content_state": runningState(), "relevance_score": 80})
	if got.r.Header.Get("apns-priority") != "5" {
		t.Fatalf("step update priority %q", got.r.Header.Get("apns-priority"))
	}
	if exp, _ := strconv.ParseInt(got.r.Header.Get("apns-expiration"), 10, 64); exp < before+liveActivityDefaultTTL-5 || exp > before+liveActivityDefaultTTL+5 {
		t.Fatalf("expiration %q", got.r.Header.Get("apns-expiration"))
	}
	var body struct{ Aps map[string]any }
	json.Unmarshal(got.body, &body)
	if ts, _ := body.Aps["timestamp"].(float64); int64(ts) < before || int64(ts) > time.Now().Unix() {
		t.Fatalf("timestamp %v", body.Aps["timestamp"])
	}
	if _, has := body.Aps["attributes"]; has {
		t.Fatalf("update with attributes %v", body.Aps)
	}
	// An update into waiting: priority 10.
	if _, got = send(map[string]any{"event": "update", "content_state": waitingState(), "alert": map[string]any{"title": "电脑在等你批准"}}); got.r.Header.Get("apns-priority") != "10" {
		t.Fatalf("waiting priority %q", got.r.Header.Get("apns-priority"))
	}
	// done and failed go at priority 10 too.
	for _, phase := range []string{"done", "failed"} {
		state := runningState()
		state["phase"] = phase
		if _, got = send(map[string]any{"event": "update", "content_state": state}); got.r.Header.Get("apns-priority") != "10" {
			t.Fatalf("%s priority %q", phase, got.r.Header.Get("apns-priority"))
		}
	}
	// end with a dismissal date: priority 10.
	state := runningState()
	state["phase"], state["ember"] = "done", "done"
	dismiss := time.Now().Unix() + 15*60
	_, got = send(map[string]any{"event": "end", "content_state": state, "dismissal_date": dismiss})
	json.Unmarshal(got.body, &body)
	if got.r.Header.Get("apns-priority") != "10" || body.Aps["event"] != "end" || body.Aps["dismissal-date"] != float64(dismiss) {
		t.Fatalf("end %v %v", got.r.Header, body.Aps)
	}
}

func TestLiveActivityRefusals(t *testing.T) {
	f, rec, _, _ := apnsFixture(t)
	now := time.Now().Unix()
	with := func(change func(la map[string]any)) map[string]any {
		la := map[string]any{"event": "update", "content_state": runningState()}
		change(la)
		return map[string]any{"subscription": activitySub(), "liveactivity": la}
	}
	state := func(change func(s map[string]any)) func(la map[string]any) {
		return func(la map[string]any) {
			s := runningState()
			change(s)
			la["content_state"] = s
		}
	}
	start := func(la map[string]any) {
		la["event"] = "start"
		la["content_state"] = waitingState()
		la["attributes_type"] = "ComputerActivityAttributes"
		la["attributes"] = map[string]any{"host": "h0st-1d", "hostName": "Mac"}
	}
	cases := []struct {
		name    string
		request map[string]any
	}{
		{"unknown event", with(func(la map[string]any) { la["event"] = "pause" })},
		{"no content state", with(func(la map[string]any) { delete(la, "content_state") })},
		{"content state not an object", with(func(la map[string]any) { la["content_state"] = []any{1} })},
		{"content state lacks a field", with(state(func(s map[string]any) { delete(s, "ember") }))},
		{"content state extra field", with(state(func(s map[string]any) { s["command"] = "git push" }))},
		{"bad phase", with(state(func(s map[string]any) { s["phase"] = "paused" }))},
		{"bad ember", with(state(func(s map[string]any) { s["ember"] = "fire" }))},
		{"tasks not an int", with(state(func(s map[string]any) { s["tasks"] = 1.5 }))},
		{"negative count", with(state(func(s map[string]any) { s["done"] = -1 }))},
		{"since a string", with(state(func(s map[string]any) { s["since"] = "now" }))},
		{"title a number", with(state(func(s map[string]any) { s["title"] = 3 }))},
		{"long detail", with(state(func(s map[string]any) { s["detail"] = strings.Repeat("长", maxContentText+1) }))},
		{"session not an id", with(state(func(s map[string]any) { s["session"] = "s 1/../x" }))},
		{"relevance over 100", with(func(la map[string]any) { la["relevance_score"] = 101 })},
		{"stale date far away", with(func(la map[string]any) { la["stale_date"] = now + 48*3600 })},
		{"timestamp in the future", with(func(la map[string]any) { la["timestamp"] = now + 3600 })},
		{"timestamp in ms", with(func(la map[string]any) { la["timestamp"] = now * 1000 })},
		{"dismissal on update", with(func(la map[string]any) { la["dismissal_date"] = now + 60 })},
		{"attributes on update", with(func(la map[string]any) { la["attributes"] = map[string]any{"host": "h"} })},
		{"alert on running", with(func(la map[string]any) { la["alert"] = map[string]any{"title": "x"} })},
		{"unknown field", with(func(la map[string]any) { la["sound"] = "default" })},
		{"start without attributes", with(func(la map[string]any) { start(la); delete(la, "attributes") })},
		{"start of another type", with(func(la map[string]any) { start(la); la["attributes_type"] = "OtherAttributes" })},
		{"start attributes extra", with(func(la map[string]any) {
			start(la)
			la["attributes"] = map[string]any{"host": "h0st-1d", "hostName": "Mac", "path": "/Users/x"}
		})},
		{"alert on end", with(func(la map[string]any) {
			la["event"] = "end"
			la["content_state"] = waitingState()
			la["alert"] = map[string]any{"title": "x"}
		})},
		{"token not hex", map[string]any{"subscription": map[string]any{"provider": "apns", "token": strings.Repeat("zz", 64)}, "liveactivity": map[string]any{"event": "update", "content_state": runningState()}}},
		{"token too long", map[string]any{"subscription": map[string]any{"provider": "apns", "token": strings.Repeat("ab", 257)}, "liveactivity": map[string]any{"event": "update", "content_state": runningState()}}},
		{"with a notice payload", map[string]any{"subscription": activitySub(), "payload": approvalNotice(), "liveactivity": map[string]any{"event": "update", "content_state": runningState()}}},
		{"through fcm", map[string]any{"subscription": map[string]any{"provider": "fcm", "token": strings.Repeat("a", 40)}, "liveactivity": map[string]any{"event": "update", "content_state": runningState()}}},
		{"through getui", map[string]any{"subscription": map[string]any{"provider": "getui", "client_id": "abc"}, "liveactivity": map[string]any{"event": "update", "content_state": runningState()}}},
		{"through web push", map[string]any{"subscription": map[string]any{"endpoint": "https://web.push.apple.com/x"}, "liveactivity": map[string]any{"event": "update", "content_state": runningState()}}},
	}
	for _, c := range cases {
		if status, text := f.post(c.request, nil); status != http.StatusBadRequest {
			t.Errorf("%s: %d %s, want 400", c.name, status, text)
		}
	}
	rec.mu.Lock()
	defer rec.mu.Unlock()
	if len(rec.got) != 0 {
		t.Fatalf("%d refused requests reached APNs", len(rec.got))
	}
}

func TestLiveActivityTooLargeForAPNs(t *testing.T) {
	f, rec, _, _ := apnsFixture(t)
	// Each field within its limit, but escaped (& is \u0026) the whole
	// is over what APNs takes.
	s := runningState()
	for _, name := range []string{"title", "detail", "sessionTitle"} {
		s[name] = strings.Repeat("&", maxContentText)
	}
	alert := map[string]any{"title": strings.Repeat("&", maxNoticeTitle), "body": strings.Repeat("&", maxNoticeBody)}
	s["phase"] = "waiting"
	status, text := f.post(map[string]any{"subscription": activitySub(), "liveactivity": map[string]any{
		"event": "start", "content_state": s, "alert": alert, "attributes_type": "ComputerActivityAttributes",
		"attributes": map[string]any{"host": "h0st-1d", "hostName": strings.Repeat("长", 64)},
	}}, nil)
	if status != http.StatusRequestEntityTooLarge {
		t.Fatalf("%d %s", status, text)
	}
	rec.mu.Lock()
	defer rec.mu.Unlock()
	if len(rec.got) != 0 {
		t.Fatal("a payload over 4096 bytes reached APNs")
	}
}

func TestLiveActivityAnswersArePassedOn(t *testing.T) {
	f, rec, _, _ := apnsFixture(t)
	for _, c := range []struct {
		status int
		reason string
		want   int
	}{
		{200, "", 204},
		{410, "Unregistered", 410},
		{400, "BadDeviceToken", 410},
		{400, "TopicDisallowed", 502},
	} {
		rec.set(c.status, `{"reason":"`+c.reason+`"}`)
		status, text := f.post(map[string]any{"subscription": activitySub(), "liveactivity": map[string]any{"event": "update", "content_state": runningState()}}, nil)
		if status != c.want {
			t.Errorf("APNs %d %s: relay %d %s, want %d", c.status, c.reason, status, text, c.want)
		}
	}
}

func TestLiveActivityWithoutAPNsIsNotConfigured(t *testing.T) {
	f, _, _, _, _ := fcmFixture(t)
	status, _ := f.post(map[string]any{"subscription": activitySub(), "liveactivity": map[string]any{"event": "update", "content_state": runningState()}}, nil)
	if status != http.StatusNotImplemented {
		t.Fatalf("%d", status)
	}
}
