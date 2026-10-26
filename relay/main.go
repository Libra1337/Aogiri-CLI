// lynshen-relay forwards opaque bytes between paired clients and a daemon
// (docs/relay-protocol.md §3, and §7 for v2, account-based, in v2.go). It
// holds no keys, sees no content and stores nothing: every client stream is a
// Noise session the relay cannot read.
package main

import (
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"log"
	"net"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/coder/websocket"
)

const (
	kindOpen  = 1
	kindData  = 2
	kindClose = 3

	maxMessage        = 1 << 20
	streamsPerHost    = 32
	clientsPerMinute  = 20
	hostAuthPerMinute = 10
	handshakeTimeout  = 10 * time.Second
	idleStream        = 10 * time.Minute
	pingEvery         = 30 * time.Second
)

var b64 = base64.RawURLEncoding

type stream struct {
	conn     *websocket.Conn
	lastSeen time.Time
	cancel   context.CancelFunc
	// v2 (v2.go): the caller the token named; empty for a v1 client.
	user, authz string
}

type host struct {
	id      string
	conn    *websocket.Conn
	writeMu sync.Mutex
	mu      sync.Mutex
	streams map[uint32]*stream
	nextID  uint32
	done    chan struct{}
	// v2 (v2.go): the account the host is registered under and the device
	// authorization its token named; empty for a v1 host.
	user, authz string
}

func (h *host) v2() bool { return h.user != "" }

func (h *host) send(ctx context.Context, kind byte, sid uint32, payload []byte) error {
	frame := make([]byte, 5+len(payload))
	frame[0] = kind
	binary.BigEndian.PutUint32(frame[1:5], sid)
	copy(frame[5:], payload)
	h.writeMu.Lock()
	defer h.writeMu.Unlock()
	return h.conn.Write(ctx, websocket.MessageBinary, frame)
}

type relay struct {
	mu    sync.Mutex
	hosts map[string]*host
	rate  *limiter
	// accounts serves v2 (v2.go); nil: only v1 is served.
	accounts *accounts
	// v1 serves the v1 endpoints (default on, while old phones need them).
	v1 bool
	// offline lists v2 hosts gone since the last heartbeat.
	offline []string
	// draining: the server is going down; new connections are refused 4503.
	draining bool
}

// limiter counts events per key in a sliding one-minute window.
type limiter struct {
	mu     sync.Mutex
	events map[string][]time.Time
}

func (l *limiter) allow(key string, perMinute int) bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	now := time.Now()
	kept := l.events[key][:0]
	for _, t := range l.events[key] {
		if now.Sub(t) < time.Minute {
			kept = append(kept, t)
		}
	}
	if len(kept) >= perMinute {
		l.events[key] = kept
		return false
	}
	l.events[key] = append(kept, now)
	return true
}

func clientIP(r *http.Request) string {
	// Caddy terminates TLS on the same machine; the relay listens on loopback.
	if forwarded := r.Header.Get("X-Forwarded-For"); forwarded != "" {
		return strings.TrimSpace(strings.Split(forwarded, ",")[0])
	}
	ip, _, _ := net.SplitHostPort(r.RemoteAddr)
	return ip
}

// hostID is base64url(SHA-256(public key)[:16]), as the daemon derives it.
func hostID(public []byte) string {
	sum := sha256.Sum256(public)
	return b64.EncodeToString(sum[:16])
}

func (rl *relay) serveHost(w http.ResponseWriter, r *http.Request) {
	if !rl.rate.allow("auth:"+clientIP(r), hostAuthPerMinute) {
		http.Error(w, "too many attempts", http.StatusTooManyRequests)
		return
	}
	conn, err := websocket.Accept(w, r, nil)
	if err != nil {
		return
	}
	conn.SetReadLimit(maxMessage + 5)
	ctx := r.Context()
	auth, ok := hostChallenge(ctx, conn)
	if !ok {
		return
	}
	h := &host{id: hostID(auth.public), conn: conn, streams: map[uint32]*stream{}, done: make(chan struct{})}
	ready, _ := json.Marshal(map[string]string{"t": "ready", "host": h.id})
	if err := conn.Write(ctx, websocket.MessageText, ready); err != nil {
		return
	}
	rl.runHost(h, auth.V)
}

// hostAuth is a host's answer to the challenge (§3.1; v2 adds static, name
// and platform).
type hostAuth struct {
	T        string `json:"t"`
	Pub      string `json:"pub"`
	Sig      string `json:"sig"`
	V        string `json:"v"`
	Static   string `json:"static"`
	Name     string `json:"name"`
	Platform string `json:"platform"`
	public   []byte
}

// hostChallenge sends the challenge and checks the signed answer; a bad one
// closes the connection with 4401.
func hostChallenge(ctx context.Context, conn *websocket.Conn) (*hostAuth, bool) {
	nonce := make([]byte, 32)
	if _, err := rand.Read(nonce); err != nil {
		conn.Close(websocket.StatusInternalError, "rng")
		return nil, false
	}
	challenge, _ := json.Marshal(map[string]string{"t": "challenge", "nonce": b64.EncodeToString(nonce)})
	authCtx, cancel := context.WithTimeout(ctx, handshakeTimeout)
	defer cancel()
	if err := conn.Write(authCtx, websocket.MessageText, challenge); err != nil {
		return nil, false
	}
	kind, body, err := conn.Read(authCtx)
	if err != nil || kind != websocket.MessageText {
		conn.Close(4401, "auth failed")
		return nil, false
	}
	var auth hostAuth
	if json.Unmarshal(body, &auth) != nil {
		conn.Close(4401, "auth failed")
		return nil, false
	}
	public, errPub := b64.DecodeString(auth.Pub)
	signature, errSig := b64.DecodeString(auth.Sig)
	message := append([]byte("lynshen-relay-v1:"), nonce...)
	if auth.T != "auth" || errPub != nil || errSig != nil || len(public) != ed25519.PublicKeySize ||
		!ed25519.Verify(public, message, signature) {
		conn.Close(4401, "auth failed")
		return nil, false
	}
	auth.public = public
	return &auth, true
}

// runHost serves an authenticated host (v1 or v2) until its socket drops: it
// replaces an older connection of the same host, pings it, and routes its
// frames to the client streams.
func (rl *relay) runHost(h *host, version string) {
	conn := h.conn
	rl.mu.Lock()
	previous := rl.hosts[h.id]
	rl.hosts[h.id] = h
	rl.mu.Unlock()
	if previous != nil {
		previous.conn.Close(4409, "replaced")
	}
	log.Printf("host %s connected (daemon %s)", h.id, version)

	go func() {
		ticker := time.NewTicker(pingEvery)
		defer ticker.Stop()
		for {
			select {
			case <-h.done:
				return
			case <-ticker.C:
				pingCtx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
				err := conn.Ping(pingCtx)
				cancel()
				if err != nil {
					conn.Close(websocket.StatusGoingAway, "ping timeout")
					return
				}
			}
		}
	}()

	defer func() {
		close(h.done)
		rl.mu.Lock()
		if rl.hosts[h.id] == h {
			delete(rl.hosts, h.id)
			if h.v2() {
				rl.offline = append(rl.offline, h.id)
			}
		}
		draining := rl.draining
		rl.mu.Unlock()
		code, reason := websocket.StatusCode(4404), "host offline"
		if draining && h.v2() {
			code, reason = 4503, "server restarting"
		}
		h.mu.Lock()
		streams := h.streams
		h.streams = map[uint32]*stream{}
		h.mu.Unlock()
		// Each close waits for its client's answer: one at a time, a slow
		// client would hold up the others.
		for _, s := range streams {
			go func() {
				s.conn.Close(code, reason)
				s.cancel()
			}()
		}
		log.Printf("host %s disconnected", h.id)
	}()

	for {
		kind, frame, err := conn.Read(context.Background())
		if err != nil {
			return
		}
		if kind != websocket.MessageBinary || len(frame) < 5 {
			continue
		}
		sid := binary.BigEndian.Uint32(frame[1:5])
		h.mu.Lock()
		s := h.streams[sid]
		h.mu.Unlock()
		if s == nil {
			continue
		}
		switch frame[0] {
		case kindData:
			s.lastSeen = time.Now()
			writeCtx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
			err := s.conn.Write(writeCtx, websocket.MessageBinary, frame[5:])
			cancel()
			if err != nil {
				s.cancel()
			}
		case kindClose:
			s.conn.Close(4410, "host closed")
			s.cancel()
		}
	}
}

func (rl *relay) serveConnect(w http.ResponseWriter, r *http.Request) {
	if !rl.rate.allow("connect:"+clientIP(r), clientsPerMinute) {
		conn, err := websocket.Accept(w, r, nil)
		if err == nil {
			conn.Close(4429, "too many connections")
		}
		return
	}
	conn, err := websocket.Accept(w, r, &websocket.AcceptOptions{InsecureSkipVerify: true})
	if err != nil {
		return
	}
	conn.SetReadLimit(maxMessage)
	rl.mu.Lock()
	h := rl.hosts[r.URL.Query().Get("host")]
	rl.mu.Unlock()
	if h == nil {
		conn.Close(4404, "host offline")
		return
	}
	// A v1 client of a v2 host (an Aogiri release from before v2): the open
	// frame says so, and the daemon admits only devices paired with it.
	var payload []byte
	if h.v2() {
		payload = []byte(`{"v":1}`)
	}
	rl.serveStream(h, conn, &stream{}, payload)
}

// serveStream forwards one client's binary messages to its host and back
// until either side closes (§3.2). `s` carries the caller (v2).
func (rl *relay) serveStream(h *host, conn *websocket.Conn, s *stream, payload []byte) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	h.mu.Lock()
	if len(h.streams) >= streamsPerHost {
		h.mu.Unlock()
		conn.Close(4429, "too many streams")
		return
	}
	h.nextID++
	if h.nextID == 0 {
		h.nextID = 1
	}
	sid := h.nextID
	s.conn, s.lastSeen, s.cancel = conn, time.Now(), cancel
	h.streams[sid] = s
	h.mu.Unlock()
	defer func() {
		h.mu.Lock()
		_, live := h.streams[sid]
		delete(h.streams, sid)
		h.mu.Unlock()
		if live {
			closeCtx, done := context.WithTimeout(context.Background(), 5*time.Second)
			h.send(closeCtx, kindClose, sid, nil)
			done()
		}
		conn.Close(websocket.StatusNormalClosure, "")
	}()
	if err := h.send(ctx, kindOpen, sid, payload); err != nil {
		return
	}

	go func() {
		ticker := time.NewTicker(time.Minute)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				if time.Since(s.lastSeen) > idleStream {
					conn.Close(4408, "idle")
					cancel()
					return
				}
			}
		}
	}()

	for {
		kind, payload, err := conn.Read(ctx)
		if err != nil {
			return
		}
		if kind != websocket.MessageBinary {
			conn.Close(4400, "binary frames only")
			return
		}
		s.lastSeen = time.Now()
		if err := h.send(ctx, kindData, sid, payload); err != nil {
			return
		}
	}
}

func newRelay() *relay {
	return &relay{hosts: map[string]*host{}, rate: &limiter{events: map[string][]time.Time{}}, v1: true}
}

// routes are the relay's endpoints; push ones only with a VAPID key or a
// native provider.
func routes(rl *relay, push *pusher) *http.ServeMux {
	mux := http.NewServeMux()
	if rl.v1 {
		mux.HandleFunc("/relay/v1/host", rl.serveHost)
		mux.HandleFunc("/relay/v1/connect", rl.serveConnect)
	}
	mux.HandleFunc("/relay/v1/healthz", func(w http.ResponseWriter, _ *http.Request) {
		w.Write([]byte("ok"))
	})
	if rl.accounts != nil {
		mux.HandleFunc("/relay/v2/host", rl.serveHostV2)
		mux.HandleFunc("/relay/v2/connect", rl.serveConnectV2)
		mux.HandleFunc("/relay/v2/healthz", func(w http.ResponseWriter, _ *http.Request) {
			w.Write([]byte("ok"))
		})
	}
	if push != nil {
		mux.HandleFunc("/relay/v1/push/key", push.serveKey)
		mux.HandleFunc("/relay/v1/push", push.servePush)
	}
	return mux
}

// credentialPath is the flag's path, else the credential `name` systemd
// passes in (LoadCredential=name:…), else none.
func credentialPath(flagValue, name string) string {
	if flagValue != "" {
		return flagValue
	}
	if dir := os.Getenv("CREDENTIALS_DIRECTORY"); dir != "" {
		path := filepath.Join(dir, name)
		if _, err := os.Stat(path); err == nil {
			return path
		}
	}
	return ""
}

func main() {
	listen := flag.String("listen", "127.0.0.1:18095", "address to listen on")
	vapidFlag := flag.String("vapid-file", "", "VAPID key file (default: the systemd credential `vapid`); Web Push is served only with one")
	subject := flag.String("vapid-subject", "https://app.lynshen.org", "contact the push services see in the VAPID token (mailto: or https:)")
	generate := flag.String("gen-vapid", "", "write a new VAPID key file at this path (0600, never replaced) and exit")
	testPush := flag.String("test-push-origin", "", "for local tests only: also post to this push service origin (https://host:port), trusting its certificate")
	apnsKey := flag.String("apns-key", "", "APNs auth key (.p8) (default: the systemd credential `apns`); APNs is served only with one")
	apnsKeyID := flag.String("apns-key-id", os.Getenv("LYNSHEN_RELAY_APNS_KEY_ID"), "the APNs key's Key ID (default $LYNSHEN_RELAY_APNS_KEY_ID)")
	apnsTeamID := flag.String("apns-team-id", os.Getenv("LYNSHEN_RELAY_APNS_TEAM_ID"), "the Apple Team ID (default $LYNSHEN_RELAY_APNS_TEAM_ID)")
	apnsTopic := flag.String("apns-topic", "com.lynshen.aogiri", "the app's bundle id (apns-topic)")
	fcmAccount := flag.String("fcm-account", "", "FCM service account key JSON (default: the systemd credential `fcm`); FCM is served only with one")
	gateway := flag.String("gateway", os.Getenv("LYNSHEN_RELAY_GATEWAY"), "LynShen gateway origin for relay v2 (e.g. https://www.lynshen.org; default $LYNSHEN_RELAY_GATEWAY); v2 is served only with one")
	serviceFile := flag.String("service-token-file", "", "the gateway's relay service token (default: the systemd credential `relay-service`, else $LYNSHEN_RELAY_SERVICE_TOKEN)")
	relayKeys := flag.String("relay-keys", os.Getenv("LYNSHEN_RELAY_TOKEN_KEYS"), "comma-separated base64url Ed25519 keys that sign relay tokens, used until the gateway's relay-key answers (default $LYNSHEN_RELAY_TOKEN_KEYS)")
	noV1 := flag.Bool("no-v1", false, "stop serving the v1 host and connect endpoints (keep them while phones from before v2 are in use)")
	flag.Parse()
	if *generate != "" {
		if err := generateVAPID(*generate); err != nil {
			log.Fatal(err)
		}
		v, err := loadVAPID(*generate, *subject)
		if err != nil {
			log.Fatal(err)
		}
		fmt.Println(b64.EncodeToString(v.public))
		return
	}
	rl := newRelay()
	rl.v1 = !*noV1
	if *gateway != "" {
		service, err := readSecret(*serviceFile, "relay-service", "LYNSHEN_RELAY_SERVICE_TOKEN")
		if err != nil {
			log.Fatal(err)
		}
		if len(service) < 32 {
			log.Fatal("relay v2 needs the gateway's relay service token (32 bytes or more)")
		}
		keys, err := parseKeys(*relayKeys)
		if err != nil {
			log.Fatal(err)
		}
		rl.accounts = newAccounts(*gateway, service, keys)
		ctx, cancel := context.WithTimeout(context.Background(), gatewayTimeout)
		if err := rl.accounts.refreshKeys(ctx); err != nil && len(keys) == 0 {
			log.Printf("relay v2: no token key yet (%v); retrying every %s", err, keysEvery)
		}
		cancel()
		go rl.runAccounts(context.Background())
		log.Printf("relay v2 on, gateway %s", *gateway)
	} else {
		log.Printf("relay v2 off: no gateway")
	}
	var v *vapid
	if path := credentialPath(*vapidFlag, "vapid"); path != "" {
		var err error
		if v, err = loadVAPID(path, *subject); err != nil {
			log.Fatal(err)
		}
		log.Printf("web push on, VAPID key %s", b64.EncodeToString(v.public))
	} else {
		log.Printf("web push off: no VAPID key")
	}
	n := natives{}
	if path := credentialPath(*apnsKey, "apns"); path != "" {
		a, err := loadAPNs(path, apnsConfig{keyID: *apnsKeyID, teamID: *apnsTeamID, topic: *apnsTopic})
		if err != nil {
			log.Fatal(err)
		}
		n["apns"] = a
		log.Printf("apns on, key %s, team %s, topic %s", *apnsKeyID, *apnsTeamID, *apnsTopic)
	} else {
		log.Printf("apns off: no key")
	}
	if path := credentialPath(*fcmAccount, "fcm"); path != "" {
		f, err := loadFCM(path)
		if err != nil {
			log.Fatal(err)
		}
		n["fcm"] = f
		log.Printf("fcm on, project %s", f.project)
	} else {
		log.Printf("fcm off: no service account")
	}
	var push *pusher
	if v != nil || len(n) > 0 {
		push = newPusher(rl, v, n)
		if *testPush != "" {
			allowTestOrigin(push, *testPush)
		}
	}
	server := &http.Server{Addr: *listen, Handler: routes(rl, push), ReadHeaderTimeout: 10 * time.Second}
	// SIGTERM (systemd stop or restart): tell v2 hosts and clients 4503, so
	// they come back in a second instead of backing off.
	stop := make(chan os.Signal, 1)
	signal.Notify(stop, syscall.SIGTERM, syscall.SIGINT)
	go func() {
		<-stop
		log.Printf("draining")
		rl.drain()
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		server.Shutdown(ctx)
	}()
	log.Printf("lynshen-relay listening on %s", *listen)
	if err := server.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
		log.Fatal(err)
	}
}
