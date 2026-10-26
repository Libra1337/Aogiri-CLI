package main

import (
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/coder/websocket"
)

// The gateway's Rust test (src/account_computers.rs) signs exactly this token
// with the seed 0x01 * 32.
const (
	vectorKey   = "iojj3XQJ8ZX9UtstPLpdcspnCb8dlBIb83SIAbQPb1w"
	vectorToken = "lrt1.eyJhdWQiOiJseW5zaGVuLXJlbGF5Iiwia2lkIjoiTkhVUG1MMVpfUHciLCJzdWIiOiJ1c2VyLTEiLCJhdXRoeiI6ImRldi0xIiwiY2xpZW50IjoiYW9naXJpLXBob25lIiwibmFtZSI6ImlQaG9uZSIsImlhdCI6MTgwMDAwMDAwMCwiZXhwIjoxODAwMDAwNjAwfQ.KU6W_RYGnHnZXNOoRpPnSgJdfBgSU_-sv4kZ8vtFR8WGA1dDNN1DGv-6K2QYae6AUU4U83MohEIfWWj3MsmSCg"
	testService = "relay-service-token-for-tests-0123456789"
)

func TestTheGatewaysTokenVectorVerifies(t *testing.T) {
	key := ed25519.PublicKey(unb64(t, vectorKey))
	c, ok := verifyToken([]ed25519.PublicKey{key}, vectorToken, time.Unix(1_800_000_100, 0))
	if !ok {
		t.Fatal("the gateway's token does not verify")
	}
	if c.Sub != "user-1" || c.Authz != "dev-1" || c.Client != "aogiri-phone" || c.Name != "iPhone" || c.Aud != tokenAudience {
		t.Fatalf("claims %+v", c)
	}
	seed := make([]byte, 32)
	for i := range seed {
		seed[i] = 1
	}
	if got := mintToken(ed25519.NewKeyFromSeed(seed), claims{Sub: "user-1", Authz: "dev-1", Client: "aogiri-phone", Name: "iPhone", Iat: 1_800_000_000, Exp: 1_800_000_600}); got != vectorToken {
		t.Fatalf("Go signs differently:\n%s", got)
	}
}

func TestTokensFailClosed(t *testing.T) {
	_, key, _ := ed25519.GenerateKey(rand.Reader)
	_, other, _ := ed25519.GenerateKey(rand.Reader)
	keys := []ed25519.PublicKey{key.Public().(ed25519.PublicKey)}
	now := time.Unix(1_000_000, 0)
	good := claims{Sub: "u", Authz: "d", Client: "lynshen-cli", Iat: now.Unix(), Exp: now.Unix() + 600}
	token := mintToken(key, good)
	if _, ok := verifyToken(keys, token, now); !ok {
		t.Fatal("a good token is refused")
	}
	cases := map[string]string{
		"another key":    mintToken(other, good),
		"expired":        token,
		"another prefix": strings.Replace(token, "lrt1", "lrt2", 1),
		"two parts":      "lrt1.abc",
		"garbage":        "lrt1.!!.!!",
		"too long":       "lrt1." + strings.Repeat("a", maxTokenBytes),
		"no subject":     mintToken(key, claims{Authz: "d", Iat: now.Unix(), Exp: now.Unix() + 600}),
	}
	for name, bad := range cases {
		at := now
		if name == "expired" {
			at = now.Add(600 * time.Second)
		}
		if _, ok := verifyToken(keys, bad, at); ok {
			t.Errorf("%s: accepted", name)
		}
	}
	if _, ok := verifyToken(keys, token, now.Add(-60*time.Second)); !ok {
		t.Error("a clock a minute behind the gateway is refused")
	}
	if _, ok := verifyToken(keys, token, now.Add(-61*time.Second)); ok {
		t.Error("a token from the future is accepted")
	}
	parts := strings.Split(token, ".")
	forged, _ := json.Marshal(claims{Aud: tokenAudience, Kid: keyID(keys[0]), Sub: "someone-else", Authz: "d", Iat: now.Unix(), Exp: now.Unix() + 600})
	if _, ok := verifyToken(keys, parts[0]+"."+b64.EncodeToString(forged)+"."+parts[2], now); ok {
		t.Error("a changed payload is accepted")
	}
}

func TestTheBearerComesFromTheHeaderOrTheSubprotocols(t *testing.T) {
	r := httptest.NewRequest(http.MethodGet, "/", nil)
	r.Header.Set("Authorization", "Bearer abc")
	if bearer(r) != "abc" {
		t.Fatal("header")
	}
	r = httptest.NewRequest(http.MethodGet, "/", nil)
	r.Header.Set("Sec-WebSocket-Protocol", "lynshen-relay, lynshen-bearer.lrt1.x.y")
	if bearer(r) != "lrt1.x.y" {
		t.Fatalf("subprotocol: %q", bearer(r))
	}
	r = httptest.NewRequest(http.MethodGet, "/", nil)
	r.Header.Set("Sec-WebSocket-Protocol", "lynshen-bearer.lrt1.x.y")
	if bearer(r) != "" {
		t.Fatal("a bearer subprotocol without lynshen-relay counts")
	}
}

func mintToken(key ed25519.PrivateKey, c claims) string {
	c.Aud = tokenAudience
	c.Kid = keyID(key.Public().(ed25519.PublicKey))
	raw, _ := json.Marshal(c)
	payload := b64.EncodeToString(raw)
	return tokenPrefix + "." + payload + "." + b64.EncodeToString(ed25519.Sign(key, []byte(tokenContext+payload)))
}

// fakeGateway answers the relay's gateway calls (account-computers.spec.md
// AC-API-2, AC-R1, AC-R2) for tokens it minted.
type fakeGateway struct {
	t      *testing.T
	key    ed25519.PrivateKey
	server *httptest.Server

	mu         sync.Mutex
	registered []map[string]string
	heartbeats []map[string]any
	revoked    map[string]bool
	refuse     bool
}

func newFakeGateway(t *testing.T) *fakeGateway {
	_, key, _ := ed25519.GenerateKey(rand.Reader)
	g := &fakeGateway{t: t, key: key, revoked: map[string]bool{}}
	public := key.Public().(ed25519.PublicKey)
	mux := http.NewServeMux()
	mux.HandleFunc("/api/desktop/oauth/relay-key", func(w http.ResponseWriter, r *http.Request) {
		json.NewEncoder(w).Encode(map[string]any{"keys": []map[string]string{{"kid": keyID(public), "public_key": b64.EncodeToString(public)}}})
	})
	service := func(w http.ResponseWriter, r *http.Request) bool {
		if r.Header.Get("Authorization") != "Bearer "+testService {
			http.Error(w, "unknown relay", http.StatusUnauthorized)
			return false
		}
		return true
	}
	mux.HandleFunc("/api/relay/v1/computers", func(w http.ResponseWriter, r *http.Request) {
		if !service(w, r) {
			return
		}
		var body map[string]string
		json.NewDecoder(r.Body).Decode(&body)
		c, ok := verifyToken([]ed25519.PublicKey{public}, body["token"], time.Now())
		g.mu.Lock()
		g.registered = append(g.registered, body)
		refuse := g.refuse
		g.mu.Unlock()
		if !ok || refuse {
			http.Error(w, "invalid token", http.StatusUnauthorized)
			return
		}
		json.NewEncoder(w).Encode(map[string]any{"user": c.Sub, "computer": map[string]string{"host_id": body["host_id"]}})
	})
	mux.HandleFunc("/api/relay/v1/heartbeat", func(w http.ResponseWriter, r *http.Request) {
		if !service(w, r) {
			return
		}
		var body struct {
			Hosts   []heartbeatHost   `json:"hosts"`
			Offline []string          `json:"offline"`
			Streams []heartbeatStream `json:"streams"`
		}
		json.NewDecoder(r.Body).Decode(&body)
		g.mu.Lock()
		defer g.mu.Unlock()
		raw, _ := json.Marshal(body)
		var recorded map[string]any
		json.Unmarshal(raw, &recorded)
		g.heartbeats = append(g.heartbeats, recorded)
		dropHosts, dropAuthz := []string{}, []string{}
		for _, h := range body.Hosts {
			if g.revoked[h.Authz] {
				dropHosts = append(dropHosts, h.Host)
			}
		}
		for _, s := range body.Streams {
			if g.revoked[s.Authz] {
				dropAuthz = append(dropAuthz, s.Authz)
			}
		}
		json.NewEncoder(w).Encode(map[string]any{"drop_hosts": dropHosts, "drop_authz": dropAuthz})
	})
	g.server = httptest.NewServer(mux)
	t.Cleanup(g.server.Close)
	return g
}

func (g *fakeGateway) token(user, authz, client, name string) string {
	now := time.Now().Unix()
	return mintToken(g.key, claims{Sub: user, Authz: authz, Client: client, Name: name, Iat: now, Exp: now + 600})
}

type v2Fixture struct {
	t       *testing.T
	gateway *fakeGateway
	relay   *relay
	server  *httptest.Server
	ws      string
	ctx     context.Context
}

func newV2Fixture(t *testing.T) *v2Fixture {
	t.Helper()
	g := newFakeGateway(t)
	rl := newRelay()
	rl.accounts = newAccounts(g.server.URL, testService, nil)
	if err := rl.accounts.refreshKeys(context.Background()); err != nil {
		t.Fatal(err)
	}
	server := httptest.NewServer(routes(rl, nil))
	t.Cleanup(server.Close)
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	t.Cleanup(cancel)
	return &v2Fixture{t: t, gateway: g, relay: rl, server: server, ws: "ws" + strings.TrimPrefix(server.URL, "http"), ctx: ctx}
}

type testHost struct {
	conn    *websocket.Conn
	id      string
	public  ed25519.PublicKey
	private ed25519.PrivateKey
	ready   map[string]string
}

// connectHost runs the v2 host handshake (§7.2) with `token`.
func (f *v2Fixture) connectHost(token string) (*testHost, error) {
	public, private, _ := ed25519.GenerateKey(rand.Reader)
	return f.connectHostWith(token, public, private)
}

func (f *v2Fixture) connectHostWith(token string, public ed25519.PublicKey, private ed25519.PrivateKey) (*testHost, error) {
	id := hostID(public)
	header := http.Header{}
	header.Set("Authorization", "Bearer "+token)
	conn, res, err := websocket.Dial(f.ctx, f.ws+"/relay/v2/host?host="+id, &websocket.DialOptions{HTTPHeader: header})
	if err != nil {
		if res != nil {
			return nil, fmt.Errorf("status %d", res.StatusCode)
		}
		return nil, err
	}
	_, text, err := conn.Read(f.ctx)
	if err != nil {
		return nil, err
	}
	var challenge struct{ Nonce string }
	json.Unmarshal(text, &challenge)
	signature := ed25519.Sign(private, append([]byte("lynshen-relay-v1:"), unb64(f.t, challenge.Nonce)...))
	static := make([]byte, 32)
	rand.Read(static)
	auth, _ := json.Marshal(map[string]string{
		"t": "auth", "pub": b64.EncodeToString(public), "sig": b64.EncodeToString(signature), "v": "0.8.0",
		"static": b64.EncodeToString(static), "name": "Chad 的 MacBook", "platform": "macos",
	})
	conn.Write(f.ctx, websocket.MessageText, auth)
	_, text, err = conn.Read(f.ctx)
	if err != nil {
		return nil, err
	}
	var ready map[string]string
	json.Unmarshal(text, &ready)
	return &testHost{conn: conn, id: id, public: public, private: private, ready: ready}, nil
}

// browser connects as a web page does: the token in the subprotocols.
func (f *v2Fixture) browser(host, token string) (*websocket.Conn, *http.Response, error) {
	return websocket.Dial(f.ctx, f.ws+"/relay/v2/connect?host="+host, &websocket.DialOptions{
		Subprotocols: []string{subprotocol, bearerSubprotocol + token},
	})
}

func closeCode(ctx context.Context, conn *websocket.Conn) websocket.StatusCode {
	_, _, err := conn.Read(ctx)
	return websocket.CloseStatus(err)
}

func readFrame(t *testing.T, ctx context.Context, conn *websocket.Conn) (byte, uint32, []byte) {
	t.Helper()
	_, frame, err := conn.Read(ctx)
	if err != nil || len(frame) < 5 {
		t.Fatalf("frame %v: %v", frame, err)
	}
	return frame[0], binary.BigEndian.Uint32(frame[1:5]), frame[5:]
}

func TestV2HostRegistersAndOnlyItsAccountConnects(t *testing.T) {
	f := newV2Fixture(t)
	g := f.gateway

	// Refused before the upgrade: no token, a bad one, an app's token as a host.
	if _, err := f.connectHost(""); err == nil || !strings.Contains(err.Error(), "401") {
		t.Fatalf("no token: %v", err)
	}
	if _, err := f.connectHost("lrt1.a.b"); err == nil || !strings.Contains(err.Error(), "401") {
		t.Fatalf("bad token: %v", err)
	}
	if _, err := f.connectHost(g.token("u1", "phone-1", "aogiri-phone", "iPhone")); err == nil || !strings.Contains(err.Error(), "403") {
		t.Fatalf("a phone as a host: %v", err)
	}

	h, err := f.connectHost(g.token("u1", "dev-mac", "lynshen-cli", "Mac"))
	if err != nil {
		t.Fatal(err)
	}
	defer h.conn.CloseNow()
	if h.ready["t"] != "ready" || h.ready["host"] != h.id || h.ready["user"] != "u1" {
		t.Fatalf("ready %v", h.ready)
	}
	g.mu.Lock()
	reg := g.registered[len(g.registered)-1]
	g.mu.Unlock()
	if reg["host_id"] != h.id || reg["name"] != "Chad 的 MacBook" || reg["platform"] != "macos" || reg["version"] != "0.8.0" || len(unb64(t, reg["static_pub"])) != 32 {
		t.Fatalf("registration %v", reg)
	}

	// A browser of the same account: subprotocol answered, open frame names the caller.
	client, res, err := f.browser(h.id, g.token("u1", "web-1", "lynshen-web", "Safari"))
	if err != nil {
		t.Fatal(err)
	}
	defer client.CloseNow()
	if res.Header.Get("Sec-WebSocket-Protocol") != subprotocol {
		t.Fatalf("subprotocol answer %q", res.Header.Get("Sec-WebSocket-Protocol"))
	}
	kind, sid, payload := readFrame(t, f.ctx, h.conn)
	var caller map[string]string
	json.Unmarshal(payload, &caller)
	if kind != kindOpen || caller["user"] != "u1" || caller["authz"] != "web-1" || caller["client"] != "lynshen-web" || caller["name"] != "Safari" {
		t.Fatalf("open %d %v", kind, caller)
	}
	client.Write(f.ctx, websocket.MessageBinary, []byte("noise msg 1"))
	if kind, got, data := readFrame(t, f.ctx, h.conn); kind != kindData || got != sid || string(data) != "noise msg 1" {
		t.Fatalf("data %d %d %q", kind, got, data)
	}
	reply := append([]byte{kindData, 0, 0, 0, 0}, "noise msg 2"...)
	binary.BigEndian.PutUint32(reply[1:5], sid)
	h.conn.Write(f.ctx, websocket.MessageBinary, reply)
	if _, got, _ := client.Read(f.ctx); string(got) != "noise msg 2" {
		t.Fatalf("client got %q", got)
	}

	// A native client with the header works too.
	header := http.Header{}
	header.Set("Authorization", "Bearer "+g.token("u1", "phone-1", "aogiri-phone", "iPhone"))
	native, _, err := websocket.Dial(f.ctx, f.ws+"/relay/v2/connect?host="+h.id, &websocket.DialOptions{HTTPHeader: header})
	if err != nil {
		t.Fatal(err)
	}
	defer native.CloseNow()
	if kind, _, payload := readFrame(t, f.ctx, h.conn); kind != kindOpen || !strings.Contains(string(payload), `"client":"aogiri-phone"`) {
		t.Fatalf("native open %q", payload)
	}

	// Another account: 4403. An unknown host: 4404. A bad token: 401 before the upgrade.
	other, _, err := f.browser(h.id, g.token("u2", "web-2", "lynshen-web", "x"))
	if err != nil {
		t.Fatal(err)
	}
	if code := closeCode(f.ctx, other); code != 4403 {
		t.Fatalf("another account: %d", code)
	}
	gone, _, err := f.browser("nobody", g.token("u1", "web-1", "lynshen-web", "x"))
	if err != nil {
		t.Fatal(err)
	}
	if code := closeCode(f.ctx, gone); code != 4404 {
		t.Fatalf("offline host: %d", code)
	}
	if _, res, err := f.browser(h.id, "lrt1.x.y"); err == nil || res == nil || res.StatusCode != http.StatusUnauthorized {
		t.Fatalf("bad token: %v", err)
	}
	if _, res, err := websocket.Dial(f.ctx, f.ws+"/relay/v2/connect?host="+h.id, nil); err == nil || res == nil || res.StatusCode != http.StatusUnauthorized {
		t.Fatalf("no token: %v", err)
	}
}

func TestAHostWhoseRegistrationIsRefusedOrWhoseIDDiffersIsClosed(t *testing.T) {
	f := newV2Fixture(t)
	public, private, _ := ed25519.GenerateKey(rand.Reader)
	header := http.Header{}
	header.Set("Authorization", "Bearer "+f.gateway.token("u1", "d", "lynshen-desktop", "Mac"))
	conn, _, err := websocket.Dial(f.ctx, f.ws+"/relay/v2/host?host=someone-else", &websocket.DialOptions{HTTPHeader: header})
	if err != nil {
		t.Fatal(err)
	}
	_, text, _ := conn.Read(f.ctx)
	var challenge struct{ Nonce string }
	json.Unmarshal(text, &challenge)
	signature := ed25519.Sign(private, append([]byte("lynshen-relay-v1:"), unb64(t, challenge.Nonce)...))
	auth, _ := json.Marshal(map[string]string{"t": "auth", "pub": b64.EncodeToString(public), "sig": b64.EncodeToString(signature), "static": b64.EncodeToString(make([]byte, 32)), "platform": "macos"})
	conn.Write(f.ctx, websocket.MessageText, auth)
	if code := closeCode(f.ctx, conn); code != 4400 {
		t.Fatalf("host id mismatch: %d", code)
	}
	f.gateway.mu.Lock()
	f.gateway.refuse = true
	f.gateway.mu.Unlock()
	_, err = f.connectHost(f.gateway.token("u1", "d", "lynshen-desktop", "Mac"))
	if websocket.CloseStatus(err) != 4401 {
		t.Fatalf("refused registration: %v", err)
	}
}

func TestHeartbeatsReportHostsAndDropRevokedLogins(t *testing.T) {
	f := newV2Fixture(t)
	g := f.gateway
	h, err := f.connectHost(g.token("u1", "dev-mac", "lynshen-cli", "Mac"))
	if err != nil {
		t.Fatal(err)
	}
	defer h.conn.CloseNow()
	phone, _, err := f.browser(h.id, g.token("u1", "phone-1", "aogiri-phone", "iPhone"))
	if err != nil {
		t.Fatal(err)
	}
	readFrame(t, f.ctx, h.conn)
	web, _, err := f.browser(h.id, g.token("u1", "web-1", "lynshen-web", "Safari"))
	if err != nil {
		t.Fatal(err)
	}
	readFrame(t, f.ctx, h.conn)
	if err := f.relay.heartbeat(f.ctx); err != nil {
		t.Fatal(err)
	}
	g.mu.Lock()
	beat := g.heartbeats[0]
	g.mu.Unlock()
	if got := fmt.Sprint(beat["hosts"]); !strings.Contains(got, h.id) || !strings.Contains(got, "dev-mac") {
		t.Fatalf("hosts %s", got)
	}
	if got := fmt.Sprint(beat["streams"]); !strings.Contains(got, "phone-1") || !strings.Contains(got, "web-1") {
		t.Fatalf("streams %s", got)
	}

	// The phone's login is removed on the account: its stream closes 4403, the web page stays.
	g.mu.Lock()
	g.revoked["phone-1"] = true
	g.mu.Unlock()
	if err := f.relay.heartbeat(f.ctx); err != nil {
		t.Fatal(err)
	}
	if code := closeCode(f.ctx, phone); code != 4403 {
		t.Fatalf("revoked phone: %d", code)
	}
	if kind, _, _ := readFrame(t, f.ctx, h.conn); kind != kindClose {
		t.Fatalf("the host hears of it: %d", kind)
	}
	web.Write(f.ctx, websocket.MessageBinary, []byte("still here"))
	if _, _, data := readFrame(t, f.ctx, h.conn); string(data) != "still here" {
		t.Fatalf("web %q", data)
	}

	// The computer's own login is removed: the host is closed 4403, then reported offline.
	g.mu.Lock()
	g.revoked["dev-mac"] = true
	g.mu.Unlock()
	if err := f.relay.heartbeat(f.ctx); err != nil {
		t.Fatal(err)
	}
	if code := closeCode(f.ctx, h.conn); code != 4403 {
		t.Fatalf("revoked host: %d", code)
	}
	if code := closeCode(f.ctx, web); code != 4404 {
		t.Fatalf("its clients: %d", code)
	}
	deadline := time.Now().Add(5 * time.Second)
	for {
		f.relay.mu.Lock()
		gone := f.relay.hosts[h.id] == nil
		f.relay.mu.Unlock()
		if gone || time.Now().After(deadline) {
			break
		}
		time.Sleep(20 * time.Millisecond)
	}
	if err := f.relay.heartbeat(f.ctx); err != nil {
		t.Fatal(err)
	}
	g.mu.Lock()
	last := g.heartbeats[len(g.heartbeats)-1]
	g.mu.Unlock()
	if got := fmt.Sprint(last["offline"]); !strings.Contains(got, h.id) {
		t.Fatalf("offline %s", got)
	}
}

func TestAV1ClientStillReachesAV2HostAndV1HostsStillWork(t *testing.T) {
	f := newV2Fixture(t)
	h, err := f.connectHost(f.gateway.token("u1", "dev-mac", "lynshen-desktop", "Mac"))
	if err != nil {
		t.Fatal(err)
	}
	defer h.conn.CloseNow()
	// An Aogiri release from before v2 dials v1 without a token.
	old, _, err := websocket.Dial(f.ctx, f.ws+"/relay/v1/connect?host="+h.id, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer old.CloseNow()
	kind, sid, payload := readFrame(t, f.ctx, h.conn)
	if kind != kindOpen || string(payload) != `{"v":1}` {
		t.Fatalf("v1 open to a v2 host: %d %q", kind, payload)
	}
	old.Write(f.ctx, websocket.MessageBinary, []byte("hello"))
	if _, got, data := readFrame(t, f.ctx, h.conn); got != sid || string(data) != "hello" {
		t.Fatalf("v1 data %q", data)
	}
	// A v1 daemon (no token) on v1: an empty open payload, as before.
	conn, _, err := websocket.Dial(f.ctx, f.ws+"/relay/v1/host", nil)
	if err != nil {
		t.Fatal(err)
	}
	defer conn.CloseNow()
	_, text, _ := conn.Read(f.ctx)
	var challenge struct{ Nonce string }
	json.Unmarshal(text, &challenge)
	public, private, _ := ed25519.GenerateKey(rand.Reader)
	signature := ed25519.Sign(private, append([]byte("lynshen-relay-v1:"), unb64(t, challenge.Nonce)...))
	auth, _ := json.Marshal(map[string]string{"t": "auth", "pub": b64.EncodeToString(public), "sig": b64.EncodeToString(signature), "v": "0.7.9"})
	conn.Write(f.ctx, websocket.MessageText, auth)
	_, text, _ = conn.Read(f.ctx)
	if !strings.Contains(string(text), `"t":"ready"`) || strings.Contains(string(text), "user") {
		t.Fatalf("v1 ready %s", text)
	}
	client, _, _ := websocket.Dial(f.ctx, f.ws+"/relay/v1/connect?host="+hostID(public), nil)
	defer client.CloseNow()
	if kind, _, payload := readFrame(t, f.ctx, conn); kind != kindOpen || len(payload) != 0 {
		t.Fatalf("v1 open %d %q", kind, payload)
	}
	// A v2 client cannot reach a v1 host: it has no account.
	v2, _, err := f.browser(hostID(public), f.gateway.token("u1", "web-1", "lynshen-web", "x"))
	if err != nil {
		t.Fatal(err)
	}
	if code := closeCode(f.ctx, v2); code != 4403 {
		t.Fatalf("v2 client to a v1 host: %d", code)
	}
}

func TestStreamLimitsAndDrainCloseWithCodesBrowsersRead(t *testing.T) {
	f := newV2Fixture(t)
	h, err := f.connectHost(f.gateway.token("u1", "dev-mac", "lynshen-cli", "Mac"))
	if err != nil {
		t.Fatal(err)
	}
	defer h.conn.CloseNow()
	go func() {
		for {
			if _, _, err := h.conn.Read(context.Background()); err != nil {
				return
			}
		}
	}()
	var clients []*websocket.Conn
	for i := 0; i < streamsPerHost; i++ {
		// Every connection comes from a new address, so the per-IP limit stays out of it.
		f.relay.rate.mu.Lock()
		f.relay.rate.events = map[string][]time.Time{}
		f.relay.rate.mu.Unlock()
		c, _, err := f.browser(h.id, f.gateway.token("u1", fmt.Sprint("d", i), "lynshen-web", "x"))
		if err != nil {
			t.Fatal(err)
		}
		clients = append(clients, c)
	}
	deadline := time.Now().Add(5 * time.Second)
	for {
		f.relay.mu.Lock()
		hh := f.relay.hosts[h.id]
		f.relay.mu.Unlock()
		hh.mu.Lock()
		n := len(hh.streams)
		hh.mu.Unlock()
		if n == streamsPerHost || time.Now().After(deadline) {
			break
		}
		time.Sleep(10 * time.Millisecond)
	}
	extra, _, err := f.browser(h.id, f.gateway.token("u1", "extra", "lynshen-web", "x"))
	if err != nil {
		t.Fatal(err)
	}
	if code := closeCode(f.ctx, extra); code != 4429 {
		t.Fatalf("stream 33: %d", code)
	}
	// The per-IP limit: the 21st new connection in a minute.
	f.relay.rate.mu.Lock()
	f.relay.rate.events = map[string][]time.Time{}
	f.relay.rate.mu.Unlock()
	for i := 0; i < clientsPerMinute; i++ {
		f.relay.rate.allow("connect:127.0.0.1", clientsPerMinute)
	}
	limited, _, err := f.browser(h.id, f.gateway.token("u1", "limited", "lynshen-web", "x"))
	if err != nil {
		t.Fatal(err)
	}
	if code := closeCode(f.ctx, limited); code != 4429 {
		t.Fatalf("per-IP limit: %d", code)
	}

	// A restart: every v2 stream and the host hear 4503; new connections too.
	f.relay.drain()
	for i, c := range clients {
		if code := closeCode(f.ctx, c); code != 4503 {
			t.Fatalf("client %d on drain: %d", i, code)
		}
	}
	f.relay.rate.mu.Lock()
	f.relay.rate.events = map[string][]time.Time{}
	f.relay.rate.mu.Unlock()
	late, _, err := f.browser(h.id, f.gateway.token("u1", "late", "lynshen-web", "x"))
	if err != nil {
		t.Fatal(err)
	}
	if code := closeCode(f.ctx, late); code != 4503 {
		t.Fatalf("a connection while draining: %d", code)
	}
}

func TestWithoutAGatewayOnlyV1IsServedAndNoV1HidesIt(t *testing.T) {
	rl := newRelay()
	server := httptest.NewServer(routes(rl, nil))
	defer server.Close()
	res, _ := http.Get(server.URL + "/relay/v2/healthz")
	if res.StatusCode != http.StatusNotFound {
		t.Fatalf("v2 without a gateway: %d", res.StatusCode)
	}
	f := newV2Fixture(t)
	f.relay.v1 = false
	only := httptest.NewServer(routes(f.relay, nil))
	defer only.Close()
	for path, want := range map[string]int{"/relay/v1/connect": 404, "/relay/v1/host": 404, "/relay/v2/healthz": 200, "/relay/v1/healthz": 200} {
		res, err := http.Get(only.URL + path)
		if err != nil {
			t.Fatal(err)
		}
		io.Copy(io.Discard, res.Body)
		res.Body.Close()
		if res.StatusCode != want {
			t.Errorf("%s: %d, want %d", path, res.StatusCode, want)
		}
	}
}

func TestPushStillGoesThroughForAV2Host(t *testing.T) {
	f := newV2Fixture(t)
	pf := newPushFixture(t)
	pf.relay.accounts = f.relay.accounts
	server := httptest.NewServer(routes(pf.relay, pf.push))
	defer server.Close()
	pf.server = server
	// The push fixture's host key connects through v2.
	ws := "ws" + strings.TrimPrefix(server.URL, "http")
	header := http.Header{}
	header.Set("Authorization", "Bearer "+f.gateway.token("u1", "dev-mac", "lynshen-cli", "Mac"))
	conn, _, err := websocket.Dial(f.ctx, ws+"/relay/v2/host?host="+hostID(pf.hostPub), &websocket.DialOptions{HTTPHeader: header})
	if err != nil {
		t.Fatal(err)
	}
	defer conn.CloseNow()
	_, text, _ := conn.Read(f.ctx)
	var challenge struct{ Nonce string }
	json.Unmarshal(text, &challenge)
	signature := ed25519.Sign(pf.hostKey, append([]byte("lynshen-relay-v1:"), unb64(t, challenge.Nonce)...))
	auth, _ := json.Marshal(map[string]string{"t": "auth", "pub": b64.EncodeToString(pf.hostPub), "sig": b64.EncodeToString(signature), "static": b64.EncodeToString(make([]byte, 32)), "platform": "linux"})
	conn.Write(f.ctx, websocket.MessageText, auth)
	if _, text, _ := conn.Read(f.ctx); !strings.Contains(string(text), `"user":"u1"`) {
		t.Fatalf("ready %s", text)
	}
	status, body := pf.post(map[string]any{"subscription": pf.subscription(), "payload": map[string]string{"title": "t"}}, nil)
	if status != http.StatusNoContent {
		t.Fatalf("push for a v2 host: %d %s", status, body)
	}
}

// A Live Activity push goes through for a host connected over v2, like a
// notification: the push endpoint knows hosts by their key, not by version.
func TestALiveActivityGoesThroughForAV2Host(t *testing.T) {
	f := newV2Fixture(t)
	pf, rec, _, _ := apnsFixture(t)
	pf.relay.mu.Lock()
	delete(pf.relay.hosts, hostID(pf.hostPub))
	pf.relay.mu.Unlock()
	pf.relay.accounts = f.relay.accounts
	server := httptest.NewServer(routes(pf.relay, pf.push))
	defer server.Close()
	pf.server = server
	activity := map[string]any{
		"subscription": activitySub(),
		"liveactivity": map[string]any{"event": "update", "content_state": runningState(), "timestamp": time.Now().Unix()},
	}
	if status, body := pf.post(activity, nil); status != http.StatusForbidden {
		t.Fatalf("before the host connects: %d %s", status, body)
	}
	ws := "ws" + strings.TrimPrefix(server.URL, "http")
	header := http.Header{}
	header.Set("Authorization", "Bearer "+f.gateway.token("u1", "dev-mac", "lynshen-cli", "Mac"))
	conn, _, err := websocket.Dial(f.ctx, ws+"/relay/v2/host?host="+hostID(pf.hostPub), &websocket.DialOptions{HTTPHeader: header})
	if err != nil {
		t.Fatal(err)
	}
	defer conn.CloseNow()
	_, text, _ := conn.Read(f.ctx)
	var challenge struct{ Nonce string }
	json.Unmarshal(text, &challenge)
	signature := ed25519.Sign(pf.hostKey, append([]byte("lynshen-relay-v1:"), unb64(t, challenge.Nonce)...))
	auth, _ := json.Marshal(map[string]string{"t": "auth", "pub": b64.EncodeToString(pf.hostPub), "sig": b64.EncodeToString(signature), "static": b64.EncodeToString(make([]byte, 32)), "platform": "linux"})
	conn.Write(f.ctx, websocket.MessageText, auth)
	if _, text, _ := conn.Read(f.ctx); !strings.Contains(string(text), `"user":"u1"`) {
		t.Fatalf("ready %s", text)
	}
	activity["liveactivity"].(map[string]any)["timestamp"] = time.Now().Unix()
	if status, body := pf.post(activity, nil); status != http.StatusNoContent {
		t.Fatalf("live activity for a v2 host: %d %s", status, body)
	}
	if h := rec.last(t).r.Header; h.Get("apns-push-type") != "liveactivity" || h.Get("apns-topic") != "com.lynshen.aogiri.push-type.liveactivity" {
		t.Fatalf("headers %v", h)
	}
}
