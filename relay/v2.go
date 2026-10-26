// Relay protocol v2 (docs/relay-protocol.md §7): remote access that belongs to
// the LynShen account. Both ends present a short-lived relay token the
// gateway signed (Ed25519, verified here offline); a host registers with the
// gateway as one of the account's computers, and a client reaches only the
// computers of its own account. The v1 endpoints stay served next to v2, and
// both share one host table: a v1 client (an Aogiri release from before v2)
// still reaches its paired computer after that computer moved to v2.
package main

import (
	"bytes"
	"context"
	"crypto/ed25519"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"strings"
	"sync"
	"time"

	"github.com/coder/websocket"
)

const (
	tokenPrefix   = "lrt1"
	tokenAudience = "lynshen-relay"
	tokenContext  = "lynshen-relay-token-v1:"
	tokenSkew     = 60 * time.Second
	maxTokenBytes = 2048
	// Browsers cannot set headers on a WebSocket: they offer these
	// subprotocols, and the relay answers with the first.
	subprotocol       = "lynshen-relay"
	bearerSubprotocol = "lynshen-bearer."
	heartbeatEvery    = 60 * time.Second
	keysEvery         = 10 * time.Minute
	gatewayTimeout    = 10 * time.Second
)

// claims of a relay token (account-computers.spec.md AC-T3).
type claims struct {
	Aud    string `json:"aud"`
	Kid    string `json:"kid"`
	Sub    string `json:"sub"`
	Authz  string `json:"authz"`
	Client string `json:"client"`
	Name   string `json:"name"`
	Iat    int64  `json:"iat"`
	Exp    int64  `json:"exp"`
}

// computerClient: the clients that may connect as a host (AC-T2).
func computerClient(client string) bool {
	return client == "lynshen-desktop" || client == "lynshen-cli"
}

func keyID(public ed25519.PublicKey) string {
	sum := sha256.Sum256(public)
	return b64.EncodeToString(sum[:8])
}

// verifyToken applies AC-V1: prefix, signature by a known key, audience, and
// iat - 60 s <= now < exp.
func verifyToken(keys []ed25519.PublicKey, token string, now time.Time) (*claims, bool) {
	if len(token) > maxTokenBytes {
		return nil, false
	}
	parts := strings.Split(token, ".")
	if len(parts) != 3 || parts[0] != tokenPrefix {
		return nil, false
	}
	raw, err := b64.DecodeString(parts[1])
	if err != nil {
		return nil, false
	}
	signature, err := b64.DecodeString(parts[2])
	if err != nil || len(signature) != ed25519.SignatureSize {
		return nil, false
	}
	var c claims
	if json.Unmarshal(raw, &c) != nil {
		return nil, false
	}
	var key ed25519.PublicKey
	for _, k := range keys {
		if keyID(k) == c.Kid {
			key = k
			break
		}
	}
	if key == nil || !ed25519.Verify(key, []byte(tokenContext+parts[1]), signature) {
		return nil, false
	}
	unix := now.Unix()
	if c.Aud != tokenAudience || c.Sub == "" || c.Authz == "" ||
		unix < c.Iat-int64(tokenSkew/time.Second) || unix >= c.Exp {
		return nil, false
	}
	return &c, true
}

// accounts is the relay's side of the gateway: the token keys, computer
// registration and heartbeats (account-computers.spec.md §4).
type accounts struct {
	gateway string
	service string
	client  *http.Client
	now     func() time.Time

	mu   sync.RWMutex
	keys []ed25519.PublicKey
}

func newAccounts(gateway, service string, keys []ed25519.PublicKey) *accounts {
	return &accounts{
		gateway: strings.TrimRight(gateway, "/"),
		service: service,
		client:  &http.Client{Timeout: gatewayTimeout},
		now:     time.Now,
		keys:    keys,
	}
}

func (a *accounts) verify(token string) (*claims, bool) {
	a.mu.RLock()
	keys := a.keys
	a.mu.RUnlock()
	return verifyToken(keys, token, a.now())
}

// refreshKeys reads the gateway's token keys (GET relay-key). The keys given
// on the command line are kept when the gateway has none.
func (a *accounts) refreshKeys(ctx context.Context) error {
	req, _ := http.NewRequestWithContext(ctx, http.MethodGet, a.gateway+"/api/desktop/oauth/relay-key", nil)
	res, err := a.client.Do(req)
	if err != nil {
		return err
	}
	defer res.Body.Close()
	if res.StatusCode != http.StatusOK {
		return fmt.Errorf("relay-key answered %d", res.StatusCode)
	}
	var body struct {
		Keys []struct {
			Kid       string `json:"kid"`
			PublicKey string `json:"public_key"`
		} `json:"keys"`
	}
	if err := json.NewDecoder(io.LimitReader(res.Body, 64<<10)).Decode(&body); err != nil {
		return err
	}
	var keys []ed25519.PublicKey
	for _, k := range body.Keys {
		raw, err := b64.DecodeString(k.PublicKey)
		if err == nil && len(raw) == ed25519.PublicKeySize && keyID(raw) == k.Kid {
			keys = append(keys, ed25519.PublicKey(raw))
		}
	}
	if len(keys) == 0 {
		return errors.New("the gateway has no relay key")
	}
	a.mu.Lock()
	a.keys = keys
	a.mu.Unlock()
	return nil
}

// post sends one service request to the gateway and decodes its JSON answer.
func (a *accounts) post(ctx context.Context, path string, body any, out any) (int, error) {
	data, _ := json.Marshal(body)
	ctx, cancel := context.WithTimeout(ctx, gatewayTimeout)
	defer cancel()
	req, _ := http.NewRequestWithContext(ctx, http.MethodPost, a.gateway+path, bytes.NewReader(data))
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", "Bearer "+a.service)
	res, err := a.client.Do(req)
	if err != nil {
		return 0, err
	}
	defer res.Body.Close()
	if res.StatusCode != http.StatusOK {
		io.Copy(io.Discard, io.LimitReader(res.Body, 4096))
		return res.StatusCode, nil
	}
	return res.StatusCode, json.NewDecoder(io.LimitReader(res.Body, 4<<20)).Decode(out)
}

// register records the host as a computer of the token's account (AC-R1)
// and returns that account.
func (a *accounts) register(ctx context.Context, token, hostID string, auth *hostAuth) (string, int, error) {
	var reply struct {
		User string `json:"user"`
	}
	status, err := a.post(ctx, "/api/relay/v1/computers", map[string]string{
		"token": token, "host_id": hostID, "static_pub": auth.Static,
		"name": auth.Name, "platform": auth.Platform, "version": auth.V,
	}, &reply)
	if err == nil && status == http.StatusOK && reply.User == "" {
		err = errors.New("registration without a user")
	}
	return reply.User, status, err
}

// bearer is the token of an upgrade request: `Authorization: Bearer`, else
// the `lynshen-bearer.<token>` subprotocol next to `lynshen-relay`.
func bearer(r *http.Request) string {
	if value := r.Header.Get("Authorization"); strings.HasPrefix(value, "Bearer ") {
		return strings.TrimSpace(strings.TrimPrefix(value, "Bearer "))
	}
	var token string
	offered := false
	for _, header := range r.Header.Values("Sec-WebSocket-Protocol") {
		for _, protocol := range strings.Split(header, ",") {
			protocol = strings.TrimSpace(protocol)
			switch {
			case protocol == subprotocol:
				offered = true
			case strings.HasPrefix(protocol, bearerSubprotocol):
				token = strings.TrimPrefix(protocol, bearerSubprotocol)
			}
		}
	}
	if !offered {
		return ""
	}
	return token
}

// draining reports whether new connections must be told 4503.
func (rl *relay) isDraining() bool {
	rl.mu.Lock()
	defer rl.mu.Unlock()
	return rl.draining
}

// serveHostV2: GET /relay/v2/host?host=<host id> with a computer's token.
func (rl *relay) serveHostV2(w http.ResponseWriter, r *http.Request) {
	if !rl.rate.allow("auth:"+clientIP(r), hostAuthPerMinute) {
		http.Error(w, "too many attempts", http.StatusTooManyRequests)
		return
	}
	token := bearer(r)
	c, ok := rl.accounts.verify(token)
	if !ok {
		http.Error(w, "invalid token", http.StatusUnauthorized)
		return
	}
	if !computerClient(c.Client) {
		http.Error(w, "not a computer", http.StatusForbidden)
		return
	}
	conn, err := websocket.Accept(w, r, nil)
	if err != nil {
		return
	}
	conn.SetReadLimit(maxMessage + 5)
	if rl.isDraining() {
		conn.Close(4503, "server restarting")
		return
	}
	ctx := r.Context()
	auth, ok := hostChallenge(ctx, conn)
	if !ok {
		return
	}
	id := hostID(auth.public)
	static, errStatic := b64.DecodeString(auth.Static)
	if id != r.URL.Query().Get("host") || errStatic != nil || len(static) != 32 {
		conn.Close(4400, "bad host")
		return
	}
	user, status, err := rl.accounts.register(ctx, token, id, auth)
	switch {
	case err != nil || status >= 500 || status == 0:
		log.Printf("host %s: registration failed: %d %v", id, status, err)
		conn.Close(websocket.StatusInternalError, "registration failed")
		return
	case status == http.StatusUnauthorized:
		conn.Close(4401, "token refused")
		return
	case status != http.StatusOK:
		conn.Close(4400, "bad registration")
		return
	case user != c.Sub:
		conn.Close(4401, "token refused")
		return
	}
	h := &host{id: id, conn: conn, streams: map[uint32]*stream{}, done: make(chan struct{}), user: user, authz: c.Authz}
	ready, _ := json.Marshal(map[string]string{"t": "ready", "host": id, "user": user})
	if err := conn.Write(ctx, websocket.MessageText, ready); err != nil {
		return
	}
	rl.runHost(h, auth.V)
}

// serveConnectV2: GET /relay/v2/connect?host=<host id> with any client's
// token; the open frame names the caller. Refusals after the upgrade are
// close codes a browser can read: 4403, 4404, 4429, 4503.
func (rl *relay) serveConnectV2(w http.ResponseWriter, r *http.Request) {
	// A browser that offered subprotocols fails a connection whose answer
	// names none, so every accept here names ours.
	accept := func() *websocket.Conn {
		conn, err := websocket.Accept(w, r, &websocket.AcceptOptions{
			InsecureSkipVerify: true,
			Subprotocols:       []string{subprotocol},
		})
		if err != nil {
			return nil
		}
		conn.SetReadLimit(maxMessage)
		return conn
	}
	if !rl.rate.allow("connect:"+clientIP(r), clientsPerMinute) {
		if conn := accept(); conn != nil {
			conn.Close(4429, "too many connections")
		}
		return
	}
	c, ok := rl.accounts.verify(bearer(r))
	if !ok {
		http.Error(w, "invalid token", http.StatusUnauthorized)
		return
	}
	conn := accept()
	if conn == nil {
		return
	}
	if rl.isDraining() {
		conn.Close(4503, "server restarting")
		return
	}
	rl.mu.Lock()
	h := rl.hosts[r.URL.Query().Get("host")]
	rl.mu.Unlock()
	switch {
	case h == nil:
		conn.Close(4404, "host offline")
		return
	case h.user != c.Sub:
		conn.Close(4403, "not your computer")
		return
	}
	payload, _ := json.Marshal(map[string]string{"user": c.Sub, "authz": c.Authz, "client": c.Client, "name": c.Name})
	rl.serveStream(h, conn, &stream{user: c.Sub, authz: c.Authz}, payload)
}

type heartbeatHost struct {
	Host  string `json:"host"`
	User  string `json:"user"`
	Authz string `json:"authz"`
}

type heartbeatStream struct {
	User  string `json:"user"`
	Authz string `json:"authz"`
}

// heartbeat tells the gateway which v2 hosts and callers are connected
// (AC-R2) and closes what it names as revoked.
func (rl *relay) heartbeat(ctx context.Context) error {
	hosts := []heartbeatHost{}
	streams := []heartbeatStream{}
	seen := map[heartbeatStream]bool{}
	rl.mu.Lock()
	offline := rl.offline
	rl.offline = nil
	live := make([]*host, 0, len(rl.hosts))
	for _, h := range rl.hosts {
		if h.v2() {
			live = append(live, h)
		}
	}
	rl.mu.Unlock()
	for _, h := range live {
		hosts = append(hosts, heartbeatHost{Host: h.id, User: h.user, Authz: h.authz})
		h.mu.Lock()
		for _, s := range h.streams {
			key := heartbeatStream{User: s.user, Authz: s.authz}
			if s.authz != "" && !seen[key] {
				seen[key] = true
				streams = append(streams, key)
			}
		}
		h.mu.Unlock()
	}
	if offline == nil {
		offline = []string{}
	}
	var reply struct {
		DropHosts []string `json:"drop_hosts"`
		DropAuthz []string `json:"drop_authz"`
	}
	status, err := rl.accounts.post(ctx, "/api/relay/v1/heartbeat",
		map[string]any{"hosts": hosts, "offline": offline, "streams": streams}, &reply)
	if err != nil || status != http.StatusOK {
		rl.mu.Lock()
		rl.offline = append(offline, rl.offline...)
		rl.mu.Unlock()
		if err == nil {
			err = fmt.Errorf("heartbeat answered %d", status)
		}
		return err
	}
	drop := map[string]bool{}
	for _, authz := range reply.DropAuthz {
		drop[authz] = true
	}
	// Collected under the locks, closed after: a close waits for the peer.
	var closeHosts []*host
	var closeStreams []*stream
	rl.mu.Lock()
	for _, id := range reply.DropHosts {
		if h := rl.hosts[id]; h != nil && h.v2() {
			closeHosts = append(closeHosts, h)
		}
	}
	for _, h := range rl.hosts {
		h.mu.Lock()
		for _, s := range h.streams {
			if s.authz != "" && drop[s.authz] {
				closeStreams = append(closeStreams, s)
			}
		}
		h.mu.Unlock()
	}
	rl.mu.Unlock()
	for _, h := range closeHosts {
		log.Printf("host %s: revoked by the account", h.id)
		go h.conn.Close(4403, "revoked")
	}
	for _, s := range closeStreams {
		go func() {
			s.conn.Close(4403, "revoked")
			s.cancel()
		}()
	}
	return nil
}

// runAccounts keeps the token keys fresh and sends heartbeats until ctx ends.
func (rl *relay) runAccounts(ctx context.Context) {
	keys := time.NewTicker(keysEvery)
	beat := time.NewTicker(heartbeatEvery)
	defer keys.Stop()
	defer beat.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-keys.C:
			if err := rl.accounts.refreshKeys(ctx); err != nil {
				log.Printf("relay keys: %v", err)
			}
		case <-beat.C:
			if err := rl.heartbeat(ctx); err != nil {
				log.Printf("heartbeat: %v", err)
			}
		}
	}
}

// drain closes every connection before the process stops: v2 hosts and
// their streams with 4503 (reconnect in a second), v1 ones as going away.
func (rl *relay) drain() {
	rl.mu.Lock()
	rl.draining = true
	hosts := make([]*host, 0, len(rl.hosts))
	for _, h := range rl.hosts {
		hosts = append(hosts, h)
	}
	rl.mu.Unlock()
	var wg sync.WaitGroup
	for _, h := range hosts {
		wg.Add(1)
		go func() {
			defer wg.Done()
			if h.v2() {
				h.conn.Close(4503, "server restarting")
			} else {
				h.conn.Close(websocket.StatusGoingAway, "server restarting")
			}
		}()
	}
	wg.Wait()
}

// readSecret is a flag's file, else the systemd credential `name`, else the
// environment variable; trimmed.
func readSecret(path, credential, env string) (string, error) {
	if path = credentialPath(path, credential); path != "" {
		data, err := os.ReadFile(path)
		if err != nil {
			return "", err
		}
		return strings.TrimSpace(string(data)), nil
	}
	return strings.TrimSpace(os.Getenv(env)), nil
}

// parseKeys reads comma-separated base64url Ed25519 public keys.
func parseKeys(list string) ([]ed25519.PublicKey, error) {
	var keys []ed25519.PublicKey
	for _, item := range strings.Split(list, ",") {
		if item = strings.TrimSpace(item); item == "" {
			continue
		}
		raw, err := b64.DecodeString(item)
		if err != nil || len(raw) != ed25519.PublicKeySize {
			return nil, fmt.Errorf("relay key %q is not a base64url Ed25519 public key", item)
		}
		keys = append(keys, ed25519.PublicKey(raw))
	}
	return keys, nil
}
