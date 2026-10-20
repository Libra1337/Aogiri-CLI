// Web Push for paired phones (docs/relay-protocol.md, "Web Push and native push").
// A daemon connected to this relay asks it to deliver one notification to a
// browser subscription it holds; the relay signs the request for the push
// service with its VAPID key (RFC 8292) and passes the message on. A daemon
// from 0.7.6 on encrypts the message for the browser itself (RFC 8291), so
// the relay forwards ciphertext it cannot read; an older one sends the
// plaintext `payload`, which the relay encrypts and forgets. The relay keeps
// no subscriptions and stores nothing it passes on.
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
	"crypto/tls"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net/http"
	"net/url"
	"os"
	"strings"
	"sync"
	"time"
)

const (
	pushContext       = "lynshen-relay-push-v1:"
	pushPerHostMinute = 60
	pushPerIPMinute   = 120
	pushClockSkew     = 5 * time.Minute
	maxPushBody       = 16 << 10
	// A plaintext payload from an older daemon; the encrypted message of a
	// newer one. 4096 is the most a push service must take (RFC 8030 7.2).
	maxPlaintext = 3 << 10
	maxEncrypted = 4096
	recordSize   = 4096
	jwtLifetime  = 12 * time.Hour
	pushTimeout  = 15 * time.Second
	defaultTTL   = 24 * 60 * 60
)

// pushHosts are the browser push services a subscription may name: the
// relay never posts anywhere else.
var pushHosts = []string{
	"fcm.googleapis.com",
	"android.googleapis.com",
	"updates.push.services.mozilla.com",
	".push.apple.com",
	".notify.windows.com",
}

func allowedEndpoint(endpoint string) (*url.URL, bool) {
	u, err := url.Parse(endpoint)
	if err != nil || u.Scheme != "https" || u.User != nil || u.Port() != "" || u.Host == "" {
		return nil, false
	}
	host := strings.ToLower(u.Hostname())
	for _, allowed := range pushHosts {
		if strings.HasPrefix(allowed, ".") {
			if strings.HasSuffix(host, allowed) && len(host) > len(allowed) {
				return u, true
			}
		} else if host == allowed {
			return u, true
		}
	}
	return nil, false
}

// vapid is the relay's application server key (RFC 8292).
type vapid struct {
	key     *ecdsa.PrivateKey
	public  []byte // uncompressed P-256 point, 65 bytes
	subject string

	mu   sync.Mutex
	jwts map[string]cachedJWT
}

type cachedJWT struct {
	token   string
	expires time.Time
}

type vapidFile struct {
	// The private scalar, 32 bytes, base64url.
	Private string `json:"private"`
	Public  string `json:"public"`
}

// generateVAPID writes a new key file at path (0600) and refuses to
// replace one: the browsers' subscriptions are bound to the key.
func generateVAPID(path string) error {
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		return err
	}
	private, err := key.Bytes()
	if err != nil {
		return err
	}
	public, err := key.PublicKey.Bytes()
	if err != nil {
		return err
	}
	text, _ := json.MarshalIndent(vapidFile{Private: b64.EncodeToString(private), Public: b64.EncodeToString(public)}, "", "  ")
	file, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		return err
	}
	if _, err := file.Write(append(text, '\n')); err != nil {
		file.Close()
		return err
	}
	return file.Close()
}

func loadVAPID(path, subject string) (*vapid, error) {
	text, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var file vapidFile
	if err := json.Unmarshal(text, &file); err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	private, err := b64.DecodeString(file.Private)
	if err != nil {
		return nil, fmt.Errorf("%s: private: %w", path, err)
	}
	key, err := ecdsa.ParseRawPrivateKey(elliptic.P256(), private)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	public, err := key.PublicKey.Bytes()
	if err != nil {
		return nil, err
	}
	if file.Public != "" && file.Public != b64.EncodeToString(public) {
		return nil, fmt.Errorf("%s: the public key does not match the private key", path)
	}
	return &vapid{key: key, public: public, subject: subject, jwts: map[string]cachedJWT{}}, nil
}

// header is the Authorization value for a push service at `audience`
// (scheme://host), from a token reused until an hour before it expires.
func (v *vapid) header(audience string, now time.Time) (string, error) {
	v.mu.Lock()
	defer v.mu.Unlock()
	if cached, ok := v.jwts[audience]; ok && now.Add(time.Hour).Before(cached.expires) {
		return cached.token, nil
	}
	expires := now.Add(jwtLifetime)
	head := b64.EncodeToString([]byte(`{"typ":"JWT","alg":"ES256"}`))
	claims, _ := json.Marshal(map[string]any{"aud": audience, "exp": expires.Unix(), "sub": v.subject})
	signing := head + "." + b64.EncodeToString(claims)
	digest := sha256.Sum256([]byte(signing))
	r, s, err := ecdsa.Sign(rand.Reader, v.key, digest[:])
	if err != nil {
		return "", err
	}
	// JWS ES256: r || s, 32 bytes each (RFC 7518 3.4), not DER.
	signature := make([]byte, 64)
	r.FillBytes(signature[:32])
	s.FillBytes(signature[32:])
	token := "vapid t=" + signing + "." + b64.EncodeToString(signature) + ", k=" + b64.EncodeToString(v.public)
	if len(v.jwts) > 64 {
		v.jwts = map[string]cachedJWT{}
	}
	v.jwts[audience] = cachedJWT{token: token, expires: expires}
	return token, nil
}

// encryptMessage encrypts `plaintext` for a browser subscription
// (`aes128gcm`, RFC 8291 + RFC 8188, one record). `asKey` and `salt` are
// random except in tests.
func encryptMessage(plaintext, uaPublic, authSecret []byte, asKey *ecdh.PrivateKey, salt []byte) ([]byte, error) {
	if len(authSecret) != 16 {
		return nil, errors.New("auth secret must be 16 bytes")
	}
	if len(salt) != 16 {
		return nil, errors.New("salt must be 16 bytes")
	}
	// NewPublicKey checks that the point is on the curve (RFC 8291 7).
	ua, err := ecdh.P256().NewPublicKey(uaPublic)
	if err != nil {
		return nil, fmt.Errorf("p256dh: %w", err)
	}
	if asKey == nil {
		if asKey, err = ecdh.P256().GenerateKey(rand.Reader); err != nil {
			return nil, err
		}
	}
	shared, err := asKey.ECDH(ua)
	if err != nil {
		return nil, err
	}
	asPublic := asKey.PublicKey().Bytes()
	keyInfo := append(append([]byte("WebPush: info\x00"), uaPublic...), asPublic...)
	ikm, err := hkdf.Key(sha256.New, shared, authSecret, string(keyInfo), 32)
	if err != nil {
		return nil, err
	}
	cek, err := hkdf.Key(sha256.New, ikm, salt, "Content-Encoding: aes128gcm\x00", 16)
	if err != nil {
		return nil, err
	}
	nonce, err := hkdf.Key(sha256.New, ikm, salt, "Content-Encoding: nonce\x00", 12)
	if err != nil {
		return nil, err
	}
	block, err := aes.NewCipher(cek)
	if err != nil {
		return nil, err
	}
	gcm, err := cipher.NewGCM(block)
	if err != nil {
		return nil, err
	}
	// The last (only) record: the plaintext, then the delimiter 0x02.
	record := append(append([]byte{}, plaintext...), 2)
	if len(record)+16 > recordSize {
		return nil, errors.New("message too large")
	}
	header := make([]byte, 0, 86)
	header = append(header, salt...)
	header = binary.BigEndian.AppendUint32(header, recordSize)
	header = append(header, byte(len(asPublic)))
	header = append(header, asPublic...)
	return gcm.Seal(header, nonce, record, nil), nil
}

type pushSubscription struct {
	Endpoint string `json:"endpoint"`
	Keys     struct {
		P256dh string `json:"p256dh"`
		Auth   string `json:"auth"`
	} `json:"keys"`
	// Native push (native.go): apns, fcm or getui, with the device token
	// (Getui: client_id) and, for APNs, its environment.
	Provider    string `json:"provider"`
	ClientID    string `json:"client_id"`
	Token       string `json:"token"`
	Environment string `json:"environment"`
}

type pushRequest struct {
	Pub          string           `json:"pub"`
	TS           int64            `json:"ts"`
	Subscription pushSubscription `json:"subscription"`
	// An older daemon's plaintext notification, encrypted here.
	Payload json.RawMessage `json:"payload"`
	// A newer daemon's message, already encrypted for the browser.
	Encrypted string `json:"encrypted"`
	Urgency   string `json:"urgency"`
	TTL       *int   `json:"ttl"`
	Topic     string `json:"topic"`
	// Makes each request's signature unique; not used otherwise.
	Nonce string `json:"nonce"`
}

// pusher serves POST /relay/v1/push and GET /relay/v1/push/key.
type pusher struct {
	relay *relay
	// nil: Web Push is not served (501, and no /push/key).
	vapid *vapid
	// The native providers configured (native.go); the others answer 501.
	natives natives
	client  *http.Client
	now     func() time.Time
	// Which endpoints may be posted to (allowedEndpoint; tests widen it).
	allow func(endpoint string) (*url.URL, bool)

	// Signatures seen within the clock window: a replayed request is refused.
	seenMu sync.Mutex
	seen   map[[32]byte]time.Time
}

func newPusher(rl *relay, v *vapid, n natives) *pusher {
	if n == nil {
		n = natives{}
	}
	return &pusher{
		relay:   rl,
		vapid:   v,
		natives: n,
		client: &http.Client{
			Timeout: pushTimeout,
			// A push service answers; it never sends us elsewhere.
			CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse },
		},
		now:   time.Now,
		allow: allowedEndpoint,
		seen:  map[[32]byte]time.Time{},
	}
}

func (p *pusher) serveKey(w http.ResponseWriter, r *http.Request) {
	if p.vapid == nil {
		http.NotFound(w, r)
		return
	}
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	w.Header().Set("Content-Type", "application/json")
	w.Header().Set("Cache-Control", "no-cache")
	json.NewEncoder(w).Encode(map[string]string{"key": b64.EncodeToString(p.vapid.public)})
}

// fresh remembers a signature; false when it was already used.
func (p *pusher) fresh(signature []byte, now time.Time) bool {
	sum := sha256.Sum256(signature)
	p.seenMu.Lock()
	defer p.seenMu.Unlock()
	for key, at := range p.seen {
		if now.Sub(at) > 2*pushClockSkew {
			delete(p.seen, key)
		}
	}
	if _, used := p.seen[sum]; used {
		return false
	}
	p.seen[sum] = now
	return true
}

func (p *pusher) servePush(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	if !p.relay.rate.allow("push-ip:"+clientIP(r), pushPerIPMinute) {
		http.Error(w, "too many notifications", http.StatusTooManyRequests)
		return
	}
	body, err := io.ReadAll(io.LimitReader(r.Body, maxPushBody+1))
	if err != nil || len(body) > maxPushBody {
		http.Error(w, "bad request", http.StatusBadRequest)
		return
	}
	var req pushRequest
	if json.Unmarshal(body, &req) != nil {
		http.Error(w, "bad request", http.StatusBadRequest)
		return
	}
	public, errPub := b64.DecodeString(req.Pub)
	signature, errSig := b64.DecodeString(r.Header.Get("X-LynShen-Signature"))
	if errPub != nil || errSig != nil || len(public) != ed25519.PublicKeySize ||
		!ed25519.Verify(public, append([]byte(pushContext), body...), signature) {
		http.Error(w, "bad signature", http.StatusUnauthorized)
		return
	}
	now := p.now()
	if d := now.Sub(time.UnixMilli(req.TS)); d > pushClockSkew || d < -pushClockSkew {
		http.Error(w, "stale request", http.StatusUnauthorized)
		return
	}
	id := hostID(public)
	p.relay.mu.Lock()
	connected := p.relay.hosts[id] != nil
	p.relay.mu.Unlock()
	if !connected {
		http.Error(w, "host not connected", http.StatusForbidden)
		return
	}
	if !p.fresh(signature, now) {
		http.Error(w, "replayed request", http.StatusUnauthorized)
		return
	}
	if !p.relay.rate.allow("push:"+id, pushPerHostMinute) {
		http.Error(w, "too many notifications", http.StatusTooManyRequests)
		return
	}
	sub := req.Subscription
	if sub.Provider != "" {
		p.serveNative(w, r, id, req, now)
		return
	}
	if p.vapid == nil {
		http.Error(w, "web push is not configured", http.StatusNotImplemented)
		return
	}
	endpoint, ok := p.allow(sub.Endpoint)
	if !ok {
		http.Error(w, "not a browser push service", http.StatusForbidden)
		return
	}
	var message []byte
	switch {
	case req.Encrypted != "":
		message, err = b64.DecodeString(req.Encrypted)
		if err != nil || len(message) < 86+17 || len(message) > maxEncrypted {
			http.Error(w, "bad encrypted message", http.StatusBadRequest)
			return
		}
	case len(req.Payload) > 0:
		if len(req.Payload) > maxPlaintext || !json.Valid(req.Payload) {
			http.Error(w, "bad payload", http.StatusBadRequest)
			return
		}
		uaPublic, errKey := b64.DecodeString(sub.Keys.P256dh)
		auth, errAuth := b64.DecodeString(sub.Keys.Auth)
		salt := make([]byte, 16)
		rand.Read(salt)
		if errKey == nil && errAuth == nil {
			message, err = encryptMessage(req.Payload, uaPublic, auth, nil, salt)
		}
		if errKey != nil || errAuth != nil || err != nil {
			http.Error(w, "bad subscription keys", http.StatusBadRequest)
			return
		}
	default:
		http.Error(w, "nothing to send", http.StatusBadRequest)
		return
	}
	status, err := p.deliver(r.Context(), endpoint, message, req, now)
	if err != nil {
		log.Printf("push for host %s via %s failed: %v", id, endpoint.Hostname(), err)
		http.Error(w, "push service unreachable", http.StatusBadGateway)
		return
	}
	log.Printf("push for host %s via %s: %d", id, endpoint.Hostname(), status)
	switch {
	case status >= 200 && status < 300:
		w.WriteHeader(http.StatusNoContent)
	case status == http.StatusNotFound || status == http.StatusGone:
		http.Error(w, "subscription gone", http.StatusGone)
	case status == http.StatusTooManyRequests:
		http.Error(w, "push service busy", http.StatusTooManyRequests)
	case status == http.StatusRequestEntityTooLarge:
		http.Error(w, "message too large", http.StatusRequestEntityTooLarge)
	default:
		http.Error(w, fmt.Sprintf("push service answered %d", status), http.StatusBadGateway)
	}
}

func (p *pusher) deliver(ctx context.Context, endpoint *url.URL, message []byte, req pushRequest, now time.Time) (int, error) {
	authorization, err := p.vapid.header(endpoint.Scheme+"://"+endpoint.Host, now)
	if err != nil {
		return 0, err
	}
	ctx, cancel := context.WithTimeout(ctx, pushTimeout)
	defer cancel()
	out, err := http.NewRequestWithContext(ctx, http.MethodPost, endpoint.String(), bytes.NewReader(message))
	if err != nil {
		return 0, err
	}
	ttl := defaultTTL
	if req.TTL != nil && *req.TTL >= 0 && *req.TTL <= 28*24*60*60 {
		ttl = *req.TTL
	}
	out.Header.Set("Authorization", authorization)
	out.Header.Set("Content-Encoding", "aes128gcm")
	out.Header.Set("Content-Type", "application/octet-stream")
	out.Header.Set("TTL", fmt.Sprint(ttl))
	switch req.Urgency {
	case "very-low", "low", "normal", "high":
		out.Header.Set("Urgency", req.Urgency)
	}
	// RFC 8030 5.4: at most 32 base64url characters.
	if topic := req.Topic; topic != "" && len(topic) <= 32 && validTopic(topic) {
		out.Header.Set("Topic", topic)
	}
	res, err := p.client.Do(out)
	if err != nil {
		return 0, err
	}
	io.Copy(io.Discard, io.LimitReader(res.Body, 4096))
	res.Body.Close()
	return res.StatusCode, nil
}

// serveNative delivers a daemon's notice through APNs, FCM (or, later,
// Getui): a plaintext notice of generic text and ids, never `encrypted`.
func (p *pusher) serveNative(w http.ResponseWriter, r *http.Request, id string, req pushRequest, now time.Time) {
	provider := req.Subscription.Provider
	sender, configured := p.natives[provider]
	switch provider {
	case "apns", "fcm", "getui":
	default:
		http.Error(w, "unknown provider", http.StatusBadRequest)
		return
	}
	if !configured {
		http.Error(w, provider+" is not configured", http.StatusNotImplemented)
		return
	}
	target, ok := nativeTargetOf(req.Subscription)
	if !ok {
		http.Error(w, "bad device token", http.StatusBadRequest)
		return
	}
	if req.Encrypted != "" {
		http.Error(w, "a native notice is not encrypted", http.StatusBadRequest)
		return
	}
	n, err := parseNotice(req.Payload)
	if err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}
	ttl := defaultTTL
	if req.TTL != nil && *req.TTL >= 0 && *req.TTL <= 28*24*60*60 {
		ttl = *req.TTL
	}
	topic := ""
	if req.Topic != "" && len(req.Topic) <= 32 && validTopic(req.Topic) {
		topic = req.Topic
	}
	ctx, cancel := context.WithTimeout(r.Context(), pushTimeout)
	defer cancel()
	result, err := sender.send(ctx, target, n, nativeDelivery{ttl: ttl, topic: topic, now: now})
	if err != nil {
		log.Printf("push for host %s via %s failed: %v", id, provider, err)
		http.Error(w, "push service unreachable", http.StatusBadGateway)
		return
	}
	log.Printf("push for host %s via %s: %d %s", id, provider, result.Status, result.Reason)
	status := result.relayStatus(provider)
	if status == http.StatusNoContent {
		w.WriteHeader(status)
		return
	}
	message := fmt.Sprintf("%s answered %d", provider, result.Status)
	if result.Reason != "" {
		message += " " + result.Reason
	}
	http.Error(w, message, status)
}

func validTopic(topic string) bool {
	for _, c := range topic {
		if !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || c == '-' || c == '_') {
			return false
		}
	}
	return true
}

// allowTestOrigin lets a local test post to one more push service (its
// self-signed certificate trusted). Never set in production.
func allowTestOrigin(p *pusher, origin string) {
	test, err := url.Parse(origin)
	if err != nil || test.Host == "" {
		log.Fatalf("-test-push-origin %q: not an origin", origin)
	}
	log.Printf("TEST ONLY: push to %s allowed", test.Host)
	p.client.Transport = &http.Transport{TLSClientConfig: &tls.Config{InsecureSkipVerify: true}}
	p.allow = func(endpoint string) (*url.URL, bool) {
		if u, err := url.Parse(endpoint); err == nil && u.Scheme == "https" && u.Host == test.Host {
			return u, true
		}
		return allowedEndpoint(endpoint)
	}
}
