//! lsp-gleam — Gleam language-server bridge for Basalt.
//!
//! Wraps `gleam lsp` through the host LSP process imports:
//! `basalt_spawn_lsp`, `basalt_lsp_write`, `basalt_lsp_read`,
//! `basalt_lsp_stop`, and `basalt_sleep_ms`.
//!
//! `diagnose` opens/updates the document and converts the server's
//! `textDocument/publishDiagnostics` notification into byte-offset
//! diagnostics. `hover` issues `textDocument/hover` and flattens the
//! MarkupContent / MarkedString response into plain Markdown text.

#![allow(static_mut_refs)]

use std::collections::HashMap;

use basalt_plugin_sdk::prelude::*;
use serde_json::{json, Value};

basalt_plugin_meta! {
    name:              "lsp-gleam",
    version:           "0.1.0",
    hook_flags:        CAP_DIAGNOSTICS | CAP_HOVER,
    provides:          "lsp:gleam",
    requires:          "",
    file_globs:        "**/*.gleam",
    activates_on:      "gleam.toml\n**/gleam.toml\nmanifest.toml\n**/manifest.toml",
    activation_events: "",
}

#[cfg(target_arch = "wasm32")]
extern "C" {
    fn basalt_spawn_lsp(cmd_ptr: i32, cmd_len: i32, root_ptr: i32, root_len: i32) -> i32;
    fn basalt_lsp_write(handle: i32, buf_ptr: i32, buf_len: i32) -> i32;
    fn basalt_lsp_read(handle: i32, out_ptr: i32, out_cap: i32) -> i32;
    fn basalt_lsp_stop(handle: i32);
    fn basalt_sleep_ms(ms: i32);
    fn basalt_read_file(path_ptr: i32, path_len: i32, out_ptr: i32, out_cap: i32) -> i32;
}

#[cfg(not(target_arch = "wasm32"))]
#[no_mangle]
unsafe extern "C" fn basalt_spawn_lsp(_cmd: i32, _cmd_len: i32, _root: i32, _root_len: i32) -> i32 {
    -1
}

#[cfg(not(target_arch = "wasm32"))]
#[no_mangle]
unsafe extern "C" fn basalt_lsp_write(_handle: i32, _buf: i32, _len: i32) -> i32 {
    -1
}

#[cfg(not(target_arch = "wasm32"))]
#[no_mangle]
unsafe extern "C" fn basalt_lsp_read(_handle: i32, _out: i32, _cap: i32) -> i32 {
    -1
}

#[cfg(not(target_arch = "wasm32"))]
#[no_mangle]
unsafe extern "C" fn basalt_lsp_stop(_handle: i32) {}

#[cfg(not(target_arch = "wasm32"))]
#[no_mangle]
unsafe extern "C" fn basalt_sleep_ms(_ms: i32) {}

#[cfg(not(target_arch = "wasm32"))]
#[no_mangle]
unsafe extern "C" fn basalt_read_file(_path: i32, _path_len: i32, _out: i32, _cap: i32) -> i32 {
    -1
}

const READ_BUF_SIZE: usize = 64 * 1024;
const FILE_PROBE_SIZE: usize = 1024;

static mut READ_BUF: [u8; READ_BUF_SIZE] = [0u8; READ_BUF_SIZE];
static mut FILE_PROBE_BUF: [u8; FILE_PROBE_SIZE] = [0u8; FILE_PROBE_SIZE];

struct LspClient {
    handle: i32,
    buffer: Vec<u8>,
    next_id: i64,
    versions: HashMap<String, i32>,
}

static mut CLIENT: Option<LspClient> = None;

// ── LSP client lifecycle ────────────────────────────────────────────────────

fn ensure_client(path: &str) -> bool {
    unsafe {
        if CLIENT.is_some() {
            return true;
        }
    }

    let root = project_root(path);
    let cmd = b"gleam\0lsp";
    let handle = unsafe {
        basalt_spawn_lsp(
            cmd.as_ptr() as i32,
            cmd.len() as i32,
            root.as_ptr() as i32,
            root.len() as i32,
        )
    };
    if handle < 0 {
        return false;
    }

    let mut client = LspClient {
        handle,
        buffer: Vec::new(),
        next_id: 2,
        versions: HashMap::new(),
    };

    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": Value::Null,
            "clientInfo": { "name": "basalt" },
            "rootUri": file_uri(&root),
            "capabilities": {},
            "workspaceFolders": Value::Null
        }
    });
    if !send_value(&mut client, &init) || wait_for_response(&mut client, 1, 2000).is_none() {
        unsafe { basalt_lsp_stop(handle) };
        return false;
    }
    let _ = send_value(
        &mut client,
        &json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }),
    );

    unsafe {
        CLIENT = Some(client);
    }
    true
}

fn send_value(client: &mut LspClient, value: &Value) -> bool {
    let Ok(body) = serde_json::to_vec(value) else {
        return false;
    };
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut message = header.into_bytes();
    message.extend_from_slice(&body);
    let written =
        unsafe { basalt_lsp_write(client.handle, message.as_ptr() as i32, message.len() as i32) };
    written == message.len() as i32
}

fn next_message(client: &mut LspClient) -> Option<Value> {
    if let Some(body) = extract_message(&mut client.buffer) {
        return serde_json::from_str(&body).ok();
    }
    let buf: &mut [u8; READ_BUF_SIZE] = unsafe { &mut *core::ptr::addr_of_mut!(READ_BUF) };
    let n =
        unsafe { basalt_lsp_read(client.handle, buf.as_mut_ptr() as i32, READ_BUF_SIZE as i32) };
    if n > 0 {
        client.buffer.extend_from_slice(&buf[..n as usize]);
    } else {
        unsafe { basalt_sleep_ms(5) };
    }
    None
}

fn wait_for_response(client: &mut LspClient, id: i64, polls: u32) -> Option<Value> {
    for _ in 0..polls {
        let Some(value) = next_message(client) else {
            continue;
        };
        if value.get("id").and_then(Value::as_i64) == Some(id) {
            return Some(value);
        }
    }
    None
}

fn wait_for_diagnostics(client: &mut LspClient, uri: &str, polls: u32) -> Option<Value> {
    for _ in 0..polls {
        let Some(value) = next_message(client) else {
            continue;
        };
        if value.get("method").and_then(Value::as_str) != Some("textDocument/publishDiagnostics") {
            continue;
        }
        let Some(params) = value.get("params") else {
            continue;
        };
        if params.get("uri").and_then(Value::as_str) != Some(uri) {
            continue;
        }
        return params.get("diagnostics").cloned();
    }
    None
}

fn open_or_update(client: &mut LspClient, path: &str, src: &str, language_id: &str) {
    let uri = file_uri(path);
    let version = {
        let entry = client.versions.entry(uri.clone()).or_insert(0);
        *entry += 1;
        *entry
    };

    let message = if version == 1 {
        json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": version,
                    "text": src
                }
            }
        })
    } else {
        json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": src }]
            }
        })
    };
    let _ = send_value(client, &message);
}

// ── Diagnostics hook ────────────────────────────────────────────────────────

#[basalt_plugin]
fn diagnose(src: &[u8], path: &str) -> Vec<Diagnostic> {
    let Ok(text) = core::str::from_utf8(src) else {
        return Vec::new();
    };
    if !ensure_client(path) {
        return Vec::new();
    }

    let client = unsafe { CLIENT.as_mut().unwrap() };
    open_or_update(client, path, text, "gleam");
    let uri = file_uri(path);

    // Barrier: a hover request at the start of the document forces the server
    // to finish checking the file. Gleam publishes diagnostics only when it has
    // something to report, so without a barrier clean files would wait for the
    // full timeout. Diagnostics usually arrive before the hover response; a
    // short drain afterwards covers the other ordering.
    let barrier_id = client.next_id;
    client.next_id += 1;
    let barrier = json!({
        "jsonrpc": "2.0",
        "id": barrier_id,
        "method": "textDocument/hover",
        "params": {
            "textDocument": { "uri": uri },
            "position": { "line": 0, "character": 0 }
        }
    });
    let _ = send_value(client, &barrier);

    let mut found: Option<Value> = None;
    for _ in 0..1200 {
        let Some(value) = next_message(client) else {
            continue;
        };
        if is_diagnostics_notification(&value, &uri) {
            found = value
                .get("params")
                .and_then(|params| params.get("diagnostics"))
                .cloned();
        }
        if value.get("id").and_then(Value::as_i64) == Some(barrier_id) {
            break;
        }
    }
    let diagnostics = match found {
        Some(diagnostics) => diagnostics,
        None => wait_for_diagnostics(client, &uri, 60).unwrap_or(Value::Array(Vec::new())),
    };
    diagnostics_from_lsp(text, &diagnostics)
}

fn is_diagnostics_notification(value: &Value, uri: &str) -> bool {
    value.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
        && value
            .get("params")
            .and_then(|params| params.get("uri"))
            .and_then(Value::as_str)
            == Some(uri)
}

fn diagnostics_from_lsp(src: &str, diagnostics: &Value) -> Vec<Diagnostic> {
    let Some(items) = diagnostics.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(range) = item.get("range") else {
            continue;
        };
        let start = position_from_range(range, "start")
            .map(|(line, character)| position_to_byte(src, line, character))
            .unwrap_or(0);
        let end = position_from_range(range, "end")
            .map(|(line, character)| position_to_byte(src, line, character))
            .unwrap_or(start);
        let length = end.saturating_sub(start).max(1);
        let mut message = item
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if let Some(code) = item.get("code") {
            let code = match code {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            if !code.is_empty() {
                message = format!("[{code}] {message}");
            }
        }
        let severity = match item.get("severity").and_then(Value::as_u64) {
            Some(2) => Severity::Warning,
            Some(3) => Severity::Info,
            Some(4) => Severity::Hint,
            _ => Severity::Error,
        };
        out.push(Diagnostic::new(start, length, message, severity));
    }
    out
}

fn position_from_range(range: &Value, key: &str) -> Option<(u32, u32)> {
    let position = range.get(key)?;
    Some((
        position.get("line")?.as_u64()? as u32,
        position.get("character")?.as_u64()? as u32,
    ))
}

// ── Hover hook ──────────────────────────────────────────────────────────────

#[basalt_plugin]
fn hover(src: &[u8], path: &str, byte_offset: u32) -> String {
    let Ok(text) = core::str::from_utf8(src) else {
        return String::new();
    };
    if !ensure_client(path) {
        return String::new();
    }

    let client = unsafe { CLIENT.as_mut().unwrap() };
    open_or_update(client, path, text, "gleam");

    let (line, character) = byte_to_position(text, byte_offset);
    let id = client.next_id;
    client.next_id += 1;
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "textDocument/hover",
        "params": {
            "textDocument": { "uri": file_uri(path) },
            "position": { "line": line, "character": character }
        }
    });
    if !send_value(client, &request) {
        return String::new();
    }

    let Some(response) = wait_for_response(client, id, 400) else {
        return String::new();
    };
    response.get("result").map(hover_text).unwrap_or_default()
}

fn hover_text(result: &Value) -> String {
    match result {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Object(map) => {
            if let Some(contents) = map.get("contents") {
                hover_text(contents)
            } else {
                map.get("value")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            }
        }
        Value::Array(items) => items
            .iter()
            .map(hover_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

// ── LSP framing ─────────────────────────────────────────────────────────────

/// Pull one complete `Content-Length`-framed message body out of `buffer`.
fn extract_message(buffer: &mut Vec<u8>) -> Option<String> {
    let header_end = find_header_end(buffer)?;
    let header = core::str::from_utf8(&buffer[..header_end]).ok()?;
    let length = header.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("Content-Length") {
            value.trim().parse::<usize>().ok()
        } else {
            None
        }
    })?;
    let total = header_end + 4 + length;
    if buffer.len() < total {
        return None;
    }
    let body = buffer[header_end + 4..total].to_vec();
    buffer.drain(..total);
    String::from_utf8(body).ok()
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

// ── Position / URI helpers ──────────────────────────────────────────────────

/// Convert an LSP (line, UTF-16 character) position to a byte offset.
fn position_to_byte(src: &str, line: u32, character: u32) -> u32 {
    let mut current_line = 0u32;
    let mut line_start = 0usize;
    for (idx, ch) in src.char_indices() {
        if current_line == line {
            break;
        }
        if ch == '\n' {
            current_line += 1;
            line_start = idx + 1;
        }
    }
    if current_line != line {
        return src.len() as u32;
    }

    let slice = &src[line_start..];
    let mut units = 0u32;
    for (offset, ch) in slice.char_indices() {
        if units >= character || ch == '\n' {
            return (line_start + offset) as u32;
        }
        units += ch.len_utf16() as u32;
    }
    src.len() as u32
}

/// Convert a byte offset to an LSP (line, UTF-16 character) position.
fn byte_to_position(src: &str, byte_offset: u32) -> (u32, u32) {
    let limit = (byte_offset as usize).min(src.len());
    let mut line = 0u32;
    let mut line_start = 0usize;
    for (idx, ch) in src.char_indices() {
        if idx >= limit {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = idx + 1;
        }
    }
    let character = src[line_start..limit]
        .chars()
        .map(|ch| ch.len_utf16() as u32)
        .sum();
    (line, character)
}

fn file_uri(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let absolute = if normalized.starts_with('/') {
        normalized
    } else {
        format!("/{normalized}")
    };
    let mut out = String::from("file://");
    for byte in absolute.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Walk up from the file until a `gleam.toml` is found.
fn project_root(path: &str) -> String {
    let mut dir = parent_dir(path);
    loop {
        if dir.is_empty() {
            break;
        }
        let manifest = join_path(&dir, "gleam.toml");
        if file_exists(&manifest) {
            return dir;
        }
        let parent = parent_dir(&dir);
        if parent == dir {
            break;
        }
        dir = parent;
    }
    let parent = parent_dir(path);
    if parent.is_empty() {
        ".".to_string()
    } else {
        parent
    }
}

fn file_exists(path: &str) -> bool {
    let buf: &mut [u8; FILE_PROBE_SIZE] = unsafe { &mut *core::ptr::addr_of_mut!(FILE_PROBE_BUF) };
    let n = unsafe {
        basalt_read_file(
            path.as_ptr() as i32,
            path.len() as i32,
            buf.as_mut_ptr() as i32,
            FILE_PROBE_SIZE as i32,
        )
    };
    n > 0
}

fn parent_dir(path: &str) -> String {
    path.rsplit_once(['/', '\\'])
        .map(|(parent, _)| parent.to_string())
        .unwrap_or_default()
}

fn join_path(root: &str, suffix: &str) -> String {
    if root.ends_with('/') || root.ends_with('\\') {
        format!("{root}{suffix}")
    } else {
        format!("{root}/{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_extracts_only_complete_messages() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut buffer = framed.into_bytes();
        assert_eq!(extract_message(&mut buffer).as_deref(), Some(body));
        assert!(buffer.is_empty());

        let mut partial = b"Content-Length: 40\r\n\r\n{\"jsonrpc\"".to_vec();
        assert!(extract_message(&mut partial).is_none());
    }

    #[test]
    fn byte_and_lsp_positions_round_trip() {
        let src = "pub fn add(a: Int) {\n  a + 1\n}\n";
        let offset = src.find("a + 1").unwrap() as u32;
        let (line, character) = byte_to_position(src, offset);
        assert_eq!((line, character), (1, 2));
        assert_eq!(position_to_byte(src, line, character), offset);
    }

    #[test]
    fn utf16_columns_are_respected() {
        // "é" is one UTF-16 unit, "𝄞" is two.
        let src = "let x = \"𝄞\"\n";
        let offset = src.find('"').unwrap() as u32;
        let (line, character) = byte_to_position(src, offset);
        assert_eq!(line, 0);
        assert_eq!(character, 8);
        assert_eq!(position_to_byte(src, line, character), offset);
    }

    #[test]
    fn lsp_diagnostics_become_byte_ranges() {
        let src = "pub fn add(a: Int, b: Int) -> Int {\n  a + missing\n}\n";
        let payload = json!([{
            "range": {
                "start": { "line": 1, "character": 6 },
                "end": { "line": 1, "character": 13 }
            },
            "severity": 1,
            "code": "unknown_variable",
            "message": "The name `missing` is not in scope here."
        }]);

        let diagnostics = diagnostics_from_lsp(src, &payload);
        assert_eq!(diagnostics.len(), 1);
        let diag = &diagnostics[0];
        assert_eq!(
            &src[diag.offset as usize..(diag.offset + diag.length) as usize],
            "missing"
        );
        assert_eq!(diag.severity, Severity::Error);
        assert!(diag.message.contains("unknown_variable"));
    }

    #[test]
    fn hover_flattens_markup_and_marked_strings() {
        assert_eq!(
            hover_text(&json!({ "kind": "markdown", "value": "```gleam\nfn add\n```" })),
            "```gleam\nfn add\n```"
        );
        assert_eq!(
            hover_text(&json!(["first", { "language": "gleam", "value": "second" }])),
            "first\n\nsecond"
        );
        assert_eq!(
            hover_text(&json!({ "contents": "```gleam\nfn() -> String\n```" })),
            "```gleam\nfn() -> String\n```"
        );
        assert_eq!(hover_text(&Value::Null), "");
    }

    #[test]
    fn file_uris_are_encoded() {
        assert_eq!(
            file_uri("C:\\ws\\src\\a.gleam"),
            "file:///C:/ws/src/a.gleam"
        );
        assert_eq!(file_uri("/ws/src/a b.gleam"), "file:///ws/src/a%20b.gleam");
    }
}
