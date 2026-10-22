//! Ticket #55: an attached image that is larger than 1 MiB reached the model,
//! and an image that cannot be attached is named to the model instead of
//! disappearing (the model answered "there is no image in the message").
//!
//! Runs against the temporary HOME and fake model of `support/fake_model.rs`.

#[path = "support/fake_model.rs"]
mod fake_model;

use fake_model::{requests, setup, temp_dir};
use lynshen_agent_core::{AgentCore, AgentEvent};
use serde_json::Value;
use std::{
    fs, thread,
    time::{Duration, Instant},
};

fn pump_until_ready(core: &mut AgentCore) -> Vec<AgentEvent> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        let batch = core.poll_events();
        let done = batch
            .iter()
            .any(|event| matches!(event, AgentEvent::Status(status) if status == "ready"));
        seen.extend(batch);
        if done {
            return seen;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out; events: {seen:#?}");
}

/// The last request whose user messages contain `marker`.
fn request_with(marker: &str) -> Value {
    requests()
        .into_iter()
        .rev()
        .find(|body| body.to_string().contains(marker))
        .expect("a request with the marker")
}

/// Every image data URL of the request's messages.
fn image_urls(body: &Value) -> Vec<String> {
    let mut urls = Vec::new();
    for message in body["messages"].as_array().into_iter().flatten() {
        for part in message["content"].as_array().into_iter().flatten() {
            if let Some(url) = part["image_url"]["url"]
                .as_str()
                .or(part["image_url"].as_str())
            {
                urls.push(url.to_string());
            }
        }
    }
    urls
}

/// A PNG signature followed by `len - 8` filler bytes: the engine checks the
/// extension and the size, not the pixels.
fn png_of(len: usize, path: &std::path::Path) {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.resize(len, 7);
    fs::write(path, bytes).unwrap();
}

#[test]
fn an_image_over_one_mib_is_sent_to_the_model() {
    let _guard = setup();
    let dir = temp_dir("t55-large");
    let image = dir.join("render.png");
    png_of(2_500_000, &image);
    let mut core = AgentCore::open(dir.clone()).unwrap();

    let events = core.submit_user_message_with_images(
        "T55_LARGE 这个怎么样".to_string(),
        vec![image.display().to_string()],
    );
    assert!(
        !events.iter().any(
            |event| matches!(event, AgentEvent::Info(text) if text.contains("skipped attachment"))
        ),
        "{events:#?}"
    );
    pump_until_ready(&mut core);

    let body = request_with("T55_LARGE");
    let urls = image_urls(&body);
    assert_eq!(urls.len(), 1, "{body}");
    assert!(urls[0].starts_with("data:image/png;base64,"));
    assert!(!body.to_string().contains("NOT sent to you"));
}

#[test]
fn an_image_that_cannot_be_sent_is_named_to_the_model() {
    let _guard = setup();
    let dir = temp_dir("t55-too-large");
    let huge = dir.join("huge.png");
    png_of(4_000_000, &huge);
    let bitmap = dir.join("scan.bmp");
    fs::write(&bitmap, b"BM....").unwrap();
    let mut core = AgentCore::open(dir.clone()).unwrap();

    let events = core.submit_user_message_with_images(
        "T55_HUGE look".to_string(),
        vec![huge.display().to_string(), bitmap.display().to_string()],
    );
    let skipped = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::Info(text) if text.starts_with("skipped attachment")))
        .count();
    assert_eq!(skipped, 2, "{events:#?}");
    pump_until_ready(&mut core);

    let body = request_with("T55_HUGE");
    assert!(image_urls(&body).is_empty(), "{body}");
    let text = body.to_string();
    assert!(text.contains("NOT sent to you"), "{text}");
    assert!(
        text.contains("huge.png: image is too large to attach (4.0 MB; the limit is 3.75 MB)"),
        "{text}"
    );
    assert!(text.contains("scan.bmp: not a supported image"), "{text}");
}
