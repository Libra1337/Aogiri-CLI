package main

import (
	"bytes"
	"context"
	"crypto/aes"
	"crypto/cipher"
	"crypto/ecdh"
	"crypto/ecdsa"
	"crypto/ed25519"
	"crypto/elliptic"
	"crypto/hkdf"
	"crypto/rand"
	"crypto/sha256"
	"encoding/binary"
	"encoding/json"
	"io"
	"math/big"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/coder/websocket"
)

func unb64(t *testing.T, s string) []byte {
	t.Helper()
	out, err := b64.DecodeString(strings.ReplaceAll(s, " ", ""))
	if err != nil {
		t.Fatalf("base64 %q: %v", s, err)
	}
	return out
}

// RFC 8291 section 5 and appendix A.
func TestEncryptMessageMatchesRFC8291(t *testing.T) {
	plaintext := []byte("When I grow up, I want to be a watermelon")
	asPrivate := unb64(t, "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw")
	uaPublic := unb64(t, "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcx aOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4")
	auth := unb64(t, "BTBZMqHH6r4Tts7J_aSIgg")
	salt := unb64(t, "DGv6ra1nlYgDCS1FRnbzlw")
	want := unb64(t, "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml"+
		"mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPT"+
		"pK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN")
	asKey, err := ecdh.P256().NewPrivateKey(asPrivate)
	if err != nil {
		t.Fatal(err)
	}
	got, err := encryptMessage(plaintext, uaPublic, auth, asKey, salt)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(got, want) {
		t.Fatalf("encrypted message differs:\n got %s\nwant %s", b64.EncodeToString(got), b64.EncodeToString(want))
	}
	uaPrivate, _ := ecdh.P256().NewPrivateKey(unb64(t, "q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94"))
	if text := decryptMessage(t, got, uaPrivate, auth); string(text) != string(plaintext) {
		t.Fatalf("decrypted %q", text)
	}
}

func TestEncryptMessageRefusesAPointOffTheCurve(t *testing.T) {
	bad := make([]byte, 65)
	bad[0] = 4
	bad[64] = 1
	if _, err := encryptMessage([]byte("x"), bad, make([]byte, 16), nil, make([]byte, 16)); err == nil {
		t.Fatal("a point off the curve was accepted")
	}
}

// decryptMessage is what the browser does with a one-record message.
func decryptMessage(t *testing.T, message []byte, ua *ecdh.PrivateKey, auth []byte) []byte {
	t.Helper()
	salt := message[:16]
	if rs := binary.BigEndian.Uint32(message[16:20]); rs != recordSize {
		t.Fatalf("record size %d", rs)
	}
	idlen := int(message[20])
	asPublic := message[21 : 21+idlen]
	as, err := ecdh.P256().NewPublicKey(asPublic)
	if err != nil {
		t.Fatal(err)
	}
	shared, err := ua.ECDH(as)
	if err != nil {
		t.Fatal(err)
	}
	info := append(append([]byte("WebPush: info\x00"), ua.PublicKey().Bytes()...), asPublic...)
	ikm, _ := hkdf.Key(sha256.New, shared, auth, string(info), 32)
	cek, _ := hkdf.Key(sha256.New, ikm, salt, "Content-Encoding: aes128gcm\x00", 16)
	nonce, _ := hkdf.Key(sha256.New, ikm, salt, "Content-Encoding: nonce\x00", 12)
	block, _ := aes.NewCipher(cek)
	gcm, _ := cipher.NewGCM(block)
	plain, err := gcm.Open(nil, nonce, message[21+idlen:], nil)
	if err != nil {
		t.Fatal(err)
	}
	if plain[len(plain)-1] != 2 {
		t.Fatalf("delimiter %d", plain[len(plain)-1])
	}
	return plain[:len(plain)-1]
}

func TestEndpointsOfTheBrowserPushServicesOnly(t *testing.T) {
	for endpoint, want := range map[string]bool{
		"https://fcm.googleapis.com/fcm/send/abc":                 true,
		"https://web.push.apple.com/QGx":                          true,
		"https://updates.push.services.mozilla.com/wpush/v2/gAAA": true,
		"https://wns2-bl2p.notify.windows.com/w/?token=x":         true,
		"https://push.apple.com/x":                                false,
		"https://evil.example/web.push.apple.com":                 false,
		"https://web.push.apple.com.evil.example/x":               false,
		"http://fcm.googleapis.com/fcm/send/abc":                  false,
		"https://fcm.googleapis.com:8443/fcm/send/abc":            false,
		"https://user@fcm.googleapis.com/fcm/send/abc":            false,
		"https://127.0.0.1/x":                                     false,
		"":                                                        false,
	} {
		if _, got := allowedEndpoint(endpoint); got != want {
			t.Errorf("%q: allowed = %v, want %v", endpoint, got, want)
		}
	}
}

func testVAPID(t *testing.T) (*vapid, string) {
	t.Helper()
	path := filepath.Join(t.TempDir(), "vapid.json")
	if err := generateVAPID(path); err != nil {
		t.Fatal(err)
	}
	info, _ := os.Stat(path)
	if info.Mode().Perm() != 0o600 {
		t.Fatalf("key file mode %v", info.Mode().Perm())
	}
	if err := generateVAPID(path); err == nil {
		t.Fatal("an existing key file was replaced")
	}
	v, err := loadVAPID(path, "https://app.lynshen.org")
	if err != nil {
		t.Fatal(err)
	}
	return v, path
}

func TestVAPIDTokenIsAnES256JWTForThePushServiceOrigin(t *testing.T) {
	v, _ := testVAPID(t)
	now := time.Unix(1_800_000_000, 0)
	header, err := v.header("https://web.push.apple.com", now)
	if err != nil {
		t.Fatal(err)
	}
	if again, _ := v.header("https://web.push.apple.com", now.Add(time.Minute)); again != header {
		t.Fatal("the token is not reused")
	}
	if later, _ := v.header("https://web.push.apple.com", now.Add(jwtLifetime-30*time.Minute)); later == header {
		t.Fatal("a token close to its expiry is reused")
	}
	rest, ok := strings.CutPrefix(header, "vapid t=")
	if !ok {
		t.Fatalf("header %q", header)
	}
	token, key, ok := strings.Cut(rest, ", k=")
	if !ok || key != b64.EncodeToString(v.public) {
		t.Fatalf("header %q", header)
	}
	parts := strings.Split(token, ".")
	if len(parts) != 3 {
		t.Fatalf("token %q", token)
	}
	var claims struct {
		Aud string `json:"aud"`
		Exp int64  `json:"exp"`
		Sub string `json:"sub"`
	}
	json.Unmarshal(unb64(t, parts[1]), &claims)
	if claims.Aud != "https://web.push.apple.com" || claims.Sub != "https://app.lynshen.org" ||
		claims.Exp <= now.Unix() || claims.Exp > now.Add(24*time.Hour).Unix() {
		t.Fatalf("claims %+v", claims)
	}
	public, err := ecdsa.ParseUncompressedPublicKey(elliptic.P256(), unb64(t, key))
	if err != nil {
		t.Fatal(err)
	}
	signature := unb64(t, parts[2])
	digest := sha256.Sum256([]byte(parts[0] + "." + parts[1]))
	r, s := new(big.Int).SetBytes(signature[:32]), new(big.Int).SetBytes(signature[32:])
	if len(signature) != 64 || !ecdsa.Verify(public, digest[:], r, s) {
		t.Fatal("the token's signature does not verify")
	}
}

func TestLoadVAPIDRefusesAMismatchedPublicKey(t *testing.T) {
	_, path := testVAPID(t)
	text, _ := os.ReadFile(path)
	var file vapidFile
	json.Unmarshal(text, &file)
	other, _ := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	public, _ := other.PublicKey.Bytes()
	file.Public = b64.EncodeToString(public)
	text, _ = json.Marshal(file)
	os.WriteFile(path, text, 0o600)
	if _, err := loadVAPID(path, ""); err == nil {
		t.Fatal("a key file whose public key is another key's was loaded")
	}
}

// pushFixture is a relay with push and a fake push service.
type pushFixture struct {
	t        *testing.T
	relay    *relay
	push     *pusher
	server   *httptest.Server
	service  *httptest.Server
	hostKey  ed25519.PrivateKey
	hostPub  ed25519.PublicKey
	uaKey    *ecdh.PrivateKey
	auth     []byte
	mu       sync.Mutex
	received []*http.Request
	bodies   [][]byte
	status   int
}

func newPushFixture(t *testing.T) *pushFixture {
	t.Helper()
	v, _ := testVAPID(t)
	f := &pushFixture{t: t, status: http.StatusCreated}
	f.service = httptest.NewTLSServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		f.mu.Lock()
		f.received = append(f.received, r)
		f.bodies = append(f.bodies, body)
		status := f.status
		f.mu.Unlock()
		w.WriteHeader(status)
	}))
	t.Cleanup(f.service.Close)
	f.relay = newRelay()
	f.push = newPusher(f.relay, v, nil)
	f.push.client = f.service.Client()
	service, _ := url.Parse(f.service.URL)
	f.push.allow = func(endpoint string) (*url.URL, bool) {
		u, err := url.Parse(endpoint)
		return u, err == nil && u.Host == service.Host
	}
	f.server = httptest.NewServer(routes(f.relay, f.push))
	t.Cleanup(f.server.Close)
	f.hostPub, f.hostKey, _ = ed25519.GenerateKey(rand.Reader)
	f.uaKey, _ = ecdh.P256().GenerateKey(rand.Reader)
	f.auth = make([]byte, 16)
	rand.Read(f.auth)
	return f
}

// connectHost makes the fixture's host one the relay knows as connected.
func (f *pushFixture) connectHost() {
	f.relay.mu.Lock()
	f.relay.hosts[hostID(f.hostPub)] = &host{id: hostID(f.hostPub)}
	f.relay.mu.Unlock()
}

func (f *pushFixture) subscription() map[string]any {
	return map[string]any{
		"endpoint": f.service.URL + "/push/abc",
		"keys": map[string]string{
			"p256dh": b64.EncodeToString(f.uaKey.PublicKey().Bytes()),
			"auth":   b64.EncodeToString(f.auth),
		},
	}
}

func (f *pushFixture) post(request map[string]any, sign func([]byte) []byte) (int, string) {
	f.t.Helper()
	if _, ok := request["pub"]; !ok {
		request["pub"] = b64.EncodeToString(f.hostPub)
	}
	if _, ok := request["ts"]; !ok {
		request["ts"] = time.Now().UnixMilli()
	}
	// A daemon makes each request unique (Ed25519 signatures are
	// deterministic, and the relay refuses a signature it saw).
	nonce := make([]byte, 12)
	rand.Read(nonce)
	request["nonce"] = b64.EncodeToString(nonce)
	body, _ := json.Marshal(request)
	if sign == nil {
		sign = func(body []byte) []byte {
			return ed25519.Sign(f.hostKey, append([]byte(pushContext), body...))
		}
	}
	req, _ := http.NewRequest(http.MethodPost, f.server.URL+"/relay/v1/push", bytes.NewReader(body))
	req.Header.Set("X-LynShen-Signature", b64.EncodeToString(sign(body)))
	res, err := http.DefaultClient.Do(req)
	if err != nil {
		f.t.Fatal(err)
	}
	defer res.Body.Close()
	text, _ := io.ReadAll(res.Body)
	return res.StatusCode, strings.TrimSpace(string(text))
}

func TestPushKeyIsTheVAPIDPublicKey(t *testing.T) {
	f := newPushFixture(t)
	res, err := http.Get(f.server.URL + "/relay/v1/push/key")
	if err != nil {
		t.Fatal(err)
	}
	var body struct{ Key string }
	json.NewDecoder(res.Body).Decode(&body)
	if res.StatusCode != 200 || body.Key != b64.EncodeToString(f.push.vapid.public) || len(unb64(t, body.Key)) != 65 {
		t.Fatalf("%d %+v", res.StatusCode, body)
	}
}

func TestPushRoutesAreOffWithoutAKey(t *testing.T) {
	server := httptest.NewServer(routes(newRelay(), nil))
	defer server.Close()
	for _, path := range []string{"/relay/v1/push/key", "/relay/v1/push"} {
		res, _ := http.Get(server.URL + path)
		if res.StatusCode != http.StatusNotFound {
			t.Errorf("%s: %d", path, res.StatusCode)
		}
	}
	res, _ := http.Get(server.URL + "/relay/v1/healthz")
	if res.StatusCode != 200 {
		t.Errorf("healthz: %d", res.StatusCode)
	}
}

func TestAPlaintextPayloadIsEncryptedForTheBrowserAndSigned(t *testing.T) {
	f := newPushFixture(t)
	f.connectHost()
	payload := map[string]any{"title": "Aogiri", "body": "某会话等你批准", "tag": "s-1"}
	status, text := f.post(map[string]any{"subscription": f.subscription(), "payload": payload, "urgency": "high", "ttl": 600, "topic": "s-1"}, nil)
	if status != http.StatusNoContent {
		t.Fatalf("%d %s", status, text)
	}
	f.mu.Lock()
	defer f.mu.Unlock()
	if len(f.received) != 1 {
		t.Fatalf("%d deliveries", len(f.received))
	}
	r := f.received[0]
	if r.URL.Path != "/push/abc" || r.Header.Get("Content-Encoding") != "aes128gcm" || r.Header.Get("TTL") != "600" ||
		r.Header.Get("Urgency") != "high" || r.Header.Get("Topic") != "s-1" ||
		!strings.HasPrefix(r.Header.Get("Authorization"), "vapid t=") {
		t.Fatalf("request %v %v", r.URL, r.Header)
	}
	var got map[string]any
	json.Unmarshal(decryptMessage(t, f.bodies[0], f.uaKey, f.auth), &got)
	if got["body"] != "某会话等你批准" || got["tag"] != "s-1" {
		t.Fatalf("browser got %v", got)
	}
}

func TestAnEncryptedMessageIsPassedOnAsItIs(t *testing.T) {
	f := newPushFixture(t)
	f.connectHost()
	salt := make([]byte, 16)
	rand.Read(salt)
	message, err := encryptMessage([]byte(`{"title":"Aogiri"}`), f.uaKey.PublicKey().Bytes(), f.auth, nil, salt)
	if err != nil {
		t.Fatal(err)
	}
	sub := f.subscription()
	// The relay needs only the endpoint to pass ciphertext on.
	delete(sub, "keys")
	status, text := f.post(map[string]any{"subscription": sub, "encrypted": b64.EncodeToString(message)}, nil)
	if status != http.StatusNoContent {
		t.Fatalf("%d %s", status, text)
	}
	f.mu.Lock()
	defer f.mu.Unlock()
	if !bytes.Equal(f.bodies[0], message) || f.received[0].Header.Get("TTL") != "86400" {
		t.Fatal("the encrypted message was changed")
	}
}

func TestPushRefusals(t *testing.T) {
	f := newPushFixture(t)
	payload := map[string]any{"title": "x"}
	if status, _ := f.post(map[string]any{"subscription": f.subscription(), "payload": payload}, nil); status != http.StatusForbidden {
		t.Fatalf("a host not connected: %d", status)
	}
	f.connectHost()
	other := ed25519.NewKeyFromSeed(make([]byte, 32))
	cases := []struct {
		name    string
		request map[string]any
		sign    func([]byte) []byte
		want    int
	}{
		{"signed by another key", map[string]any{"subscription": f.subscription(), "payload": payload}, func(b []byte) []byte {
			return ed25519.Sign(other, append([]byte(pushContext), b...))
		}, http.StatusUnauthorized},
		{"signed without the context", map[string]any{"subscription": f.subscription(), "payload": payload}, func(b []byte) []byte {
			return ed25519.Sign(f.hostKey, b)
		}, http.StatusUnauthorized},
		{"too old", map[string]any{"ts": time.Now().Add(-6 * time.Minute).UnixMilli(), "subscription": f.subscription(), "payload": payload}, nil, http.StatusUnauthorized},
		{"from the future", map[string]any{"ts": time.Now().Add(6 * time.Minute).UnixMilli(), "subscription": f.subscription(), "payload": payload}, nil, http.StatusUnauthorized},
		{"not a push service", map[string]any{"subscription": map[string]any{"endpoint": "https://evil.example/x", "keys": f.subscription()["keys"]}, "payload": payload}, nil, http.StatusForbidden},
		{"getui", map[string]any{"subscription": map[string]any{"provider": "getui", "client_id": "abc"}, "payload": payload}, nil, http.StatusNotImplemented},
		{"payload too large", map[string]any{"subscription": f.subscription(), "payload": map[string]string{"body": strings.Repeat("x", maxPlaintext)}}, nil, http.StatusBadRequest},
		{"encrypted too large", map[string]any{"subscription": f.subscription(), "encrypted": b64.EncodeToString(make([]byte, maxEncrypted+1))}, nil, http.StatusBadRequest},
		{"nothing to send", map[string]any{"subscription": f.subscription()}, nil, http.StatusBadRequest},
		{"bad keys", map[string]any{"subscription": map[string]any{"endpoint": f.service.URL + "/x", "keys": map[string]string{"p256dh": "AAAA", "auth": "AAAA"}}, "payload": payload}, nil, http.StatusBadRequest},
	}
	for _, c := range cases {
		if status, text := f.post(c.request, c.sign); status != c.want {
			t.Errorf("%s: %d %s, want %d", c.name, status, text, c.want)
		}
	}
	f.mu.Lock()
	defer f.mu.Unlock()
	if len(f.received) != 0 {
		t.Fatalf("%d refused requests reached the push service", len(f.received))
	}
}

func TestAReplayedRequestIsRefused(t *testing.T) {
	f := newPushFixture(t)
	f.connectHost()
	request := map[string]any{"pub": b64.EncodeToString(f.hostPub), "subscription": f.subscription(), "payload": map[string]any{"title": "x"}, "ts": time.Now().UnixMilli()}
	body, _ := json.Marshal(request)
	signature := ed25519.Sign(f.hostKey, append([]byte(pushContext), body...))
	send := func() int {
		req, _ := http.NewRequest(http.MethodPost, f.server.URL+"/relay/v1/push", bytes.NewReader(body))
		req.Header.Set("X-LynShen-Signature", b64.EncodeToString(signature))
		res, _ := http.DefaultClient.Do(req)
		res.Body.Close()
		return res.StatusCode
	}
	if first, second := send(), send(); first != http.StatusNoContent || second != http.StatusUnauthorized {
		t.Fatalf("first %d, replay %d", first, second)
	}
}

func TestPushServiceAnswersArePassedOn(t *testing.T) {
	f := newPushFixture(t)
	f.connectHost()
	for service, want := range map[int]int{
		http.StatusCreated:               http.StatusNoContent,
		http.StatusGone:                  http.StatusGone,
		http.StatusNotFound:              http.StatusGone,
		http.StatusTooManyRequests:       http.StatusTooManyRequests,
		http.StatusForbidden:             http.StatusBadGateway,
		http.StatusRequestEntityTooLarge: http.StatusRequestEntityTooLarge,
	} {
		f.mu.Lock()
		f.status = service
		f.mu.Unlock()
		if status, _ := f.post(map[string]any{"subscription": f.subscription(), "payload": map[string]any{"title": "x"}}, nil); status != want {
			t.Errorf("service %d: relay answered %d, want %d", service, status, want)
		}
	}
}

func TestPushIsLimitedPerHost(t *testing.T) {
	f := newPushFixture(t)
	f.connectHost()
	var last int
	for i := 0; i <= pushPerHostMinute; i++ {
		last, _ = f.post(map[string]any{"subscription": f.subscription(), "payload": map[string]any{"title": "x", "n": i}}, nil)
	}
	if last != http.StatusTooManyRequests {
		t.Fatalf("request %d: %d", pushPerHostMinute+1, last)
	}
}

// The relay's own path, unchanged by push: a host authenticates, a client
// stream opens and bytes go both ways.
func TestAHostAndAClientExchangeFrames(t *testing.T) {
	rl := newRelay()
	server := httptest.NewServer(routes(rl, nil))
	defer server.Close()
	ws := "ws" + strings.TrimPrefix(server.URL, "http")
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	hostConn, _, err := websocket.Dial(ctx, ws+"/relay/v1/host", nil)
	if err != nil {
		t.Fatal(err)
	}
	defer hostConn.CloseNow()
	_, text, _ := hostConn.Read(ctx)
	var challenge struct{ Nonce string }
	json.Unmarshal(text, &challenge)
	public, private, _ := ed25519.GenerateKey(rand.Reader)
	signature := ed25519.Sign(private, append([]byte("lynshen-relay-v1:"), unb64(t, challenge.Nonce)...))
	auth, _ := json.Marshal(map[string]string{"t": "auth", "pub": b64.EncodeToString(public), "sig": b64.EncodeToString(signature), "v": "test"})
	hostConn.Write(ctx, websocket.MessageText, auth)
	_, text, _ = hostConn.Read(ctx)
	var ready struct{ T, Host string }
	json.Unmarshal(text, &ready)
	if ready.T != "ready" || ready.Host != hostID(public) {
		t.Fatalf("ready %s", text)
	}

	client, _, err := websocket.Dial(ctx, ws+"/relay/v1/connect?host="+ready.Host, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer client.CloseNow()
	_, open, _ := hostConn.Read(ctx)
	if open[0] != kindOpen {
		t.Fatalf("open frame %v", open)
	}
	sid := binary.BigEndian.Uint32(open[1:5])
	client.Write(ctx, websocket.MessageBinary, []byte("to host"))
	_, data, _ := hostConn.Read(ctx)
	if data[0] != kindData || binary.BigEndian.Uint32(data[1:5]) != sid || string(data[5:]) != "to host" {
		t.Fatalf("data frame %v", data)
	}
	reply := append([]byte{kindData, 0, 0, 0, 0}, "to client"...)
	binary.BigEndian.PutUint32(reply[1:5], sid)
	hostConn.Write(ctx, websocket.MessageBinary, reply)
	if _, got, _ := client.Read(ctx); string(got) != "to client" {
		t.Fatalf("client got %q", got)
	}

	// A client naming no connected host is told so with 4404.
	gone, _, _ := websocket.Dial(ctx, ws+"/relay/v1/connect?host=nobody", nil)
	_, _, err = gone.Read(ctx)
	if websocket.CloseStatus(err) != 4404 {
		t.Fatalf("offline host: %v", err)
	}
}

func TestABadHostSignatureIsRefused(t *testing.T) {
	server := httptest.NewServer(routes(newRelay(), nil))
	defer server.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	conn, _, err := websocket.Dial(ctx, "ws"+strings.TrimPrefix(server.URL, "http")+"/relay/v1/host", nil)
	if err != nil {
		t.Fatal(err)
	}
	conn.Read(ctx)
	public, _, _ := ed25519.GenerateKey(rand.Reader)
	auth, _ := json.Marshal(map[string]string{"t": "auth", "pub": b64.EncodeToString(public), "sig": b64.EncodeToString(make([]byte, 64))})
	conn.Write(ctx, websocket.MessageText, auth)
	if _, _, err := conn.Read(ctx); websocket.CloseStatus(err) != 4401 {
		t.Fatalf("got %v", err)
	}
}
