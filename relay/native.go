// Native push for the Aogiri app (docs/relay-protocol.md, "Native push"):
// APNs for iPhones, FCM HTTP v1 for Android phones with Google services, and
// a place for 个推 (Getui) later. The daemon sends a short generic notice
// (no command, path or reply text: Apple and Google read it) with ids only;
// the relay turns it into the provider's request, signs it with the
// provider credentials it holds, and keeps nothing.
package main

import (
	"bytes"
	"context"
	"crypto"
	"crypto/ecdsa"
	"crypto/rand"
	"crypto/rsa"
	"crypto/sha256"
	"crypto/x509"
	"encoding/hex"
	"encoding/json"
	"encoding/pem"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode/utf8"
)

const (
	apnsProduction = "https://api.push.apple.com"
	apnsSandbox    = "https://api.sandbox.push.apple.com"
	// Apple refuses a provider token older than an hour and one renewed
	// more often than every 20 minutes (TooManyProviderTokenUpdates).
	apnsTokenLifetime = 40 * time.Minute
	fcmScope          = "https://www.googleapis.com/auth/firebase.messaging"
	fcmTokenURI       = "https://oauth2.googleapis.com/token"
	fcmAPI            = "https://fcm.googleapis.com"
	// What a provider takes at most (APNs: 4096 bytes; FCM: 4000 of data).
	maxNativeMessage = 4000
	maxNoticeTitle   = 80
	maxNoticeBody    = 200
)

// notice is what a daemon asks to show natively: generic text and ids.
type notice struct {
	Title string `json:"title"`
	Body  string `json:"body"`
	// approval, error, done, update or test.
	Kind string `json:"kind"`
	// log, silent, normal or urgent (push_policy levels).
	Level string `json:"level"`
	// Groups the notices of one thing (a later one replaces it).
	Tag     string `json:"tag"`
	Session string `json:"session"`
	Host    string `json:"host"`
	// An approval the notification's 允许/拒绝 buttons answer: the
	// approval_request's call id, or the deferred action's id.
	CallID   string `json:"call_id"`
	ActionID string `json:"action_id"`
	Count    int    `json:"count"`
	// The daemon's remote page for it (/remote?…), for the web fallback.
	URL string `json:"url"`
}

// data is what the app reads from a tapped notification (Expo's `data`).
func (n *notice) data() map[string]any {
	data := map[string]any{"kind": n.Kind, "tag": n.Tag}
	for key, value := range map[string]string{"session": n.Session, "host": n.Host, "call_id": n.CallID, "action_id": n.ActionID} {
		if value != "" {
			data[key] = value
		}
	}
	if n.Count > 1 {
		data["count"] = n.Count
	}
	if n.URL != "" {
		data["url"] = n.URL
	}
	return data
}

// category names the app's notification category with 允许/拒绝 buttons:
// one approval with an id to answer.
func (n *notice) category() string {
	if n.Kind == "approval" && n.Count <= 1 && (n.CallID != "" || n.ActionID != "") {
		return "approval"
	}
	return ""
}

func idText(value string) bool {
	if len(value) > 128 {
		return false
	}
	for _, c := range value {
		if !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || c == '-' || c == '_' || c == '.' || c == ':') {
			return false
		}
	}
	return true
}

func parseNotice(raw json.RawMessage) (*notice, error) {
	var n notice
	if len(raw) == 0 || len(raw) > maxPlaintext || json.Unmarshal(raw, &n) != nil {
		return nil, errors.New("bad payload")
	}
	if n.Title == "" || utf8.RuneCountInString(n.Title) > maxNoticeTitle || utf8.RuneCountInString(n.Body) > maxNoticeBody {
		return nil, errors.New("bad payload")
	}
	switch n.Kind {
	case "approval", "error", "done", "update", "test":
	default:
		return nil, errors.New("bad payload kind")
	}
	switch n.Level {
	case "":
		n.Level = "normal"
	case "silent", "normal", "urgent":
	default:
		return nil, errors.New("bad payload level")
	}
	for _, id := range []string{n.Tag, n.Session, n.Host, n.CallID, n.ActionID} {
		if !idText(id) {
			return nil, errors.New("bad payload id")
		}
	}
	if n.URL != "" && !remoteURL(n.URL) {
		return nil, errors.New("bad payload url")
	}
	return &n, nil
}

// remoteURL is a path of the remote page with a plain query.
func remoteURL(value string) bool {
	if len(value) > 256 || !strings.HasPrefix(value, "/remote") {
		return false
	}
	for _, c := range value {
		if !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || strings.ContainsRune("-_.:/?=&%", c)) {
			return false
		}
	}
	return true
}

// nativeTarget is a device token of one provider.
type nativeTarget struct {
	Token string
	// APNs only: production (App Store, TestFlight) or sandbox (builds
	// from Xcode and development builds).
	Environment string
}

// nativeResult is a provider's answer: its HTTP status and, when it
// refused, its reason code (never content).
type nativeResult struct {
	Status int
	Reason string
}

// nativeSender delivers to one provider.
type nativeSender interface {
	send(ctx context.Context, target nativeTarget, n *notice, delivery nativeDelivery) (nativeResult, error)
}

type nativeDelivery struct {
	ttl   int
	topic string
	now   time.Time
}

// relayStatus is what the daemon hears for a provider's answer: 410 means
// forget the token.
func (r nativeResult) relayStatus(provider string) int {
	switch {
	case r.Status >= 200 && r.Status < 300:
		return http.StatusNoContent
	case r.Status == http.StatusGone:
		return http.StatusGone
	case provider == "apns" && r.Status == http.StatusBadRequest &&
		(r.Reason == "BadDeviceToken" || r.Reason == "DeviceTokenNotForTopic"):
		return http.StatusGone
	case provider == "fcm" && (r.Reason == "UNREGISTERED" || r.Reason == "SENDER_ID_MISMATCH"):
		return http.StatusGone
	case provider == "fcm" && r.Status == http.StatusNotFound:
		return http.StatusGone
	case r.Status == http.StatusTooManyRequests:
		return http.StatusTooManyRequests
	case r.Status == http.StatusRequestEntityTooLarge:
		return http.StatusRequestEntityTooLarge
	default:
		return http.StatusBadGateway
	}
}

// ---- APNs ----

type apnsConfig struct {
	keyID  string
	teamID string
	topic  string
}

type apnsSender struct {
	key    *ecdsa.PrivateKey
	config apnsConfig
	client *http.Client
	// Origins by environment; tests point both at a local server.
	origins map[string]string

	mu      sync.Mutex
	token   string
	created time.Time
}

func loadAPNs(path string, config apnsConfig) (*apnsSender, error) {
	text, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	block, _ := pem.Decode(text)
	if block == nil || block.Type != "PRIVATE KEY" {
		return nil, fmt.Errorf("%s: not a .p8 key (PEM PRIVATE KEY)", path)
	}
	parsed, err := x509.ParsePKCS8PrivateKey(block.Bytes)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	key, ok := parsed.(*ecdsa.PrivateKey)
	if !ok || key.Curve.Params().Name != "P-256" {
		return nil, fmt.Errorf("%s: not a P-256 key", path)
	}
	if !appleID(config.keyID) || !appleID(config.teamID) {
		return nil, errors.New("APNs needs a key id and a team id (10 letters or digits each)")
	}
	if config.topic == "" {
		return nil, errors.New("APNs needs a topic (the app's bundle id)")
	}
	return &apnsSender{
		key:     key,
		config:  config,
		client:  &http.Client{Timeout: pushTimeout, Transport: &http.Transport{ForceAttemptHTTP2: true, IdleConnTimeout: 10 * time.Minute}, CheckRedirect: noRedirect},
		origins: map[string]string{"production": apnsProduction, "sandbox": apnsSandbox},
	}, nil
}

func appleID(value string) bool {
	if len(value) != 10 {
		return false
	}
	for _, c := range value {
		if !(c >= 'A' && c <= 'Z' || c >= '0' && c <= '9') {
			return false
		}
	}
	return true
}

func noRedirect(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }

// providerToken is the ES256 JWT Apple wants as the bearer, reused for
// apnsTokenLifetime.
func (a *apnsSender) providerToken(now time.Time) (string, error) {
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.token != "" && now.Sub(a.created) < apnsTokenLifetime && !now.Before(a.created) {
		return a.token, nil
	}
	head, _ := json.Marshal(map[string]string{"alg": "ES256", "kid": a.config.keyID})
	claims, _ := json.Marshal(map[string]any{"iss": a.config.teamID, "iat": now.Unix()})
	signing := b64.EncodeToString(head) + "." + b64.EncodeToString(claims)
	signature, err := es256(a.key, signing)
	if err != nil {
		return "", err
	}
	a.token, a.created = signing+"."+signature, now
	return a.token, nil
}

// forgetToken drops a token Apple said is expired or invalid.
func (a *apnsSender) forgetToken() {
	a.mu.Lock()
	a.token = ""
	a.mu.Unlock()
}

func es256(key *ecdsa.PrivateKey, signing string) (string, error) {
	digest := sha256.Sum256([]byte(signing))
	r, s, err := ecdsa.Sign(rand.Reader, key, digest[:])
	if err != nil {
		return "", err
	}
	signature := make([]byte, 64)
	r.FillBytes(signature[:32])
	s.FillBytes(signature[32:])
	return b64.EncodeToString(signature), nil
}

func apnsToken(token string) bool {
	if len(token) < 32 || len(token) > 200 || len(token)%2 != 0 {
		return false
	}
	_, err := hex.DecodeString(token)
	return err == nil
}

// apnsPayload is the notification as iOS shows it; `body` holds the data
// (where expo-notifications reads it on iOS).
func apnsPayload(n *notice) ([]byte, error) {
	aps := map[string]any{
		"alert":     map[string]string{"title": n.Title, "body": n.Body},
		"thread-id": firstNonEmpty(n.Session, n.Tag),
	}
	switch n.Level {
	case "urgent":
		aps["sound"] = "default"
		aps["interruption-level"] = "time-sensitive"
	case "normal":
		aps["sound"] = "default"
		aps["interruption-level"] = "active"
	default:
		aps["interruption-level"] = "passive"
	}
	if category := n.category(); category != "" {
		aps["category"] = category
	}
	return json.Marshal(map[string]any{"aps": aps, "body": n.data()})
}

func firstNonEmpty(values ...string) string {
	for _, v := range values {
		if v != "" {
			return v
		}
	}
	return ""
}

func (a *apnsSender) send(ctx context.Context, target nativeTarget, n *notice, delivery nativeDelivery) (nativeResult, error) {
	origin, ok := a.origins[target.Environment]
	if !ok {
		return nativeResult{}, errors.New("unknown APNs environment")
	}
	body, err := apnsPayload(n)
	if err != nil {
		return nativeResult{}, err
	}
	if len(body) > maxNativeMessage {
		return nativeResult{Status: http.StatusRequestEntityTooLarge}, nil
	}
	token, err := a.providerToken(delivery.now)
	if err != nil {
		return nativeResult{}, err
	}
	out, err := http.NewRequestWithContext(ctx, http.MethodPost, origin+"/3/device/"+target.Token, bytes.NewReader(body))
	if err != nil {
		return nativeResult{}, err
	}
	out.Header.Set("Authorization", "bearer "+token)
	out.Header.Set("apns-topic", a.config.topic)
	out.Header.Set("apns-push-type", "alert")
	out.Header.Set("Content-Type", "application/json")
	if n.Level == "silent" {
		out.Header.Set("apns-priority", "5")
	} else {
		out.Header.Set("apns-priority", "10")
	}
	expiration := int64(0)
	if delivery.ttl > 0 {
		expiration = delivery.now.Add(time.Duration(delivery.ttl) * time.Second).Unix()
	}
	out.Header.Set("apns-expiration", strconv.FormatInt(expiration, 10))
	if delivery.topic != "" && len(delivery.topic) <= 64 {
		out.Header.Set("apns-collapse-id", delivery.topic)
	}
	res, err := a.client.Do(out)
	if err != nil {
		return nativeResult{}, err
	}
	defer res.Body.Close()
	var answer struct {
		Reason string `json:"reason"`
	}
	json.NewDecoder(io.LimitReader(res.Body, 4096)).Decode(&answer)
	if answer.Reason == "ExpiredProviderToken" || answer.Reason == "InvalidProviderToken" {
		a.forgetToken()
	}
	return nativeResult{Status: res.StatusCode, Reason: answer.Reason}, nil
}

// ---- FCM HTTP v1 ----

type serviceAccount struct {
	Type         string `json:"type"`
	ProjectID    string `json:"project_id"`
	PrivateKeyID string `json:"private_key_id"`
	PrivateKey   string `json:"private_key"`
	ClientEmail  string `json:"client_email"`
	TokenURI     string `json:"token_uri"`
}

type fcmSender struct {
	project  string
	email    string
	keyID    string
	key      *rsa.PrivateKey
	tokenURI string
	api      string
	client   *http.Client

	mu      sync.Mutex
	access  string
	expires time.Time
}

func loadFCM(path string) (*fcmSender, error) {
	text, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var account serviceAccount
	if err := json.Unmarshal(text, &account); err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	if account.Type != "service_account" || account.ProjectID == "" || account.ClientEmail == "" {
		return nil, fmt.Errorf("%s: not a service account key", path)
	}
	// The token endpoint is Google's; a key file that names another
	// would send our signed assertion elsewhere.
	if account.TokenURI != "" && account.TokenURI != fcmTokenURI {
		return nil, fmt.Errorf("%s: unexpected token_uri %q", path, account.TokenURI)
	}
	block, _ := pem.Decode([]byte(account.PrivateKey))
	if block == nil {
		return nil, fmt.Errorf("%s: no private key", path)
	}
	parsed, err := x509.ParsePKCS8PrivateKey(block.Bytes)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	key, ok := parsed.(*rsa.PrivateKey)
	if !ok {
		return nil, fmt.Errorf("%s: not an RSA key", path)
	}
	return &fcmSender{
		project:  account.ProjectID,
		email:    account.ClientEmail,
		keyID:    account.PrivateKeyID,
		key:      key,
		tokenURI: fcmTokenURI,
		api:      fcmAPI,
		client:   &http.Client{Timeout: pushTimeout, CheckRedirect: noRedirect},
	}, nil
}

// accessToken is an OAuth 2.0 token for FCM from a signed JWT assertion
// (RFC 7523), reused until five minutes before it expires.
func (f *fcmSender) accessToken(ctx context.Context, now time.Time) (string, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.access != "" && now.Add(5*time.Minute).Before(f.expires) {
		return f.access, nil
	}
	head, _ := json.Marshal(map[string]string{"alg": "RS256", "typ": "JWT", "kid": f.keyID})
	claims, _ := json.Marshal(map[string]any{
		"iss": f.email, "scope": fcmScope, "aud": fcmTokenURI,
		"iat": now.Unix(), "exp": now.Add(time.Hour).Unix(),
	})
	signing := b64.EncodeToString(head) + "." + b64.EncodeToString(claims)
	digest := sha256.Sum256([]byte(signing))
	signature, err := rsa.SignPKCS1v15(nil, f.key, crypto.SHA256, digest[:])
	if err != nil {
		return "", err
	}
	form := url.Values{
		"grant_type": {"urn:ietf:params:oauth:grant-type:jwt-bearer"},
		"assertion":  {signing + "." + b64.EncodeToString(signature)},
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, f.tokenURI, strings.NewReader(form.Encode()))
	if err != nil {
		return "", err
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	res, err := f.client.Do(req)
	if err != nil {
		return "", err
	}
	defer res.Body.Close()
	var answer struct {
		AccessToken string `json:"access_token"`
		ExpiresIn   int    `json:"expires_in"`
		Error       string `json:"error"`
	}
	json.NewDecoder(io.LimitReader(res.Body, 16<<10)).Decode(&answer)
	if res.StatusCode != http.StatusOK || answer.AccessToken == "" {
		return "", fmt.Errorf("google token endpoint answered %d %s", res.StatusCode, answer.Error)
	}
	if answer.ExpiresIn <= 0 {
		answer.ExpiresIn = 3600
	}
	f.access, f.expires = answer.AccessToken, now.Add(time.Duration(answer.ExpiresIn)*time.Second)
	return f.access, nil
}

func (f *fcmSender) forgetToken() {
	f.mu.Lock()
	f.access = ""
	f.mu.Unlock()
}

func fcmToken(token string) bool {
	if len(token) < 16 || len(token) > 512 {
		return false
	}
	for _, c := range token {
		if !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || c == '-' || c == '_' || c == ':' || c == '.') {
			return false
		}
	}
	return true
}

// fcmChannel is the Android notification channel the app creates for a
// level: approvals and errors are loud, the rest normal, quiet hours quiet.
func fcmChannel(level string) string {
	switch level {
	case "urgent":
		return "approvals"
	case "normal":
		return "updates"
	default:
		return "quiet"
	}
}

// fcmMessage is a data-only message: expo-notifications shows it itself
// (title, message, channelId, categoryId and tag are its keys), so the
// 允许/拒绝 buttons of the category work on Android too.
func fcmMessage(target nativeTarget, n *notice, delivery nativeDelivery) ([]byte, error) {
	data, _ := json.Marshal(n.data())
	fields := map[string]string{
		"title":     n.Title,
		"message":   n.Body,
		"tag":       firstNonEmpty(n.Tag, n.Session),
		"channelId": fcmChannel(n.Level),
		"body":      string(data),
	}
	if category := n.category(); category != "" {
		fields["categoryId"] = category
	}
	android := map[string]any{"priority": "HIGH", "ttl": fmt.Sprintf("%ds", delivery.ttl)}
	if n.Level == "silent" {
		android["priority"] = "NORMAL"
	}
	if delivery.topic != "" {
		android["collapse_key"] = delivery.topic
	}
	return json.Marshal(map[string]any{"message": map[string]any{"token": target.Token, "data": fields, "android": android}})
}

func (f *fcmSender) send(ctx context.Context, target nativeTarget, n *notice, delivery nativeDelivery) (nativeResult, error) {
	body, err := fcmMessage(target, n, delivery)
	if err != nil {
		return nativeResult{}, err
	}
	if len(body) > maxNativeMessage+512 {
		return nativeResult{Status: http.StatusRequestEntityTooLarge}, nil
	}
	access, err := f.accessToken(ctx, delivery.now)
	if err != nil {
		return nativeResult{}, err
	}
	out, err := http.NewRequestWithContext(ctx, http.MethodPost, f.api+"/v1/projects/"+url.PathEscape(f.project)+"/messages:send", bytes.NewReader(body))
	if err != nil {
		return nativeResult{}, err
	}
	out.Header.Set("Authorization", "Bearer "+access)
	out.Header.Set("Content-Type", "application/json")
	res, err := f.client.Do(out)
	if err != nil {
		return nativeResult{}, err
	}
	defer res.Body.Close()
	result := nativeResult{Status: res.StatusCode}
	if res.StatusCode >= 300 {
		result.Reason = fcmErrorCode(io.LimitReader(res.Body, 16<<10))
		if res.StatusCode == http.StatusUnauthorized {
			f.forgetToken()
		}
	}
	return result, nil
}

// fcmErrorCode is the FcmError code of an error answer (UNREGISTERED, …),
// else its status (INVALID_ARGUMENT, …).
func fcmErrorCode(body io.Reader) string {
	var answer struct {
		Error struct {
			Status  string `json:"status"`
			Details []struct {
				Type      string `json:"@type"`
				ErrorCode string `json:"errorCode"`
			} `json:"details"`
		} `json:"error"`
	}
	json.NewDecoder(body).Decode(&answer)
	for _, detail := range answer.Error.Details {
		if strings.HasSuffix(detail.Type, "google.firebase.fcm.v1.FcmError") && detail.ErrorCode != "" {
			return detail.ErrorCode
		}
	}
	return answer.Error.Status
}

// ---- dispatch ----

// natives are the configured providers; a missing one answers 501.
type natives map[string]nativeSender

// target checks a subscription's token for its provider.
func nativeTargetOf(sub pushSubscription) (nativeTarget, bool) {
	switch sub.Provider {
	case "apns":
		env := sub.Environment
		if env == "" {
			env = "production"
		}
		return nativeTarget{Token: strings.ToLower(sub.Token), Environment: env},
			apnsToken(sub.Token) && (env == "production" || env == "sandbox")
	case "fcm":
		return nativeTarget{Token: sub.Token}, fcmToken(sub.Token)
	case "getui":
		return nativeTarget{Token: sub.ClientID}, sub.ClientID != ""
	}
	return nativeTarget{}, false
}
