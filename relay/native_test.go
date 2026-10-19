package main

import (
	"crypto"
	"crypto/ecdsa"
	"crypto/ed25519"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/rsa"
	"crypto/sha256"
	"crypto/x509"
	"encoding/json"
	"encoding/pem"
	"io"
	"math/big"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"
)

const testAPNsToken = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90"

type recorded struct {
	r    *http.Request
	body []byte
}

// recorder is a fake provider that answers `status` and `answer`.
type recorder struct {
	mu     sync.Mutex
	got    []recorded
	status int
	answer string
}

func (rec *recorder) handler(w http.ResponseWriter, r *http.Request) {
	body, _ := io.ReadAll(r.Body)
	rec.mu.Lock()
	rec.got = append(rec.got, recorded{r, body})
	status, answer := rec.status, rec.answer
	rec.mu.Unlock()
	w.WriteHeader(status)
	io.WriteString(w, answer)
}

func (rec *recorder) set(status int, answer string) {
	rec.mu.Lock()
	rec.status, rec.answer = status, answer
	rec.mu.Unlock()
}

func (rec *recorder) last(t *testing.T) recorded {
	t.Helper()
	rec.mu.Lock()
	defer rec.mu.Unlock()
	if len(rec.got) == 0 {
		t.Fatal("nothing reached the provider")
	}
	return rec.got[len(rec.got)-1]
}

func writeP8(t *testing.T) (string, *ecdsa.PrivateKey) {
	t.Helper()
	key, _ := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	der, _ := x509.MarshalPKCS8PrivateKey(key)
	path := filepath.Join(t.TempDir(), "AuthKey_ABC123DEFG.p8")
	os.WriteFile(path, pem.EncodeToMemory(&pem.Block{Type: "PRIVATE KEY", Bytes: der}), 0o600)
	return path, key
}

// apnsFixture is a relay with APNs only, posting to a local HTTP/2 server.
func apnsFixture(t *testing.T) (*pushFixture, *recorder, *apnsSender, *ecdsa.PrivateKey) {
	t.Helper()
	rec := &recorder{status: http.StatusOK}
	service := httptest.NewUnstartedServer(http.HandlerFunc(rec.handler))
	service.EnableHTTP2 = true
	service.StartTLS()
	t.Cleanup(service.Close)
	path, key := writeP8(t)
	a, err := loadAPNs(path, apnsConfig{keyID: "ABC123DEFG", teamID: "TEAM123456", topic: "com.lynshen.aogiri"})
	if err != nil {
		t.Fatal(err)
	}
	a.client = service.Client()
	a.origins = map[string]string{"production": service.URL, "sandbox": service.URL + "/sandbox"}
	f := nativeFixture(t, natives{"apns": a})
	return f, rec, a, key
}

func nativeFixture(t *testing.T, n natives) *pushFixture {
	t.Helper()
	f := &pushFixture{t: t}
	f.relay = newRelay()
	f.push = newPusher(f.relay, nil, n)
	f.server = httptest.NewServer(routes(f.relay, f.push))
	t.Cleanup(f.server.Close)
	f.hostPub, f.hostKey, _ = ed25519.GenerateKey(rand.Reader)
	f.connectHost()
	return f
}

func approvalNotice() map[string]any {
	return map[string]any{
		"title": "等你处理", "body": "「修登录页」等你批准", "kind": "approval", "level": "urgent",
		"tag": "s-1", "session": "s-1", "host": "h0st-1d", "call_id": "call_9",
		"url": "/remote?host=h0st-1d&session=s-1",
	}
}

func verifyES256(t *testing.T, token string, public *ecdsa.PublicKey) (map[string]any, map[string]any) {
	t.Helper()
	parts := strings.Split(token, ".")
	if len(parts) != 3 {
		t.Fatalf("token %q", token)
	}
	var head, claims map[string]any
	json.Unmarshal(unb64(t, parts[0]), &head)
	json.Unmarshal(unb64(t, parts[1]), &claims)
	signature := unb64(t, parts[2])
	digest := sha256.Sum256([]byte(parts[0] + "." + parts[1]))
	if len(signature) != 64 || !ecdsa.Verify(public, digest[:], new(big.Int).SetBytes(signature[:32]), new(big.Int).SetBytes(signature[32:])) {
		t.Fatal("the provider token does not verify")
	}
	return head, claims
}

func TestAnApprovalGoesToAPNsWithItsCategoryAndIDsOnly(t *testing.T) {
	f, rec, _, key := apnsFixture(t)
	status, text := f.post(map[string]any{
		"subscription": map[string]any{"provider": "apns", "token": strings.ToUpper(testAPNsToken), "environment": "production"},
		"payload":      approvalNotice(), "urgency": "high", "ttl": 3600, "topic": "c2Vzc2lvbi0x",
	}, nil)
	if status != http.StatusNoContent {
		t.Fatalf("%d %s", status, text)
	}
	got := rec.last(t)
	if got.r.ProtoMajor != 2 {
		t.Fatalf("APNs needs HTTP/2, got %s", got.r.Proto)
	}
	h := got.r.Header
	if got.r.URL.Path != "/3/device/"+testAPNsToken || h.Get("apns-topic") != "com.lynshen.aogiri" ||
		h.Get("apns-push-type") != "alert" || h.Get("apns-priority") != "10" || h.Get("apns-collapse-id") != "c2Vzc2lvbi0x" {
		t.Fatalf("request %s %v", got.r.URL, h)
	}
	if exp, err := strconv.ParseInt(h.Get("apns-expiration"), 10, 64); err != nil || exp < time.Now().Add(59*time.Minute).Unix() || exp > time.Now().Add(61*time.Minute).Unix() {
		t.Fatalf("expiration %q", h.Get("apns-expiration"))
	}
	bearer, ok := strings.CutPrefix(h.Get("Authorization"), "bearer ")
	if !ok {
		t.Fatalf("authorization %q", h.Get("Authorization"))
	}
	head, claims := verifyES256(t, bearer, &key.PublicKey)
	if head["alg"] != "ES256" || head["kid"] != "ABC123DEFG" || claims["iss"] != "TEAM123456" || claims["iat"] == nil {
		t.Fatalf("token %v %v", head, claims)
	}
	var payload struct {
		Aps  map[string]any `json:"aps"`
		Body map[string]any `json:"body"`
	}
	json.Unmarshal(got.body, &payload)
	alert, _ := payload.Aps["alert"].(map[string]any)
	if alert["title"] != "等你处理" || alert["body"] != "「修登录页」等你批准" || payload.Aps["category"] != "approval" ||
		payload.Aps["sound"] != "default" || payload.Aps["interruption-level"] != "time-sensitive" || payload.Aps["thread-id"] != "s-1" {
		t.Fatalf("aps %v", payload.Aps)
	}
	want := map[string]any{"kind": "approval", "tag": "s-1", "session": "s-1", "host": "h0st-1d", "call_id": "call_9", "url": "/remote?host=h0st-1d&session=s-1"}
	if len(payload.Body) != len(want) {
		t.Fatalf("data %v", payload.Body)
	}
	for k, v := range want {
		if payload.Body[k] != v {
			t.Fatalf("data %v", payload.Body)
		}
	}
}

func TestAPNsSandboxQuietNoticesAndTokenReuse(t *testing.T) {
	f, rec, _, _ := apnsFixture(t)
	notice := map[string]any{"title": "本轮完成", "body": "「x」这一轮做完了", "kind": "done", "level": "silent", "tag": "s-2"}
	sub := map[string]any{"provider": "apns", "token": testAPNsToken, "environment": "sandbox"}
	for i := 0; i < 2; i++ {
		if status, text := f.post(map[string]any{"subscription": sub, "payload": notice}, nil); status != http.StatusNoContent {
			t.Fatalf("%d %s", status, text)
		}
	}
	rec.mu.Lock()
	first, second := rec.got[0], rec.got[1]
	rec.mu.Unlock()
	if !strings.HasPrefix(first.r.URL.Path, "/sandbox/3/device/") || first.r.Header.Get("apns-priority") != "5" {
		t.Fatalf("%s %v", first.r.URL, first.r.Header)
	}
	if first.r.Header.Get("Authorization") != second.r.Header.Get("Authorization") {
		t.Fatal("the provider token is not reused")
	}
	var payload struct{ Aps map[string]any }
	json.Unmarshal(first.body, &payload)
	if payload.Aps["sound"] != nil || payload.Aps["interruption-level"] != "passive" || payload.Aps["category"] != nil {
		t.Fatalf("aps %v", payload.Aps)
	}
}

func TestAPNsAnswersArePassedOn(t *testing.T) {
	f, rec, a, _ := apnsFixture(t)
	sub := map[string]any{"provider": "apns", "token": testAPNsToken}
	for _, c := range []struct {
		status int
		reason string
		want   int
	}{
		{200, "", 204},
		{410, "Unregistered", 410},
		{400, "BadDeviceToken", 410},
		{400, "DeviceTokenNotForTopic", 410},
		{400, "BadCollapseId", 502},
		{429, "TooManyRequests", 429},
		{413, "PayloadTooLarge", 413},
		{403, "ExpiredProviderToken", 502},
	} {
		rec.set(c.status, `{"reason":"`+c.reason+`"}`)
		if status, text := f.post(map[string]any{"subscription": sub, "payload": approvalNotice()}, nil); status != c.want {
			t.Errorf("APNs %d %s: relay %d %s, want %d", c.status, c.reason, status, text, c.want)
		}
	}
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.token != "" {
		t.Fatal("a token Apple called expired is kept")
	}
}

func TestNativeRefusals(t *testing.T) {
	f, rec, _, _ := apnsFixture(t)
	good := map[string]any{"provider": "apns", "token": testAPNsToken}
	cases := []struct {
		name    string
		request map[string]any
		want    int
	}{
		{"fcm not configured", map[string]any{"subscription": map[string]any{"provider": "fcm", "token": strings.Repeat("a", 40)}, "payload": approvalNotice()}, 501},
		{"getui not configured", map[string]any{"subscription": map[string]any{"provider": "getui", "client_id": "abc"}, "payload": approvalNotice()}, 501},
		{"unknown provider", map[string]any{"subscription": map[string]any{"provider": "hms", "token": "x"}, "payload": approvalNotice()}, 400},
		{"token not hex", map[string]any{"subscription": map[string]any{"provider": "apns", "token": strings.Repeat("z", 64)}, "payload": approvalNotice()}, 400},
		{"token with a path", map[string]any{"subscription": map[string]any{"provider": "apns", "token": "../" + testAPNsToken}, "payload": approvalNotice()}, 400},
		{"bad environment", map[string]any{"subscription": map[string]any{"provider": "apns", "token": testAPNsToken, "environment": "staging"}, "payload": approvalNotice()}, 400},
		{"encrypted", map[string]any{"subscription": good, "encrypted": b64.EncodeToString(make([]byte, 120))}, 400},
		{"no payload", map[string]any{"subscription": good}, 400},
		{"no title", map[string]any{"subscription": good, "payload": map[string]any{"kind": "done"}}, 400},
		{"long body", map[string]any{"subscription": good, "payload": map[string]any{"title": "x", "kind": "done", "body": strings.Repeat("长", maxNoticeBody+1)}}, 400},
		{"bad kind", map[string]any{"subscription": good, "payload": map[string]any{"title": "x", "kind": "command"}}, 400},
		{"bad id", map[string]any{"subscription": good, "payload": map[string]any{"title": "x", "kind": "done", "session": "s 1"}}, 400},
		{"bad url", map[string]any{"subscription": good, "payload": map[string]any{"title": "x", "kind": "done", "url": "https://evil.example/"}}, 400},
	}
	for _, c := range cases {
		if status, text := f.post(c.request, nil); status != c.want {
			t.Errorf("%s: %d %s, want %d", c.name, status, text, c.want)
		}
	}
	rec.mu.Lock()
	defer rec.mu.Unlock()
	if len(rec.got) != 0 {
		t.Fatalf("%d refused requests reached APNs", len(rec.got))
	}
	// Web Push is off on this relay: no key, and 501 for a browser.
	res, _ := http.Get(f.server.URL + "/relay/v1/push/key")
	if res.StatusCode != http.StatusNotFound {
		t.Fatalf("push key without VAPID: %d", res.StatusCode)
	}
	if status, _ := f.post(map[string]any{"subscription": map[string]any{"endpoint": "https://web.push.apple.com/x"}, "encrypted": b64.EncodeToString(make([]byte, 120))}, nil); status != http.StatusNotImplemented {
		t.Fatalf("web push without VAPID: %d", status)
	}
}

func TestLoadAPNsRefusesBadKeysAndIDs(t *testing.T) {
	path, _ := writeP8(t)
	for _, config := range []apnsConfig{
		{keyID: "", teamID: "TEAM123456", topic: "com.lynshen.aogiri"},
		{keyID: "ABC123DEFG", teamID: "team", topic: "com.lynshen.aogiri"},
		{keyID: "ABC123DEFG", teamID: "TEAM123456", topic: ""},
	} {
		if _, err := loadAPNs(path, config); err == nil {
			t.Errorf("%+v loaded", config)
		}
	}
	rsaKey, _ := rsa.GenerateKey(rand.Reader, 2048)
	der, _ := x509.MarshalPKCS8PrivateKey(rsaKey)
	other := filepath.Join(t.TempDir(), "rsa.p8")
	os.WriteFile(other, pem.EncodeToMemory(&pem.Block{Type: "PRIVATE KEY", Bytes: der}), 0o600)
	if _, err := loadAPNs(other, apnsConfig{keyID: "ABC123DEFG", teamID: "TEAM123456", topic: "x"}); err == nil {
		t.Error("an RSA key was loaded as an APNs key")
	}
}

// fcmFixture is a relay with FCM only, its token endpoint and API local.
func fcmFixture(t *testing.T) (*pushFixture, *recorder, *recorder, *fcmSender, *rsa.PrivateKey) {
	t.Helper()
	key, _ := rsa.GenerateKey(rand.Reader, 2048)
	der, _ := x509.MarshalPKCS8PrivateKey(key)
	account, _ := json.Marshal(serviceAccount{
		Type: "service_account", ProjectID: "aogiri-demo", PrivateKeyID: "k1",
		PrivateKey:  string(pem.EncodeToMemory(&pem.Block{Type: "PRIVATE KEY", Bytes: der})),
		ClientEmail: "relay@aogiri-demo.iam.gserviceaccount.com", TokenURI: fcmTokenURI,
	})
	path := filepath.Join(t.TempDir(), "fcm.json")
	os.WriteFile(path, account, 0o600)
	fcm, err := loadFCM(path)
	if err != nil {
		t.Fatal(err)
	}
	oauth := &recorder{status: 200, answer: `{"access_token":"ya29.test","expires_in":3599,"token_type":"Bearer"}`}
	api := &recorder{status: 200, answer: `{"name":"projects/aogiri-demo/messages/1"}`}
	mux := http.NewServeMux()
	mux.HandleFunc("/token", oauth.handler)
	mux.HandleFunc("/", api.handler)
	server := httptest.NewTLSServer(mux)
	t.Cleanup(server.Close)
	fcm.client = server.Client()
	fcm.tokenURI = server.URL + "/token"
	fcm.api = server.URL
	return nativeFixture(t, natives{"fcm": fcm}), oauth, api, fcm, key
}

func TestAnApprovalGoesToFCMAsADataMessageExpoShows(t *testing.T) {
	f, oauth, api, _, key := fcmFixture(t)
	token := "dXNlcjEyMzQ1Njc4OTA:APA91bH-test_token"
	sub := map[string]any{"provider": "fcm", "token": token}
	for i := 0; i < 2; i++ {
		if status, text := f.post(map[string]any{"subscription": sub, "payload": approvalNotice(), "ttl": 3600, "topic": "c2Vzc2lvbi0x"}, nil); status != http.StatusNoContent {
			t.Fatalf("%d %s", status, text)
		}
	}
	oauth.mu.Lock()
	if len(oauth.got) != 1 {
		t.Fatalf("%d token requests for two messages", len(oauth.got))
	}
	form, _ := url.ParseQuery(string(oauth.got[0].body))
	oauth.mu.Unlock()
	if form.Get("grant_type") != "urn:ietf:params:oauth:grant-type:jwt-bearer" {
		t.Fatalf("form %v", form)
	}
	parts := strings.Split(form.Get("assertion"), ".")
	digest := sha256.Sum256([]byte(parts[0] + "." + parts[1]))
	if err := rsa.VerifyPKCS1v15(&key.PublicKey, crypto.SHA256, digest[:], unb64(t, parts[2])); err != nil {
		t.Fatal("the assertion does not verify")
	}
	var claims map[string]any
	json.Unmarshal(unb64(t, parts[1]), &claims)
	if claims["iss"] != "relay@aogiri-demo.iam.gserviceaccount.com" || claims["scope"] != fcmScope || claims["aud"] != fcmTokenURI {
		t.Fatalf("claims %v", claims)
	}
	got := api.last(t)
	if got.r.URL.Path != "/v1/projects/aogiri-demo/messages:send" || got.r.Header.Get("Authorization") != "Bearer ya29.test" {
		t.Fatalf("%s %v", got.r.URL, got.r.Header)
	}
	var message struct {
		Message struct {
			Token        string            `json:"token"`
			Data         map[string]string `json:"data"`
			Notification any               `json:"notification"`
			Android      map[string]any    `json:"android"`
		} `json:"message"`
	}
	json.Unmarshal(got.body, &message)
	m := message.Message
	if m.Token != token || m.Notification != nil || m.Data["title"] != "等你处理" || m.Data["message"] != "「修登录页」等你批准" ||
		m.Data["channelId"] != "approvals" || m.Data["categoryId"] != "approval" || m.Data["tag"] != "s-1" {
		t.Fatalf("message %+v", m)
	}
	var data map[string]any
	json.Unmarshal([]byte(m.Data["body"]), &data)
	if data["session"] != "s-1" || data["call_id"] != "call_9" || data["host"] != "h0st-1d" {
		t.Fatalf("data %v", data)
	}
	if m.Android["priority"] != "HIGH" || m.Android["ttl"] != "3600s" || m.Android["collapse_key"] != "c2Vzc2lvbi0x" {
		t.Fatalf("android %v", m.Android)
	}
}

func TestFCMAnswersArePassedOn(t *testing.T) {
	f, oauth, api, fcm, _ := fcmFixture(t)
	sub := map[string]any{"provider": "fcm", "token": "dXNlcjEyMzQ1Njc4OTA:APA91bH-test_token"}
	unregistered := `{"error":{"code":404,"status":"NOT_FOUND","details":[{"@type":"type.googleapis.com/google.firebase.fcm.v1.FcmError","errorCode":"UNREGISTERED"}]}}`
	for _, c := range []struct {
		status int
		answer string
		want   int
	}{
		{404, unregistered, 410},
		{400, `{"error":{"code":400,"status":"INVALID_ARGUMENT"}}`, 502},
		{429, `{"error":{"code":429,"status":"RESOURCE_EXHAUSTED"}}`, 429},
		{401, `{"error":{"code":401,"status":"UNAUTHENTICATED"}}`, 502},
	} {
		api.set(c.status, c.answer)
		if status, text := f.post(map[string]any{"subscription": sub, "payload": approvalNotice()}, nil); status != c.want {
			t.Errorf("FCM %d: relay %d %s, want %d", c.status, status, text, c.want)
		}
	}
	fcm.mu.Lock()
	if fcm.access != "" {
		t.Error("an access token FCM refused is kept")
	}
	fcm.mu.Unlock()
	oauth.set(400, `{"error":"invalid_grant"}`)
	if status, _ := f.post(map[string]any{"subscription": sub, "payload": approvalNotice()}, nil); status != http.StatusBadGateway {
		t.Errorf("no access token: %d", status)
	}
}

func TestLoadFCMRefusesAnotherTokenEndpoint(t *testing.T) {
	path := filepath.Join(t.TempDir(), "fcm.json")
	os.WriteFile(path, []byte(`{"type":"service_account","project_id":"p","client_email":"e","private_key":"x","token_uri":"https://evil.example/token"}`), 0o600)
	if _, err := loadFCM(path); err == nil || !strings.Contains(err.Error(), "token_uri") {
		t.Fatalf("%v", err)
	}
}
