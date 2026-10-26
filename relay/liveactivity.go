// Live Activity pushes for the Aogiri app's Dynamic Island
// (docs/relay-protocol.md, "Live Activity push"): the daemon sends the
// activity's state (generic words, counts and ids, never a command, a path
// or a reply), the relay checks it against the one activity the app has
// (`ComputerActivityAttributes`), signs it for APNs and keeps nothing.
package main

import (
	"bytes"
	"context"
	"encoding/hex"
	"encoding/json"
	"errors"
	"net/http"
	"strconv"
	"time"
	"unicode/utf8"
)

const (
	// The only ActivityAttributes type the app declares.
	liveActivityAttributesType = "ComputerActivityAttributes"
	// apns-topic suffix for Live Activity pushes.
	liveActivityTopicSuffix = ".push-type.liveactivity"
	// APNs takes at most 4096 bytes of payload.
	maxLiveActivityPayload = 4096
	// A Live Activity update is stale soon: the next one replaces it.
	liveActivityDefaultTTL = 10 * 60
	liveActivityMaxTTL     = 60 * 60
	maxContentText         = 200
	maxContentCount        = 100000
)

// liveActivityRequest is the `liveactivity` object of a push request.
type liveActivityRequest struct {
	// start, update or end.
	Event        string          `json:"event"`
	ContentState json.RawMessage `json:"content_state"`
	// Unix seconds.
	StaleDate     *int64 `json:"stale_date"`
	DismissalDate *int64 `json:"dismissal_date"`
	// 0..100: which of the app's activities iOS shows first.
	RelevanceScore *float64 `json:"relevance_score"`
	// Unix seconds of the state; iOS ignores one older than what it shows.
	Timestamp *int64 `json:"timestamp"`
	// start only.
	AttributesType string          `json:"attributes_type"`
	Attributes     json.RawMessage `json:"attributes"`
	// start or update of a waiting state.
	Alert *liveActivityAlert `json:"alert"`
}

type liveActivityAlert struct {
	Title string `json:"title"`
	Body  string `json:"body"`
}

// liveActivity is a checked request, ready to send.
type liveActivity struct {
	event    string
	priority int
	aps      map[string]any
}

// The ContentState fields of ComputerActivityAttributes and their JSON
// kinds; all are always present.
var contentStateFields = map[string]string{
	"phase": "string", "title": "string", "detail": "string", "session": "string",
	"sessionTitle": "string", "tasks": "int", "waiting": "int", "done": "int",
	"total": "int", "since": "number", "ember": "string",
}

func decodeStrict(raw json.RawMessage, into any) error {
	decoder := json.NewDecoder(bytes.NewReader(raw))
	decoder.DisallowUnknownFields()
	decoder.UseNumber()
	if err := decoder.Decode(into); err != nil {
		return err
	}
	if decoder.More() {
		return errors.New("trailing data")
	}
	return nil
}

// checkContentState checks the activity's ContentState: exactly the
// contract's fields, of their kinds, short text, phase and ember from their
// sets. Returns the phase.
func checkContentState(raw json.RawMessage) (string, error) {
	var state map[string]any
	if len(raw) == 0 || decodeStrict(raw, &state) != nil || state == nil {
		return "", errors.New("content_state must be an object")
	}
	if len(state) != len(contentStateFields) {
		return "", errors.New("content_state must have exactly the ContentState fields")
	}
	for name, kind := range contentStateFields {
		value, ok := state[name]
		if !ok {
			return "", errors.New("content_state lacks " + name)
		}
		switch kind {
		case "string":
			text, ok := value.(string)
			if !ok || utf8.RuneCountInString(text) > maxContentText {
				return "", errors.New("content_state " + name + " must be a short string")
			}
		case "int":
			number, ok := value.(json.Number)
			n, err := strconv.ParseInt(string(number), 10, 64)
			if !ok || err != nil || n < 0 || n > maxContentCount {
				return "", errors.New("content_state " + name + " must be a count")
			}
		case "number":
			number, ok := value.(json.Number)
			f, err := number.Float64()
			if !ok || err != nil || f < 0 || f > 1e11 {
				return "", errors.New("content_state " + name + " must be seconds since 1970")
			}
		}
	}
	phase := state["phase"].(string)
	switch phase {
	case "waiting", "running", "done", "failed":
	default:
		return "", errors.New("content_state phase must be waiting, running, done or failed")
	}
	switch state["ember"].(string) {
	case "calm", "thinking", "done", "attention":
	default:
		return "", errors.New("content_state ember must be calm, thinking, done or attention")
	}
	if !idText(state["session"].(string)) {
		return "", errors.New("content_state session must be an id")
	}
	return phase, nil
}

// checkAttributes checks ComputerActivityAttributes: {host, hostName}.
func checkAttributes(raw json.RawMessage) error {
	var attributes struct {
		Host     *string `json:"host"`
		HostName *string `json:"hostName"`
	}
	if len(raw) == 0 || decodeStrict(raw, &attributes) != nil || attributes.Host == nil || attributes.HostName == nil {
		return errors.New("attributes must be {host, hostName}")
	}
	if *attributes.Host == "" || !idText(*attributes.Host) || utf8.RuneCountInString(*attributes.HostName) > 64 {
		return errors.New("attributes must be {host, hostName}")
	}
	return nil
}

// parseLiveActivity checks a Live Activity request and builds its `aps`.
func parseLiveActivity(raw json.RawMessage, now time.Time) (*liveActivity, error) {
	var req liveActivityRequest
	if decodeStrict(raw, &req) != nil {
		return nil, errors.New("bad liveactivity")
	}
	switch req.Event {
	case "start", "update", "end":
	default:
		return nil, errors.New("liveactivity event must be start, update or end")
	}
	phase, err := checkContentState(req.ContentState)
	if err != nil {
		return nil, err
	}
	var state bytes.Buffer
	if err := json.Compact(&state, req.ContentState); err != nil {
		return nil, errors.New("bad content_state")
	}
	timestamp := now.Unix()
	if req.Timestamp != nil {
		// A state from the last hour (one coalesced for a while), not
		// from the future.
		if *req.Timestamp > now.Add(pushClockSkew).Unix() || *req.Timestamp < now.Add(-time.Hour).Unix() {
			return nil, errors.New("liveactivity timestamp out of range")
		}
		timestamp = *req.Timestamp
	}
	aps := map[string]any{
		"timestamp":     timestamp,
		"event":         req.Event,
		"content-state": json.RawMessage(state.Bytes()),
	}
	// An activity lives at most 8 hours, then 4 more on the Lock Screen.
	within := func(seconds int64, after time.Duration) bool {
		return seconds >= now.Add(-12*time.Hour).Unix() && seconds <= now.Add(after).Unix()
	}
	if req.StaleDate != nil {
		if !within(*req.StaleDate, 12*time.Hour) {
			return nil, errors.New("liveactivity stale_date out of range")
		}
		aps["stale-date"] = *req.StaleDate
	}
	if req.DismissalDate != nil {
		if req.Event != "end" {
			return nil, errors.New("liveactivity dismissal_date is for end only")
		}
		if !within(*req.DismissalDate, 4*time.Hour) {
			return nil, errors.New("liveactivity dismissal_date out of range")
		}
		aps["dismissal-date"] = *req.DismissalDate
	}
	if req.RelevanceScore != nil {
		if *req.RelevanceScore < 0 || *req.RelevanceScore > 100 {
			return nil, errors.New("liveactivity relevance_score must be 0..100")
		}
		aps["relevance-score"] = *req.RelevanceScore
	}
	if req.Event == "start" {
		if req.AttributesType != liveActivityAttributesType {
			return nil, errors.New("liveactivity attributes_type must be " + liveActivityAttributesType)
		}
		if err := checkAttributes(req.Attributes); err != nil {
			return nil, err
		}
		var attributes bytes.Buffer
		json.Compact(&attributes, req.Attributes)
		aps["attributes-type"] = req.AttributesType
		aps["attributes"] = json.RawMessage(attributes.Bytes())
	} else if req.AttributesType != "" || len(req.Attributes) > 0 {
		return nil, errors.New("liveactivity attributes are for start only")
	}
	if req.Alert != nil {
		if req.Event == "end" || phase != "waiting" {
			return nil, errors.New("liveactivity alert is for a waiting state only")
		}
		if req.Alert.Title == "" || utf8.RuneCountInString(req.Alert.Title) > maxNoticeTitle ||
			utf8.RuneCountInString(req.Alert.Body) > maxNoticeBody {
			return nil, errors.New("bad liveactivity alert")
		}
		aps["alert"] = map[string]string{"title": req.Alert.Title, "body": req.Alert.Body}
	}
	// Waiting, finished and ended go at once; a plain step update may wait
	// for the phone's power budget.
	priority := 5
	if req.Event != "update" || phase != "running" || req.Alert != nil {
		priority = 10
	}
	return &liveActivity{event: req.Event, priority: priority, aps: aps}, nil
}

// liveActivityToken is a push-to-start or activity token in hex: longer
// than a device token (128 bytes on current iOS).
func liveActivityToken(token string) bool {
	if len(token) < 32 || len(token) > 512 || len(token)%2 != 0 {
		return false
	}
	_, err := hex.DecodeString(token)
	return err == nil
}

// liveActivitySender is a provider that serves Live Activities (APNs).
type liveActivitySender interface {
	sendLiveActivity(ctx context.Context, target nativeTarget, la *liveActivity, ttl int, now time.Time) (nativeResult, error)
}

func (a *apnsSender) sendLiveActivity(ctx context.Context, target nativeTarget, la *liveActivity, ttl int, now time.Time) (nativeResult, error) {
	body, err := json.Marshal(map[string]any{"aps": la.aps})
	if err != nil {
		return nativeResult{}, err
	}
	if len(body) > maxLiveActivityPayload {
		return nativeResult{Status: http.StatusRequestEntityTooLarge}, nil
	}
	return a.post(ctx, target, body, map[string]string{
		"apns-topic":      a.config.topic + liveActivityTopicSuffix,
		"apns-push-type":  "liveactivity",
		"apns-priority":   strconv.Itoa(la.priority),
		"apns-expiration": strconv.FormatInt(now.Add(time.Duration(ttl)*time.Second).Unix(), 10),
	}, now)
}
