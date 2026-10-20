package main

import (
	"context"
	"os"
	"strings"
	"testing"
	"time"
)

// Live check of an APNs key (opt-in): LYNSHEN_RELAY_APNS_LIVE=<.p8>,
// LYNSHEN_RELAY_APNS_KEY_ID, LYNSHEN_RELAY_APNS_TEAM_ID. Posts to a token no
// phone holds in both environments; Apple must answer BadDeviceToken (the
// key, team and topic are accepted), not an auth error. Prints no secret.
func TestAPNsLive(t *testing.T) {
	path := os.Getenv("LYNSHEN_RELAY_APNS_LIVE")
	if path == "" {
		t.Skip("LYNSHEN_RELAY_APNS_LIVE not set")
	}
	a, err := loadAPNs(path, apnsConfig{
		keyID:  os.Getenv("LYNSHEN_RELAY_APNS_KEY_ID"),
		teamID: os.Getenv("LYNSHEN_RELAY_APNS_TEAM_ID"),
		topic:  "com.lynshen.aogiri",
	})
	if err != nil {
		t.Fatal(err)
	}
	n := &notice{Title: "Aogiri", Body: "test", Kind: "test", Level: "normal"}
	for _, env := range []string{"sandbox", "production"} {
		ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
		result, err := a.send(ctx, nativeTarget{Token: strings.Repeat("ab", 32), Environment: env}, n, nativeDelivery{ttl: 60, now: time.Now()})
		cancel()
		if err != nil {
			t.Fatalf("%s: %v", env, err)
		}
		t.Logf("%s: APNs answered %d %s -> relay %d", env, result.Status, result.Reason, result.relayStatus("apns"))
		if result.Status != 400 || result.Reason != "BadDeviceToken" {
			t.Errorf("%s: APNs answered %d %s", env, result.Status, result.Reason)
		}
	}
}
