use super::stream::Stream;
use super::{Error, Url};
use base64;
use rand::Rng;
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::str;
use tokio;
use tokio::io::{AsyncRead, AsyncWrite};

// Accepts http upgrade requests.
// Parses http headers. Checks weather it is valid WebSocket upgrade request.
// Responds to client with http upgrade response.
pub async fn accept<R, W>(mut stream: Stream<R, W>) -> Result<(Stream<R, W>, bool, HashMap<String, String>), Error>
where
    R: AsyncRead + std::marker::Unpin,
    W: AsyncWrite + std::marker::Unpin,
{
    let lines = stream.rh.http_header().await?;
    let header = Header::from_lines(&lines);
    if header.is_valid_upgrade() {
        stream.wh.write(header.upgrade_response().as_bytes()).await?;
        return Ok((stream, header.is_deflate_supported(), header.lines));
    }
    const BAD_REQUEST_HTTP_RESPONSE: &[u8] = "HTTP/1.1 400 Bad Request\r\n\r\n".as_bytes();
    stream.wh.write(BAD_REQUEST_HTTP_RESPONSE).await?;
    Err(Error::InvalidUpgradeRequest)
}

// Connects to the WebSocket server.
// It will send http upgrade request, wait for response and check whether
// upgrade request is accepted.
pub async fn connect<R, W>(
    mut stream: Stream<R, W>,
    url: &Url,
    headers: Option<HashMap<String, String>>,
) -> Result<(Stream<R, W>, bool, HashMap<String, String>), Error>
where
    R: AsyncRead + std::marker::Unpin,
    W: AsyncWrite + std::marker::Unpin,
{
    let key = connect_key();
    stream
        .wh
        .write(connect_header(&url.addr, &url.path, &key, headers).as_bytes())
        .await?;

    let lines = stream.rh.http_header().await?;
    let header = Header::from_lines(&lines);
    if header.is_valid_connect(&key) {
        return Ok((stream, header.is_deflate_supported(), header.lines));
    }
    Err(Error::InvalidUpgradeRequest)
}

#[derive(Debug)]
struct Header {
    connection: String,
    upgrade: String,
    version: String,
    key: String,
    extensions: String,
    accept: String,
    lines: HashMap<String, String>,
}

impl Header {
    fn new() -> Header {
        Header {
            connection: String::new(),
            upgrade: String::new(),
            version: String::new(),
            key: String::new(),
            extensions: String::new(),
            accept: String::new(),
            lines: HashMap::new(),
        }
    }

    fn from_lines(lines: &Vec<String>) -> Self {
        let mut header = Header::new();
        for line in lines {
            header.append(&line);
        }
        header
    }

    fn append(&mut self, line: &str) {
        if let Some((key, value)) = split_header_line(&line) {
            self.lines.insert(key.to_owned(), value.to_owned());
            match key.to_lowercase().as_str() {
                "connection" => self.connection = value.to_lowercase(),
                "upgrade" => self.upgrade = value.to_lowercase(),
                "sec-websocket-version" => self.version = value.to_string(),
                "sec-websocket-key" => self.key = value.to_string(),
                "sec-websocket-extensions" => self.add_extensions(value),
                "sec-websocket-accept" => self.accept = value.to_string(),
                _ => (),
            }
        }
    }

    fn add_extensions(&mut self, ex: &str) {
        if !self.extensions.is_empty() {
            self.extensions.push_str(", ");
        }
        self.extensions.push_str(ex);
    }

    fn is_deflate_supported(&self) -> bool {
        self.extensions.contains("permessage-deflate")
    }

    fn upgrade_response(&self) -> String {
        const HEADER: &str = "HTTP/1.1 101 Switching Protocols\r\n\
            Upgrade: websocket\r\n\
            Server: yarws\r\n\
            Connection: Upgrade\r\n\
            Sec-WebSocket-Accept: ";
        let mut s = HEADER.to_string();
        s.push_str(&ws_accept(&self.key));
        s.push_str(&"\r\n");
        if self.is_deflate_supported() {
            s.push_str(
                "Sec-WebSocket-Extensions: permessage-deflate;client_no_context_takeover;server_no_context_takeover",
            );
            s.push_str(&"\r\n");
        }
        s.push_str(&"\r\n");
        s
    }

    fn is_valid_upgrade(&self) -> bool {
        self.connection == "upgrade" && self.upgrade == "websocket" && self.version == "13" && self.key.len() > 0
    }

    fn is_valid_connect(&self, key: &str) -> bool {
        let accept = ws_accept(key);
        self.connection == "upgrade" && self.upgrade == "websocket" && self.accept == accept
    }
}

fn split_header_line(line: &str) -> Option<(&str, &str)> {
    let mut splitter = line.splitn(2, ':');
    let key = splitter.next()?;
    let value = splitter.next()?;
    Some((key, value.trim()))
}

// Calculate accept header value from |Sec-WebSocket-Key|.
// Ref: https://tools.ietf.org/html/rfc6455
//
// The server would append the string "258EAFA5-E914-47DA-95CA-C5AB0DC85B11" the
// value of the |Sec-WebSocket-Key| header field in the client's handshake. The
// server would then take the SHA-1 hash of this string. This value is then
// base64-encoded, to give the value which would be returned in the
// |Sec-WebSocket-Accept| header field.
fn ws_accept(key: &str) -> String {
    const WS_MAGIC_KEY: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let mut hasher = Sha1::new();
    let s = key.to_string() + WS_MAGIC_KEY;

    sha1::Digest::update(&mut hasher, s.as_bytes());
    let hr = hasher.finalize();
    base64::encode(&hr)
}

// Http header for client upgrade request to the WebSocket server.
fn connect_header(host: &str, path: &str, key: &str, headers: Option<HashMap<String, String>>) -> String {
    let mut h = "GET ".to_owned()
        + path
        + " HTTP/1.1\r\n\
Connection: Upgrade\r\n\
Upgrade: websocket\r\n\
Sec-WebSocket-Version: 13\r\n\
Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits\r\n\
Sec-WebSocket-Key: ";
    h.push_str(key);
    h.push_str("\r\n");
    h.push_str("Host: ");
    h.push_str(host);
    h.push_str("\r\n");
    if let Some(headers) = headers {
        for (key, value) in headers.iter() {
            h.push_str(key);
            h.push_str(": ");
            h.push_str(value);
            h.push_str("\r\n");
        }
    }
    h.push_str("\r\n");
    h
}

// Creates random key for |Sec-WebSocket-Key| http header used in client
// connections.
fn connect_key() -> String {
    let buf = rand::thread_rng().gen::<[u8; 16]>();
    base64::encode(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sha1_general() {
        let mut hasher = Sha1::new();
        sha1::Digest::update(&mut hasher, b"hello world");
        let result = hasher.finalize();
        assert_eq!(result[..], hex!("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed"));
    }
    #[test]
    fn test_ws_accept() {
        let acc = ws_accept("dGhlIHNhbXBsZSBub25jZQ==");
        assert_eq!(acc, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn test_connect_header() {
        let k = connect_key();
        assert_eq!(24, k.len());
        let ch = connect_header("minus5.hr", "/ws", "mRfknYOIooirQK3OuKf54A==", None);
        assert_eq!(
            ch,
            "GET /ws HTTP/1.1\r\n\
Connection: Upgrade\r\n\
Upgrade: websocket\r\n\
Sec-WebSocket-Version: 13\r\n\
Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits\r\n\
Sec-WebSocket-Key: mRfknYOIooirQK3OuKf54A==\r\n\
Host: minus5.hr\r\n\r\n"
        );

        let mut headers: HashMap<String, String> = HashMap::new();
        headers.insert("Server".to_owned(), "yarws".to_owned());
        let ch = connect_header("minus5.hr", "/ws", "mRfknYOIooirQK3OuKf54A==", Some(headers));
        assert_eq!(
            ch,
            "GET /ws HTTP/1.1\r\n\
Connection: Upgrade\r\n\
Upgrade: websocket\r\n\
Sec-WebSocket-Version: 13\r\n\
Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits\r\n\
Sec-WebSocket-Key: mRfknYOIooirQK3OuKf54A==\r\n\
Host: minus5.hr\r\n\
Server: yarws\r\n\r\n"
        );
    }

    #[test]
    fn test_parse_header() {
        test_parse_header_asserts(
            "GET /chat HTTP/1.1
Upgrade: websocket
Connection: Upgrade
Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==
Sec-WebSocket-Version: 13",
        );
        test_parse_header_asserts(
            "GET /chat HTTP/1.1
upgrade: websocket
coNNection: Upgrade
sec-webSocket-key: dGhlIHNhbXBsZSBub25jZQ==
sec-WEBSocket-VerSion: 13",
        );
    }

    fn test_parse_header_asserts(req: &str) {
        let mut header = Header::new();
        for line in req.lines() {
            header.append(line);
        }
        assert!(header.is_valid_upgrade());
        assert_eq!(header.connection, "upgrade");
        assert_eq!(header.upgrade, "websocket");
        assert_eq!(header.key, "dGhlIHNhbXBsZSBub25jZQ==");
        assert_eq!(header.version, "13");
    }
}
