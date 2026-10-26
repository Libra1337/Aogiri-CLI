//! Plays the account side of relay v2 against a live gateway, relay and
//! daemon (docs/relay-protocol.md §7): a phone signed in to the account asks
//! to connect, the desktop (a local client of the daemon) allows it, and the
//! phone lists the sessions. Then the same phone reconnects without asking,
//! another account is refused, and a revoked login is dropped.
//!
//! cargo run -p lynshen-daemon --example account_phone -- <gateway> <relay ws base> <daemon ws addr> <daemon token file> <phone key file> <other key file> <computer key file>
//!
//! The key files hold device keys of the account (the phone) and of another
//! account. Nothing secret is printed.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use lynshen_daemon::noise;
use serde_json::{json, Value};
use std::{
    net::TcpStream,
    time::{Duration, Instant},
};
use tungstenite::{client::IntoClientRequest, stream::MaybeTlsStream, Message, WebSocket};

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

fn gateway(base: &str, method: &str, path: &str, key: &str) -> Value {
    let request = ureq::request(method, &format!("{base}{path}"))
        .set("Authorization", &format!("Bearer {key}"))
        .timeout(Duration::from_secs(10));
    match request.call() {
        Ok(response) => response.into_json().unwrap(),
        Err(ureq::Error::Status(status, response)) => {
            json!({ "status": status, "body": response.into_string().unwrap_or_default() })
        }
        Err(error) => panic!("{path}: {error}"),
    }
}

fn read_timeout(socket: &Socket, timeout: Duration) {
    if let MaybeTlsStream::Plain(stream) = socket.get_ref() {
        stream.set_read_timeout(Some(timeout)).unwrap();
    }
}

/// The desktop: a local client of the daemon holding its token.
fn desktop(address: &str, token: &str) -> Socket {
    let (socket, _) = tungstenite::connect(format!("ws://{address}/?token={token}")).unwrap();
    read_timeout(&socket, Duration::from_millis(100));
    socket
}

fn next_text(socket: &mut Socket, until: Instant) -> Option<Value> {
    while Instant::now() < until {
        match socket.read() {
            Ok(Message::Text(text)) => return serde_json::from_str(text.as_str()).ok(),
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("desktop: {error}"),
        }
    }
    None
}

/// A phone stream through relay v2 with a fresh relay token. Returns the
/// msg 2 payload and, when allowed, the transport and socket.
fn phone_connect(
    relay: &str,
    host: &str,
    token: &str,
    phone_key: &[u8],
    host_key: &[u8],
    hello: Value,
) -> Result<(Value, Option<(noise::Transport, Socket)>), String> {
    let mut request = format!("{relay}/connect?host={host}")
        .into_client_request()
        .unwrap();
    // As a browser does: the token in the subprotocols.
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        format!("lynshen-relay, lynshen-bearer.{token}")
            .parse()
            .unwrap(),
    );
    let (mut socket, response) = tungstenite::connect(request).map_err(|e| e.to_string())?;
    assert_eq!(
        response.headers()["sec-websocket-protocol"],
        "lynshen-relay"
    );
    read_timeout(&socket, Duration::from_secs(130));
    let mut initiator = noise::initiator(phone_key, host_key).unwrap();
    let mut buffer = vec![0u8; noise::MAX_MESSAGE];
    let written = initiator
        .write_message(hello.to_string().as_bytes(), &mut buffer)
        .unwrap();
    socket
        .send(Message::binary(buffer[..written].to_vec()))
        .unwrap();
    let reply = match socket.read() {
        Ok(Message::Binary(bytes)) => bytes,
        Ok(Message::Close(frame)) => return Err(format!("closed {frame:?}")),
        other => return Err(format!("unexpected {other:?}")),
    };
    let read = initiator
        .read_message(&reply, &mut buffer)
        .map_err(|e| e.to_string())?;
    let payload: Value = serde_json::from_slice(&buffer[..read]).unwrap();
    if payload["ok"] != true {
        return Ok((payload, None));
    }
    let transport = noise::Transport::new(initiator).unwrap();
    read_timeout(&socket, Duration::from_secs(10));
    Ok((payload, Some((transport, socket))))
}

fn phone_request(transport: &mut noise::Transport, socket: &mut Socket, op: Value) -> Value {
    let id = op["id"].clone();
    for chunk in transport.seal(op.to_string().as_bytes()).unwrap() {
        socket.send(Message::binary(chunk)).unwrap();
    }
    loop {
        let message = match socket.read().expect("reply") {
            Message::Binary(bytes) => bytes,
            _ => continue,
        };
        let Some(frame) = transport.open(&message).unwrap() else {
            continue;
        };
        let frame: Value = serde_json::from_slice(&frame).unwrap();
        if frame["id"] == id {
            return frame;
        }
    }
}

fn check(label: &str, ok: bool, detail: impl std::fmt::Display) {
    println!(
        "{} {label}{}",
        if ok { "PASS" } else { "FAIL" },
        if ok {
            String::new()
        } else {
            format!(": {detail}")
        }
    );
    if !ok {
        std::process::exit(1);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (base, relay, daemon_addr) = (&args[1], &args[2], &args[3]);
    let read = |path: &str| std::fs::read_to_string(path).unwrap().trim().to_string();
    let (daemon_token, phone_device_key, other_device_key) =
        (read(&args[4]), read(&args[5]), read(&args[6]));

    // The desktop sees the daemon registered on the account over v2.
    let mut desk = desktop(daemon_addr, &daemon_token);
    desk.send(Message::text(
        json!({ "op": "relay_status", "id": 1 }).to_string(),
    ))
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match next_text(&mut desk, deadline) {
            Some(frame) if frame["id"] == 1 => break frame,
            Some(_) => {}
            None => panic!("no relay_status"),
        }
    };
    check(
        "daemon on relay v2, signed in",
        status["mode"] == "v2" && status["signed_in"] == true && status["connected"] == true,
        &status,
    );

    // The phone lists the account's computers (gateway AC-API-3).
    let computers = gateway(
        base,
        "GET",
        "/api/desktop/oauth/computers",
        &phone_device_key,
    );
    let computer = computers["computers"]
        .as_array()
        .and_then(|list| list.iter().find(|c| c["host_id"] == status["host"]))
        .cloned()
        .unwrap_or_default();
    check(
        "phone sees the computer, online, with its Noise key",
        computer["online"] == true && computer["static_pub"].as_str().is_some(),
        &computers,
    );
    let host = computer["host_id"].as_str().unwrap().to_string();
    let host_key = URL_SAFE_NO_PAD
        .decode(computer["static_pub"].as_str().unwrap())
        .unwrap();

    // A connect request: the desktop gets it with a check code and allows.
    let (phone_key, _) = noise::generate_keypair().unwrap();
    let token = gateway(
        base,
        "POST",
        "/api/desktop/oauth/relay-token",
        &phone_device_key,
    )["token"]
        .as_str()
        .unwrap()
        .to_string();
    let answer = std::thread::spawn({
        let daemon_addr = daemon_addr.clone();
        let daemon_token = daemon_token.clone();
        move || {
            let mut desk = desktop(&daemon_addr, &daemon_token);
            let deadline = Instant::now() + Duration::from_secs(30);
            while let Some(frame) = next_text(&mut desk, deadline) {
                if frame["type"] == "connect_request" {
                    desk.send(Message::text(
                        json!({ "op": "connect_decide", "request": frame["request"], "allow": true, "id": 2 })
                            .to_string(),
                    ))
                    .unwrap();
                    return frame;
                }
            }
            Value::Null
        }
    });
    let started = Instant::now();
    let (reply, link) = phone_connect(
        relay,
        &host,
        &token,
        &phone_key,
        &host_key,
        json!({ "name": "e2e iPhone", "request": true }),
    )
    .unwrap();
    let request = answer.join().unwrap();
    check(
        "desktop got connect_request (name, client, 6-digit check)",
        request["name"] == "e2e iPhone"
            && request["client"] == "aogiri-phone"
            && request["check"].as_str().is_some_and(|c| c.len() == 6),
        &request,
    );
    check("allowed: msg 2 ok", reply["ok"] == true, &reply);
    println!(
        "     (request → allowed in {} ms)",
        started.elapsed().as_millis()
    );
    let (mut transport, mut socket) = link.unwrap();

    // The phone gets hello and lists the sessions.
    let hello = loop {
        let Message::Binary(message) = socket.read().unwrap() else {
            continue;
        };
        if let Some(frame) = transport.open(&message).unwrap() {
            break serde_json::from_slice::<Value>(&frame).unwrap();
        }
    };
    check("phone gets hello", hello["type"] == "hello", &hello);
    let sessions = phone_request(
        &mut transport,
        &mut socket,
        json!({ "op": "session_list", "id": 7 }),
    );
    check(
        "phone lists the sessions",
        sessions["type"] == "sessions" && sessions["sessions"].is_array(),
        &sessions,
    );
    let me = phone_request(
        &mut transport,
        &mut socket,
        json!({ "op": "device_me", "id": 8 }),
    );
    check(
        "the device has no elevated scope until granted",
        me["scopes"] == json!([]),
        &me,
    );
    drop(socket);

    // The same phone reconnects without asking.
    let token = gateway(
        base,
        "POST",
        "/api/desktop/oauth/relay-token",
        &phone_device_key,
    )["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (reply, _) = phone_connect(
        relay,
        &host,
        &token,
        &phone_key,
        &host_key,
        json!({ "name": "e2e iPhone" }),
    )
    .unwrap();
    check(
        "paired phone reconnects without a request",
        reply["ok"] == true,
        &reply,
    );

    // Another account: the relay closes with 4403 before any handshake.
    let other_token = gateway(
        base,
        "POST",
        "/api/desktop/oauth/relay-token",
        &other_device_key,
    )["token"]
        .as_str()
        .unwrap()
        .to_string();
    let other = phone_connect(
        relay,
        &host,
        &other_token,
        &phone_key,
        &host_key,
        json!({ "request": true }),
    );
    let other = other.map(|(reply, _)| reply);
    check(
        "another account is refused (4403)",
        other.as_ref().is_err_and(|e| e.contains("4403")),
        format!("{other:?}"),
    );

    // A stranger of the same account without a code or request: not paired.
    let (stranger, _) = noise::generate_keypair().unwrap();
    let (reply, _) = phone_connect(
        relay,
        &host,
        &token,
        &stranger,
        &host_key,
        json!({ "name": "x" }),
    )
    .unwrap();
    check(
        "an unknown device without a request is refused",
        reply["error"] == "not paired",
        &reply,
    );
    // Revocation: the phone removes its own login on the account; within a
    // heartbeat (60 s) the relay drops its open stream with 4403.
    let token = gateway(
        base,
        "POST",
        "/api/desktop/oauth/relay-token",
        &phone_device_key,
    )["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, link) = phone_connect(
        relay,
        &host,
        &token,
        &phone_key,
        &host_key,
        json!({ "name": "e2e iPhone" }),
    )
    .unwrap();
    let (_, mut socket) = link.unwrap();
    let devices = gateway(base, "GET", "/api/desktop/oauth/devices", &phone_device_key);
    let current = devices["current_api_key_id"].clone();
    let own = devices["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["api_key_id"] == current)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    // A device cannot remove itself through the device API; the account's
    // computer key does it (as the console's 授权设备 page would).
    let computer_key = std::fs::read_to_string(&args[7])
        .unwrap()
        .trim()
        .to_string();
    let removed = gateway(
        base,
        "DELETE",
        &format!("/api/desktop/oauth/devices/{own}"),
        &computer_key,
    );
    check(
        "the phone's login is removed on the account",
        removed["devices"].is_array(),
        &removed,
    );
    let started = Instant::now();
    read_timeout(&socket, Duration::from_secs(90));
    let code = loop {
        match socket.read() {
            Ok(_) => continue,
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                break None
            }
            Err(tungstenite::Error::Protocol(_)) => break None,
            Err(error) => {
                break Some(format!("{error}"));
            }
        }
    };
    let _ = code;
    let elapsed = started.elapsed();
    check(
        "the revoked phone's open stream is dropped within a heartbeat",
        elapsed < Duration::from_secs(75),
        format!("{} s", elapsed.as_secs()),
    );
    println!("     (dropped after {} s)", elapsed.as_secs());
    let refused = gateway(
        base,
        "POST",
        "/api/desktop/oauth/relay-token",
        &phone_device_key,
    );
    check(
        "the revoked phone gets no new relay token",
        refused["status"] == 401,
        &refused,
    );
    println!("ALL PASS");
}
