package main

import (
	"context"
	"os"
	"testing"
	"time"
)

// Live check of an FCM service account (opt-in): LYNSHEN_RELAY_FCM_LIVE=<key file>.
// Gets an access token and posts to a token no phone holds; FCM must answer
// that the token is bad (400 INVALID_ARGUMENT or 404 UNREGISTERED), which
// proves the key, the project and the API. Prints no secret.
func TestFCMLive(t *testing.T) {
	path := os.Getenv("LYNSHEN_RELAY_FCM_LIVE")
	if path == "" {
		t.Skip("LYNSHEN_RELAY_FCM_LIVE not set")
	}
	f, err := loadFCM(path)
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	if _, err := f.accessToken(ctx, time.Now()); err != nil {
		t.Fatal(err)
	}
	n := &notice{Title: "Aogiri", Body: "test", Kind: "test", Level: "normal"}
	result, err := f.send(ctx, nativeTarget{Token: "dXNlcjEyMzQ1Njc4OTA:APA91bH-not-a-real-token"}, n, nativeDelivery{ttl: 60, now: time.Now()})
	if err != nil {
		t.Fatal(err)
	}
	t.Logf("project %s: FCM answered %d %s -> relay %d", f.project, result.Status, result.Reason, result.relayStatus("fcm"))
	if result.Status != 400 && result.Status != 404 {
		t.Fatalf("FCM answered %d %s", result.Status, result.Reason)
	}
}
