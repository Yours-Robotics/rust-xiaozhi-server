use std::{
    collections::HashMap,
    io::{self, BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Default)]
struct AppState {
    sessions: Arc<Mutex<HashMap<String, DeviceSession>>>,
}

#[derive(Clone)]
struct DeviceSession {
    device_id: String,
    created_at_unix: u64,
}

impl AppState {
    fn create_session(&self, device_id: String) -> String {
        let session_id = gen_id();
        let created_at_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs());

        self.sessions.lock().expect("state lock").insert(
            session_id.clone(),
            DeviceSession {
                device_id,
                created_at_unix,
            },
        );

        session_id
    }

    fn remove_session(&self, session_id: &str) {
        self.sessions.lock().expect("state lock").remove(session_id);
    }

    fn session_count(&self) -> usize {
        self.sessions.lock().expect("state lock").len()
    }

    fn get_session(&self, session_id: &str) -> Option<DeviceSession> {
        self.sessions
            .lock()
            .expect("state lock")
            .get(session_id)
            .cloned()
    }
}

struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

pub fn run(bind: &str) -> io::Result<()> {
    let listener = TcpListener::bind(bind)?;
    let state = AppState::default();

    println!("rust-xiaozhi-server listening on {bind}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = state.clone();
                thread::spawn(move || {
                    if let Err(error) = handle_connection(stream, state) {
                        eprintln!("connection error: {error}");
                    }
                });
            }
            Err(error) => eprintln!("accept error: {error}"),
        }
    }

    Ok(())
}

fn handle_connection(mut stream: TcpStream, state: AppState) -> io::Result<()> {
    let request = match read_http_request(stream.try_clone()?) {
        Ok(request) => request,
        Err(_) => return Ok(()),
    };

    if request.method == "GET" && request.path.starts_with("/ws/") {
        let session_id = request.path.trim_start_matches("/ws/").to_string();
        return handle_websocket_upgrade(&mut stream, &state, &session_id, &request.headers);
    }

    let response = route_http_request(&request, &state);
    stream.write_all(response.as_bytes())
}

fn read_http_request(stream: TcpStream) -> io::Result<HttpRequest> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    if request_line.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "empty request",
        ));
    }

    let (method, path) = parse_request_line(&request_line);
    let mut headers = HashMap::new();
    let mut content_length = 0usize;

    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        if header == "\r\n" || header.is_empty() {
            break;
        }

        if let Some((name, value)) = header.split_once(':') {
            let key = name.trim().to_ascii_lowercase();
            let val = value.trim().to_string();
            if key == "content-length" {
                content_length = val.parse::<usize>().unwrap_or(0);
            }
            headers.insert(key, val);
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    Ok(HttpRequest {
        method: method.to_string(),
        path: path.to_string(),
        headers,
        body,
    })
}

fn parse_request_line(line: &str) -> (&str, &str) {
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let path = parts.next().unwrap_or("/");
    (method, path)
}

fn route_http_request(request: &HttpRequest, state: &AppState) -> String {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/health") => {
            let payload = format!(
                "{{\"status\":\"ok\",\"active_sessions\":{}}}",
                state.session_count()
            );
            http_response(200, &payload)
        }
        ("POST", "/api/v1/device/register") => {
            let body_text = String::from_utf8_lossy(&request.body);
            match parse_device_id(&body_text) {
                Some(device_id) if !device_id.trim().is_empty() => {
                    let session_id = state.create_session(device_id);
                    let payload = format!(
                        "{{\"session_id\":\"{}\",\"websocket_url\":\"/ws/{}\",\"ice_servers\":[{{\"urls\":[\"stun:stun.l.google.com:19302\"]}}]}}",
                        session_id, session_id
                    );
                    http_response(200, &payload)
                }
                _ => http_response(400, "{\"error\":\"device_id is required\"}"),
            }
        }
        _ => http_response(404, "{\"error\":\"not found\"}"),
    }
}

fn handle_websocket_upgrade(
    stream: &mut TcpStream,
    state: &AppState,
    session_id: &str,
    headers: &HashMap<String, String>,
) -> io::Result<()> {
    if state.get_session(session_id).is_none() {
        return stream
            .write_all(http_response(404, "{\"error\":\"session not found\"}").as_bytes());
    }

    let connection = headers
        .get("connection")
        .map(String::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let upgrade = headers
        .get("upgrade")
        .map(String::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let sec_key = headers.get("sec-websocket-key");

    if !connection.contains("upgrade") || upgrade != "websocket" || sec_key.is_none() {
        let response =
            "HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        return stream.write_all(response.as_bytes());
    }

    let accept_value = websocket_accept_key(sec_key.expect("checked above"));
    let handshake = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        accept_value
    );
    stream.write_all(handshake.as_bytes())?;

    websocket_loop(stream, state, session_id)
}

fn websocket_loop(stream: &mut TcpStream, state: &AppState, session_id: &str) -> io::Result<()> {
    let session = state.get_session(session_id);

    while let Ok(frame) = read_ws_frame(stream) {
        match frame.opcode {
            0x1 => {
                let incoming = String::from_utf8_lossy(&frame.payload);
                let reply = if incoming.contains("\"type\":\"ping\"") {
                    "{\"type\":\"pong\"}".to_string()
                } else if let Some(session) = &session {
                    format!(
                        "{{\"type\":\"ack\",\"device_id\":\"{}\",\"created_at_unix\":{}}}",
                        session.device_id, session.created_at_unix
                    )
                } else {
                    "{\"type\":\"ack\"}".to_string()
                };
                write_ws_text(stream, &reply)?;
            }
            0x8 => {
                write_ws_close(stream)?;
                break;
            }
            0x9 => {
                write_ws_frame(stream, 0xA, &frame.payload)?;
            }
            _ => {}
        }
    }

    state.remove_session(session_id);
    Ok(())
}

struct WsFrame {
    opcode: u8,
    payload: Vec<u8>,
}

fn read_ws_frame(stream: &mut TcpStream) -> io::Result<WsFrame> {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head)?;

    let opcode = head[0] & 0x0f;
    let masked = (head[1] & 0x80) != 0;
    let mut payload_len = (head[1] & 0x7f) as u64;

    if payload_len == 126 {
        let mut ext = [0u8; 2];
        stream.read_exact(&mut ext)?;
        payload_len = u16::from_be_bytes(ext) as u64;
    } else if payload_len == 127 {
        let mut ext = [0u8; 8];
        stream.read_exact(&mut ext)?;
        payload_len = u64::from_be_bytes(ext);
    }

    let mut masking_key = [0u8; 4];
    if masked {
        stream.read_exact(&mut masking_key)?;
    }

    let mut payload = vec![0u8; payload_len as usize];
    if payload_len > 0 {
        stream.read_exact(&mut payload)?;
    }

    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= masking_key[index % 4];
        }
    }

    Ok(WsFrame { opcode, payload })
}

fn write_ws_text(stream: &mut TcpStream, text: &str) -> io::Result<()> {
    write_ws_frame(stream, 0x1, text.as_bytes())
}

fn write_ws_close(stream: &mut TcpStream) -> io::Result<()> {
    write_ws_frame(stream, 0x8, &[])
}

fn write_ws_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) -> io::Result<()> {
    let mut frame = vec![0x80 | opcode];
    let len = payload.len();
    if len <= 125 {
        frame.push(len as u8);
    } else if len <= u16::MAX as usize {
        frame.push(126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }

    frame.extend_from_slice(payload);
    stream.write_all(&frame)
}

fn parse_device_id(body: &str) -> Option<String> {
    let key = "\"device_id\"";
    let index = body.find(key)?;
    let value_part = body.get(index + key.len()..)?;
    let colon = value_part.find(':')?;
    let after_colon = value_part.get(colon + 1..)?.trim_start();
    if !after_colon.starts_with('"') {
        return None;
    }

    let rest = after_colon.get(1..)?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn http_response(status: u16, body: &str) -> String {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Internal Server Error",
    };

    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

fn gen_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("{:032x}", nanos)
}

fn websocket_accept_key(sec_websocket_key: &str) -> String {
    let mut input = Vec::with_capacity(sec_websocket_key.len() + 36);
    input.extend_from_slice(sec_websocket_key.as_bytes());
    input.extend_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64_encode(&sha1_digest(&input))
}

fn base64_encode(data: &[u8]) -> String {
    let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();

    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);

        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
        output.push(table[((n >> 18) & 0x3f) as usize] as char);
        output.push(table[((n >> 12) & 0x3f) as usize] as char);

        if chunk.len() > 1 {
            output.push(table[((n >> 6) & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }

        if chunk.len() > 2 {
            output.push(table[(n & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }
    }

    output
}

fn sha1_digest(data: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x67452301;
    let mut h1: u32 = 0xEFCDAB89;
    let mut h2: u32 = 0x98BADCFE;
    let mut h3: u32 = 0x10325476;
    let mut h4: u32 = 0xC3D2E1F0;

    let mut message = data.to_vec();
    let bit_len = (message.len() as u64) * 8;

    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks(64) {
        let mut w = [0u32; 80];
        for (index, word) in w.iter_mut().take(16).enumerate() {
            let start = index * 4;
            *word = u32::from_be_bytes([
                chunk[start],
                chunk[start + 1],
                chunk[start + 2],
                chunk[start + 3],
            ]);
        }
        for index in 16..80 {
            w[index] = (w[index - 3] ^ w[index - 8] ^ w[index - 14] ^ w[index - 16]).rotate_left(1);
        }

        let mut a = h0;
        let mut b = h1;
        let mut c = h2;
        let mut d = h3;
        let mut e = h4;

        for (index, word) in w.iter().enumerate() {
            let (f, k) = if index < 20 {
                ((b & c) | ((!b) & d), 0x5A827999)
            } else if index < 40 {
                (b ^ c ^ d, 0x6ED9EBA1)
            } else if index < 60 {
                ((b & c) | (b & d) | (c & d), 0x8F1BBCDC)
            } else {
                (b ^ c ^ d, 0xCA62C1D6)
            };

            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut out = [0u8; 20];
    out[..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::{
        base64_encode, parse_device_id, route_http_request, sha1_digest, websocket_accept_key,
        AppState, HttpRequest,
    };
    use std::collections::HashMap;

    #[test]
    fn parse_device_id_works() {
        assert_eq!(
            parse_device_id(r#"{"device_id":"esp32-01"}"#).as_deref(),
            Some("esp32-01")
        );
    }

    #[test]
    fn register_then_health_increases_sessions() {
        let state = AppState::default();
        let register = HttpRequest {
            method: "POST".to_string(),
            path: "/api/v1/device/register".to_string(),
            headers: HashMap::new(),
            body: br#"{"device_id":"abc"}"#.to_vec(),
        };
        let response = route_http_request(&register, &state);
        assert!(response.contains("HTTP/1.1 200"));

        let health = HttpRequest {
            method: "GET".to_string(),
            path: "/health".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let health_resp = route_http_request(&health, &state);
        assert!(health_resp.contains("\"active_sessions\":1"));
    }

    #[test]
    fn websocket_accept_key_matches_rfc_example() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        assert_eq!(websocket_accept_key(key), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn sha1_and_base64_smoke_test() {
        let digest = sha1_digest(b"abc");
        assert_eq!(
            hex::encode(digest),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
    }

    mod hex {
        pub fn encode(bytes: [u8; 20]) -> String {
            let mut out = String::with_capacity(40);
            for byte in bytes {
                out.push_str(&format!("{byte:02x}"));
            }
            out
        }
    }
}
