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
    client_id: String,
    created_at_unix: u64,
    hello_done: bool,
}

impl AppState {
    fn create_session(&self, device_id: String, client_id: String) -> String {
        let session_id = gen_id();
        let created_at_unix = now_secs();

        self.sessions.lock().expect("state lock").insert(
            session_id.clone(),
            DeviceSession {
                device_id,
                client_id,
                created_at_unix,
                hello_done: false,
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

    fn find_by_device_client(&self, device_id: &str, client_id: &str) -> Option<String> {
        self.sessions
            .lock()
            .expect("state lock")
            .iter()
            .find(|(_, session)| session.device_id == device_id && session.client_id == client_id)
            .map(|(id, _)| id.clone())
    }

    fn get_session(&self, session_id: &str) -> Option<DeviceSession> {
        self.sessions
            .lock()
            .expect("state lock")
            .get(session_id)
            .cloned()
    }

    fn mark_hello_done(&self, session_id: &str) {
        if let Some(session) = self
            .sessions
            .lock()
            .expect("state lock")
            .get_mut(session_id)
        {
            session.hello_done = true;
        }
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

    if request.method == "GET" && is_websocket_path(&request.path) {
        return handle_websocket_upgrade(&mut stream, &state, &request);
    }

    let response = route_http_request(&request, &state);
    stream.write_all(response.as_bytes())
}

fn is_websocket_path(path: &str) -> bool {
    path == "/xiaozhi/v1/" || path.starts_with("/ws/")
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
                "{{\"status\":\"ok\",\"active_sessions\":{},\"protocol_version\":1,\"transport\":\"websocket\"}}",
                state.session_count()
            );
            http_response(200, &payload)
        }
        ("POST", "/api/v1/device/register") => {
            let body_text = String::from_utf8_lossy(&request.body);
            match parse_json_string(&body_text, "device_id") {
                Some(device_id) if !device_id.trim().is_empty() => {
                    let client_id = parse_json_string(&body_text, "client_id")
                        .unwrap_or_else(|| gen_id().chars().take(8).collect());
                    let session_id = state.create_session(device_id, client_id);
                    let payload = format!(
                        "{{\"session_id\":\"{}\",\"websocket_url\":\"/xiaozhi/v1/\",\"ice_servers\":[{{\"urls\":[\"stun:stun.l.google.com:19302\"]}}]}}",
                        session_id
                    );
                    http_response(200, &payload)
                }
                _ => http_response(400, "{\"error\":\"device_id is required\"}"),
            }
        }
        ("POST", "/api/ota/") | ("POST", "/xiaozhi/ota/") => handle_ota(request),
        _ => http_response(404, "{\"error\":\"not found\"}"),
    }
}

fn handle_ota(request: &HttpRequest) -> String {
    let device_id = header(request, "device-id");
    let client_id = header(request, "client-id");
    let user_agent = header(request, "user-agent");

    if device_id.is_none() {
        return http_response(400, "{\"error\":\"Device ID is required\"}");
    }
    if client_id.is_none() {
        return http_response(400, "{\"error\":\"Client ID is required\"}");
    }
    if user_agent.is_none() {
        return http_response(400, "{\"error\":\"User-Agent is required\"}");
    }

    let body = String::from_utf8_lossy(&request.body);
    let board_name = parse_nested_json_string(&body, "board", "name");
    let app_version = parse_nested_json_string(&body, "application", "version");

    if board_name.is_none() || app_version.is_none() {
        return http_response(400, "{\"error\":\"Invalid OTA request\"}");
    }

    let board_name = board_name.expect("checked above");
    let current_version = app_version.expect("checked above");
    let latest = select_firmware(&board_name, &current_version);

    let now_ms = now_secs() * 1000;
    let activation_code = gen_code(6);

    let payload = format!(
        "{{\"activation\":{{\"code\":\"{}\",\"message\":\"请在设备端输入激活码\"}},\"mqtt\":{{\"endpoint\":\"mqtt.example.com\",\"client_id\":\"GID_test@@@{}@@@{}\",\"username\":\"device_{}\",\"password\":\"password\",\"publish_topic\":\"device-server\"}},\"websocket\":{{\"url\":\"wss://api.tenclass.net/xiaozhi/v1/\",\"token\":\"test-token\"}},\"server_time\":{{\"timestamp\":{},\"timezone\":\"Asia/Shanghai\",\"timezone_offset\":-480}},\"firmware\":{{\"version\":\"{}\",\"url\":\"{}\"}}}}",
        activation_code,
        device_id.expect("checked above"),
        client_id.expect("checked above"),
        sanitize(device_id.expect("checked above")),
        now_ms,
        latest.version,
        latest.url
    );

    http_response(200, &payload)
}

struct FirmwareResult {
    version: String,
    url: String,
}

fn select_firmware(board_name: &str, current_version: &str) -> FirmwareResult {
    if board_name.starts_with("bread-") && current_version == "0.9.8" {
        return FirmwareResult {
            version: "1.0.2".to_string(),
            url: "https://example.com/firmware/1.0.2.bin".to_string(),
        };
    }

    let latest = "1.0.2";
    if version_lt(current_version, latest) {
        FirmwareResult {
            version: latest.to_string(),
            url: format!("https://example.com/firmware/{latest}.bin"),
        }
    } else {
        FirmwareResult {
            version: current_version.to_string(),
            url: String::new(),
        }
    }
}

fn version_lt(current: &str, latest: &str) -> bool {
    let parse = |v: &str| {
        v.split('.')
            .map(|x| x.parse::<u32>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    parse(current) < parse(latest)
}

fn handle_websocket_upgrade(
    stream: &mut TcpStream,
    state: &AppState,
    request: &HttpRequest,
) -> io::Result<()> {
    let validation = validate_ws_headers(&request.headers);
    if let Err(error_message) = validation {
        return stream.write_all(
            http_response(401, &format!("{{\"error\":\"{}\"}}", error_message)).as_bytes(),
        );
    }

    let (device_id, client_id, sec_key) = validation.expect("checked above");
    let session_id = if request.path.starts_with("/ws/") {
        request.path.trim_start_matches("/ws/").to_string()
    } else {
        state
            .find_by_device_client(&device_id, &client_id)
            .unwrap_or_else(|| state.create_session(device_id.clone(), client_id.clone()))
    };

    if state.get_session(&session_id).is_none() {
        return stream
            .write_all(http_response(404, "{\"error\":\"session not found\"}").as_bytes());
    }

    let accept_value = websocket_accept_key(&sec_key);
    let handshake = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        accept_value
    );
    stream.write_all(handshake.as_bytes())?;

    websocket_loop(stream, state, &session_id)
}

fn validate_ws_headers(
    headers: &HashMap<String, String>,
) -> Result<(String, String, String), &'static str> {
    let connection = header_map(headers, "connection")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let upgrade = header_map(headers, "upgrade")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !connection.contains("upgrade") || upgrade != "websocket" {
        return Err("invalid websocket upgrade headers");
    }

    let protocol_version = header_map(headers, "protocol-version").unwrap_or_default();
    if protocol_version != "1" {
        return Err("Protocol-Version must be 1");
    }

    let auth = header_map(headers, "authorization").unwrap_or_default();
    if !auth.starts_with("Bearer ") || auth.len() <= 7 {
        return Err("Authorization header is required");
    }

    let device_id = header_map(headers, "device-id").unwrap_or_default();
    if device_id.is_empty() {
        return Err("Device-Id header is required");
    }

    let client_id = header_map(headers, "client-id").unwrap_or_default();
    if client_id.is_empty() {
        return Err("Client-Id header is required");
    }

    let sec_key = header_map(headers, "sec-websocket-key").unwrap_or_default();
    if sec_key.is_empty() {
        return Err("Sec-WebSocket-Key header is required");
    }

    Ok((
        device_id.to_string(),
        client_id.to_string(),
        sec_key.to_string(),
    ))
}

fn websocket_loop(stream: &mut TcpStream, state: &AppState, session_id: &str) -> io::Result<()> {
    while let Ok(frame) = read_ws_frame(stream) {
        match frame.opcode {
            0x1 => {
                let incoming = String::from_utf8_lossy(&frame.payload);
                if let Some(reply) = handle_ws_text_message(&incoming, state, session_id) {
                    write_ws_text(stream, &reply)?;
                }
            }
            0x2 => {
                // binary OPUS frame: demo behavior is sending it back as tts binary
                write_ws_frame(stream, 0x2, &frame.payload)?;
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

fn handle_ws_text_message(text: &str, state: &AppState, session_id: &str) -> Option<String> {
    if text.contains("\"type\":\"hello\"") {
        state.mark_hello_done(session_id);
        return Some("{\"type\":\"hello\",\"transport\":\"websocket\",\"audio_params\":{\"format\":\"opus\",\"sample_rate\":24000,\"channels\":1,\"frame_duration\":60}}".to_string());
    }

    if text.contains("\"type\":\"listen\"") {
        if text.contains("\"state\":\"start\"") {
            return Some("{\"session_id\":\"\",\"type\":\"listen\",\"state\":\"detect\",\"text\":\"你好小智\"}".to_string());
        }
        if text.contains("\"state\":\"stop\"") {
            return Some("{\"type\":\"tts\",\"state\":\"stop\"}".to_string());
        }
    }

    if text.contains("\"type\":\"abort\"") {
        return Some("{\"type\":\"tts\",\"state\":\"stop\"}".to_string());
    }

    if text.contains("\"type\":\"mcp\"") {
        return Some(format!(
            "{{\"session_id\":\"\",\"type\":\"mcp\",\"payload\":{{\"echo\":{}}}}}",
            extract_payload(text)
        ));
    }

    if text.contains("\"type\":\"ping\"") {
        return Some("{\"type\":\"pong\"}".to_string());
    }

    if let Some(session) = state.get_session(session_id) {
        if session.hello_done {
            return Some(format!(
                "{{\"type\":\"llm\",\"emotion\":\"neutral\",\"session_created_at\":{}}}",
                session.created_at_unix
            ));
        }
    }

    Some("{\"error\":\"please send hello first\"}".to_string())
}

fn extract_payload(text: &str) -> &str {
    let key = "\"payload\":";
    if let Some(index) = text.find(key) {
        let start = index + key.len();
        return text
            .get(start..text.len().saturating_sub(1))
            .unwrap_or("{}");
    }
    "{}"
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

fn parse_json_string(body: &str, key_name: &str) -> Option<String> {
    let key = format!("\"{}\"", key_name);
    let index = body.find(&key)?;
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

fn parse_nested_json_string(body: &str, object_key: &str, field_key: &str) -> Option<String> {
    let object = format!("\"{}\"", object_key);
    let start = body.find(&object)?;
    let sliced = body.get(start..)?;
    parse_json_string(sliced, field_key)
}

fn http_response(status: u16, body: &str) -> String {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
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

fn gen_code(len: usize) -> String {
    gen_id().chars().take(len).collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn sanitize(s: &str) -> String {
    s.chars().filter(|ch| ch.is_ascii_alphanumeric()).collect()
}

fn header<'a>(request: &'a HttpRequest, key: &str) -> Option<&'a str> {
    request.headers.get(key).map(String::as_str)
}

fn header_map<'a>(headers: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    headers.get(key).map(String::as_str)
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
        base64_encode, handle_ota, handle_ws_text_message, parse_json_string, route_http_request,
        select_firmware, sha1_digest, validate_ws_headers, websocket_accept_key, AppState,
        HttpRequest,
    };
    use std::collections::HashMap;

    #[test]
    fn parse_device_id_works() {
        assert_eq!(
            parse_json_string(r#"{"device_id":"esp32-01"}"#, "device_id").as_deref(),
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

    #[test]
    fn websocket_headers_must_include_auth_protocol_ids() {
        let mut headers = HashMap::new();
        headers.insert("connection".to_string(), "Upgrade".to_string());
        headers.insert("upgrade".to_string(), "websocket".to_string());
        headers.insert("protocol-version".to_string(), "1".to_string());
        headers.insert("authorization".to_string(), "Bearer token".to_string());
        headers.insert("device-id".to_string(), "11:22:33".to_string());
        headers.insert("client-id".to_string(), "uuid-1".to_string());
        headers.insert("sec-websocket-key".to_string(), "abc".to_string());

        assert!(validate_ws_headers(&headers).is_ok());
    }

    #[test]
    fn ota_requires_headers_and_core_fields() {
        let req = HttpRequest {
            method: "POST".to_string(),
            path: "/api/ota/".to_string(),
            headers: HashMap::new(),
            body: br#"{}"#.to_vec(),
        };
        let bad = handle_ota(&req);
        assert!(bad.contains("Device ID is required"));

        let mut headers = HashMap::new();
        headers.insert("device-id".to_string(), "11:22".to_string());
        headers.insert("client-id".to_string(), "uuid".to_string());
        headers.insert("user-agent".to_string(), "bread/1.0.0".to_string());
        let good = HttpRequest {
            method: "POST".to_string(),
            path: "/api/ota/".to_string(),
            headers,
            body: br#"{"application":{"version":"0.9.8"},"board":{"name":"bread-compact-wifi-128x64"}}"#.to_vec(),
        };
        let ok = handle_ota(&good);
        assert!(ok.contains("\"firmware\""));
        assert!(ok.contains("1.0.2"));
    }

    #[test]
    fn firmware_selection_forces_bread_098() {
        let fw = select_firmware("bread-compact", "0.9.8");
        assert_eq!(fw.version, "1.0.2");
        assert!(!fw.url.is_empty());
    }

    #[test]
    fn ws_hello_message_returns_server_hello() {
        let state = AppState::default();
        let sid = state.create_session("d".to_string(), "c".to_string());
        let msg = handle_ws_text_message("{\"type\":\"hello\"}", &state, &sid)
            .expect("response should exist");
        assert!(msg.contains("\"type\":\"hello\""));
        assert!(msg.contains("\"sample_rate\":24000"));
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
