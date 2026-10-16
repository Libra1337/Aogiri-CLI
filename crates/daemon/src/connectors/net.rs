//! Network pieces the platforms share: a WebSocket to a gateway (Discord,
//! QQ) through the connector's HTTP proxy, which `ureq` covers for plain
//! HTTPS requests but `tungstenite` does not.

use base64::{engine::general_purpose::STANDARD, Engine};
use std::{
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};
use tungstenite::{client::IntoClientRequest, stream::MaybeTlsStream, WebSocket};

pub(crate) type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

/// How long connecting, the proxy's answer, TLS and the upgrade may take.
const CONNECT: Duration = Duration::from_secs(15);

/// Opens `url` (`wss://` or `ws://`), through `proxy` (an `http://` URL)
/// with an HTTP CONNECT tunnel when one is given. Every step is bounded.
pub(crate) fn connect(url: &str, proxy: Option<&str>) -> Result<Socket, String> {
    let request = url
        .into_client_request()
        .map_err(|error| error.to_string())?;
    let uri = request.uri();
    let host = uri
        .host()
        .ok_or("a gateway URL without a host")?
        .to_string();
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("wss") {
            443
        } else {
            80
        });
    let tcp = match proxy {
        None => dial(&host, port)?,
        Some(proxy) => tunnel(proxy, &host, port)?,
    };
    tcp.set_read_timeout(Some(CONNECT))
        .and_then(|()| tcp.set_write_timeout(Some(CONNECT)))
        .map_err(|error| error.to_string())?;
    let (socket, _) = tungstenite::client_tls(request, tcp).map_err(|error| error.to_string())?;
    Ok(socket)
}

fn dial(host: &str, port: u16) -> Result<TcpStream, String> {
    let mut last = format!("could not resolve {host}");
    for addr in (host, port)
        .to_socket_addrs()
        .map_err(|error| format!("{host}: {error}"))?
    {
        match TcpStream::connect_timeout(&addr, CONNECT) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = format!("{host}:{port}: {error}"),
        }
    }
    Err(last)
}

/// A TCP stream to `host:port` through an HTTP proxy's CONNECT.
fn tunnel(proxy: &str, host: &str, port: u16) -> Result<TcpStream, String> {
    let (credentials, address) = parse_proxy(proxy)?;
    let (proxy_host, proxy_port) = address;
    let mut tcp = dial(&proxy_host, proxy_port)
        .map_err(|error| format!("the proxy {}: {error}", super::shown_proxy(proxy)))?;
    tcp.set_read_timeout(Some(CONNECT))
        .and_then(|()| tcp.set_write_timeout(Some(CONNECT)))
        .map_err(|error| error.to_string())?;
    let mut head = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    if let Some(credentials) = credentials {
        head.push_str(&format!(
            "Proxy-Authorization: Basic {}\r\n",
            STANDARD.encode(credentials)
        ));
    }
    head.push_str("\r\n");
    tcp.write_all(head.as_bytes())
        .map_err(|error| error.to_string())?;
    // The answer's head, byte by byte: nothing of the tunnel is read.
    let mut answer = Vec::new();
    let mut byte = [0u8; 1];
    while !answer.ends_with(b"\r\n\r\n") {
        if answer.len() > 8192 {
            return Err("the proxy sent an overlong answer".to_string());
        }
        match tcp.read(&mut byte) {
            Ok(0) => return Err("the proxy closed the connection".to_string()),
            Ok(_) => answer.push(byte[0]),
            Err(error) => return Err(format!("the proxy did not answer: {error}")),
        }
    }
    let status = String::from_utf8_lossy(&answer);
    let line = status.lines().next().unwrap_or_default();
    if line.split_whitespace().nth(1) != Some("200") {
        return Err(format!("the proxy refused the tunnel: {line}"));
    }
    Ok(tcp)
}

/// `http://[user:pass@]host[:port]` → credentials and address.
type ProxyParts = (Option<String>, (String, u16));

fn parse_proxy(proxy: &str) -> Result<ProxyParts, String> {
    let rest = proxy
        .strip_prefix("http://")
        .ok_or("the proxy must be an http:// URL")?
        .trim_end_matches('/');
    let (credentials, address) = match rest.rsplit_once('@') {
        Some((credentials, address)) => (Some(credentials.to_string()), address),
        None => (None, rest),
    };
    let (host, port) = match address.rsplit_once(':') {
        Some((host, port)) => (
            host.to_string(),
            port.parse()
                .map_err(|_| format!("the proxy port {port} is not a number"))?,
        ),
        None => (address.to_string(), 80),
    };
    if host.is_empty() {
        return Err("the proxy URL has no host".to_string());
    }
    Ok((
        credentials,
        (host.trim_matches(['[', ']']).to_string(), port),
    ))
}

/// Short reads on an open socket, so one thread both reads and keeps the
/// heartbeat.
pub(crate) fn set_read_timeout(socket: &Socket, timeout: Duration) -> Result<(), String> {
    let tcp = match socket.get_ref() {
        MaybeTlsStream::Plain(tcp) => tcp,
        MaybeTlsStream::Rustls(tls) => tls.get_ref(),
        _ => return Err("unsupported gateway stream".to_string()),
    };
    tcp.set_read_timeout(Some(timeout))
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::BufRead, net::TcpListener, thread};

    #[test]
    fn proxy_urls_are_taken_apart() {
        assert_eq!(
            parse_proxy("http://127.0.0.1:7890/").unwrap(),
            (None, ("127.0.0.1".to_string(), 7890))
        );
        assert_eq!(
            parse_proxy("http://u:p@proxy.lan").unwrap(),
            (Some("u:p".to_string()), ("proxy.lan".to_string(), 80))
        );
        assert!(parse_proxy("socks5://127.0.0.1:1080").is_err());
        assert!(parse_proxy("http://host:port").is_err());
    }

    /// A proxy that tunnels one connection to `target` after checking the
    /// CONNECT line and the credentials.
    fn proxy(target: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (client, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(client.try_clone().unwrap());
            let mut head = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            let mut client = client;
            if !head.starts_with(&format!("CONNECT {target} HTTP/1.1"))
                || !head.contains("Proxy-Authorization: Basic dTpw")
            {
                client
                    .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
                    .unwrap();
                return;
            }
            let upstream = TcpStream::connect(&target).unwrap();
            client
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .unwrap();
            let (mut a, mut b) = (client.try_clone().unwrap(), upstream.try_clone().unwrap());
            thread::spawn(move || {
                let _ = std::io::copy(&mut a, &mut b);
            });
            let (mut c, mut d) = (upstream, client);
            let _ = std::io::copy(&mut c, &mut d);
        });
        format!("http://u:p@{address}")
    }

    #[test]
    fn a_websocket_goes_through_the_proxy_tunnel() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = listener.local_addr().unwrap().to_string();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            socket
                .send(tungstenite::Message::text("hello through the tunnel"))
                .unwrap();
        });
        let mut socket = connect(&format!("ws://{target}/"), Some(&proxy(target.clone()))).unwrap();
        let message = socket.read().unwrap();
        assert_eq!(message.to_text().unwrap(), "hello through the tunnel");

        // Wrong credentials: the proxy refuses, and the error says so.
        let refused = connect(
            &format!("ws://{target}/"),
            Some(&proxy(target.clone()).replace("u:p@", "x:y@")),
        )
        .unwrap_err();
        assert!(refused.contains("407"), "{refused}");
    }
}
