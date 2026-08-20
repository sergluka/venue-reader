//! Byte-safe JSON-RPC transport.
//!
//! `lsp-server`'s stdio reader does `String::from_utf8` on the whole body and
//! dies on the non-UTF-8 bytes FIX logs carry. We own the reader/writer instead:
//! the incoming body is made parseable by replacing each invalid UTF-8 byte with
//! a reserved code point (`U+F700 + b`); byte recovery back to `Line.bytes`
//! happens only for document-text fields (see [`text_to_bytes`]). Outgoing
//! messages are plain UTF-8 JSON — never unescaped.

use std::io::{self, BufRead, Write};
use std::thread::JoinHandle;

use crossbeam_channel::{unbounded, Receiver, Sender};
use lsp_server::{ErrorCode, Message, Notification, Request, RequestId, Response, ResponseError};
use serde_json::Value;

/// Start of the reserved byte-escape range: `U+F700..=U+F7FF` maps to bytes `0..=255`.
const ESCAPE_BASE: u32 = 0xF700;

/// Make a raw JSON-RPC body parseable: every invalid UTF-8 byte `b` becomes the
/// scalar `U+F700 + b`. Valid content (JSON structure is pure ASCII) is untouched,
/// so only bytes inside string values are ever rewritten.
///
/// Takes the body by value: a well-formed message — which is nearly all of them,
/// including a multi-megabyte `didOpen` — is then validated in place and reuses
/// the reader's allocation instead of being copied.
pub fn sanitize_incoming_body(bytes: Vec<u8>) -> String {
    let bytes = match String::from_utf8(bytes) {
        Ok(text) => return text,
        Err(e) => e.into_bytes(),
    };
    let mut out = String::with_capacity(bytes.len());
    let mut rest = &bytes[..];
    loop {
        match std::str::from_utf8(rest) {
            Ok(tail) => {
                out.push_str(tail);
                return out;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                // Valid by definition of `valid_up_to`.
                out.push_str(std::str::from_utf8(&rest[..valid]).unwrap());
                let bad = e.error_len().unwrap_or(rest.len() - valid);
                for &b in &rest[valid..valid + bad] {
                    out.push(escape_byte(b));
                }
                rest = &rest[valid + bad..];
            }
        }
    }
}

/// Recover document bytes from sanitised text: reserved-range scalars become the
/// single byte they escape, everything else its UTF-8 bytes. Apply **only** to
/// document-text fields (`didOpen`/`didChange` text) — never to URIs/config.
pub fn text_to_bytes(s: &str) -> Vec<u8> {
    if !has_escape(s) {
        return s.as_bytes().to_vec();
    }
    decode_escapes(s)
}

/// As [`text_to_bytes`], but consumes the string so an unescaped one — the usual
/// case, and the whole document on `didOpen` — is moved rather than copied.
pub fn text_into_bytes(s: String) -> Vec<u8> {
    if !has_escape(&s) {
        return s.into_bytes();
    }
    decode_escapes(&s)
}

/// Every reserved-range scalar encodes as `EF 9C 80`..`EF 9F BF`, so a body with no
/// `0xEF` byte cannot contain one. Conservative and a single pass.
fn has_escape(s: &str) -> bool {
    s.as_bytes().contains(&0xEF)
}

fn decode_escapes(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut run_start = 0;
    for (idx, c) in s.char_indices() {
        let cp = c as u32;
        if (ESCAPE_BASE..=ESCAPE_BASE + 0xFF).contains(&cp) {
            // Copy the untouched run before this escape in one go.
            out.extend_from_slice(&s.as_bytes()[run_start..idx]);
            out.push((cp - ESCAPE_BASE) as u8);
            run_start = idx + c.len_utf8();
        }
    }
    out.extend_from_slice(&s.as_bytes()[run_start..]);
    out
}

fn escape_byte(b: u8) -> char {
    // Always valid: U+F700..=U+F7FF are assigned scalar values.
    char::from_u32(ESCAPE_BASE + b as u32).unwrap()
}

/// A result the writer serialises, rather than one a handler turned into a
/// `serde_json::Value` first.
///
/// `serde::Serialize` is not object-safe — its method is generic — so this
/// erases it with the generic on the *impl* instead, which needs no crate to
/// do. What it buys: a handler returns its own type, and the walk over it
/// happens on the writer's thread rather than on the loop that is also
/// answering every other request.
pub trait WriteJson: Send {
    fn write_json(&self, out: &mut Vec<u8>) -> serde_json::Result<()>;
}

impl<T: serde::Serialize + Send> WriteJson for T {
    fn write_json(&self, out: &mut Vec<u8>) -> serde_json::Result<()> {
        serde_json::to_writer(out, self)
    }
}

/// What the server hands the writer.
///
/// A `Result` skips `serde_json::Value` entirely: building the tree and then
/// letting `lsp_server` rebuild it costs three times the time and ten times
/// the transient memory of writing the bytes, measured on a million-integer
/// array.
pub enum Outbound {
    Message(Message),
    /// A successful response whose result is still a value, not yet JSON.
    Result {
        id: RequestId,
        body: Box<dyn WriteJson>,
    },
}

impl From<Message> for Outbound {
    fn from(msg: Message) -> Outbound {
        Outbound::Message(msg)
    }
}

impl From<Response> for Outbound {
    fn from(resp: Response) -> Outbound {
        Outbound::Message(Message::Response(resp))
    }
}

impl From<Notification> for Outbound {
    fn from(note: Notification) -> Outbound {
        Outbound::Message(Message::Notification(note))
    }
}

impl From<Request> for Outbound {
    fn from(req: Request) -> Outbound {
        Outbound::Message(Message::Request(req))
    }
}

impl Outbound {
    /// For a client that reads `Message`s rather than bytes: the result is
    /// serialised and parsed back into the `Value` a `Response` holds, which
    /// costs what the wire path saved and is paid only in process.
    pub fn into_message(self) -> Message {
        match self {
            Outbound::Message(msg) => msg,
            Outbound::Result { id, body } => Message::Response(Response {
                id,
                response_result: as_value(body.as_ref()).map_err(|e| ResponseError {
                    code: ErrorCode::InternalError as i32,
                    message: format!("the server built a result it cannot serialise: {e}"),
                    data: None,
                }),
            }),
        }
    }
}

fn as_value(body: &dyn WriteJson) -> serde_json::Result<Value> {
    let mut out = Vec::new();
    body.write_json(&mut out)?;
    serde_json::from_slice(&out)
}

/// Where the server's outgoing messages go.
///
/// Two backends and no thread between them, so what a handler sends is written
/// in the order it sent it — the in-process one converts in the sending
/// thread rather than handing the work to a bridge.
#[derive(Clone)]
pub enum Sink {
    /// The writer thread frames it.
    Stdio(Sender<Outbound>),
    /// An in-process client reading `Message`s.
    Memory(Sender<Message>),
}

impl From<Sender<Outbound>> for Sink {
    fn from(sender: Sender<Outbound>) -> Sink {
        Sink::Stdio(sender)
    }
}

impl From<Sender<Message>> for Sink {
    fn from(sender: Sender<Message>) -> Sink {
        Sink::Memory(sender)
    }
}

impl Sink {
    /// A closed channel is not an error here: the client is gone, and the loop
    /// finds out from its own receiver.
    pub fn send(&self, out: impl Into<Outbound>) {
        match self {
            Sink::Stdio(writer) => {
                let _ = writer.send(out.into());
            }
            Sink::Memory(client) => {
                let _ = client.send(out.into().into_message());
            }
        }
    }
}

/// The server's end of a transport.
pub struct Link {
    pub receiver: Receiver<Message>,
    pub sender: Sink,
}

/// Handles for the reader/writer threads. [`join`](TransportThreads::join) waits
/// for the writer to drain; the reader is **detached** (a client that leaves stdin
/// open after `exit` must not deadlock the process).
pub struct TransportThreads {
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl TransportThreads {
    pub fn join(self) {
        // Join the writer so buffered responses reach stdout. The reader may be
        // blocked on stdin `read`; leave it detached — the process exits regardless.
        let _ = self.writer.join();
        drop(self.reader);
    }
}

/// Build a byte-safe [`Link`] over stdin/stdout plus its threads.
pub fn stdio() -> (Link, TransportThreads) {
    let (reader_tx, reader_rx) = unbounded::<Message>();
    let (writer_tx, writer_rx) = unbounded::<Outbound>();

    let reader = std::thread::spawn(move || {
        let stdin = io::stdin();
        let mut lock = stdin.lock();
        loop {
            match read_frame(&mut lock) {
                Ok(Some(body)) => match parse_message(body) {
                    Ok(msg) => {
                        if reader_tx.send(msg).is_err() {
                            break; // server gone
                        }
                    }
                    Err(e) => {
                        log::error!("transport: skipping malformed message: {e}");
                    }
                },
                Ok(None) => break, // EOF
                Err(e) => {
                    log::error!("transport: framing error: {e}");
                    break;
                }
            }
        }
    });

    let writer = std::thread::spawn(move || {
        let stdout = io::stdout();
        let mut lock = stdout.lock();
        for out in writer_rx {
            let wrote = match &out {
                Outbound::Message(msg) => write_message(&mut lock, msg),
                Outbound::Result { id, body } => write_result(&mut lock, id, body.as_ref()),
            };
            if let Err(e) = wrote {
                log::error!("transport: write error: {e}");
                break;
            }
        }
    });

    let link = Link {
        receiver: reader_rx,
        sender: Sink::Stdio(writer_tx),
    };
    (link, TransportThreads { reader, writer })
}

fn parse_message(body: Vec<u8>) -> Result<Message, serde_json::Error> {
    serde_json::from_str::<Message>(&sanitize_incoming_body(body))
}

/// Read one `Content-Length`-framed body. `Ok(None)` on EOF at a frame boundary.
fn read_frame(r: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut content_len: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            return Ok(None); // EOF
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some(v) = trimmed.strip_prefix("Content-Length:") {
            content_len = v.trim().parse().ok();
        }
        // Content-Type and any other headers are ignored.
    }
    let len = content_len
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length"))?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

/// Delegates to `lsp-server`, which wraps the payload so the mandatory
/// `"jsonrpc":"2.0"` member is present — serialising `Message` directly omits it,
/// because the enum is `#[serde(untagged)]`.
fn write_message(w: &mut impl Write, msg: &Message) -> io::Result<()> {
    msg.write(w)?;
    w.flush()
}

/// One response, serialised here rather than by whoever produced it.
///
/// Hand-framed rather than routed through `lsp_server::Response`, which holds
/// a `serde_json::Value`: going through it would build the very tree this
/// exists to avoid. The buffer is not avoidable — `Content-Length` precedes
/// the body, so the length must be known before the first byte goes out.
fn write_result(w: &mut impl Write, id: &RequestId, body: &dyn WriteJson) -> io::Result<()> {
    let mut result = Vec::new();
    // Two failures with nothing in common. A body that will not serialise is
    // about this one response and the client is owed an error for it; a write
    // that fails is the connection, and nothing more can be said on it. Writing
    // the bytes only after the body is in hand is what keeps them apart — a
    // half-written frame would desynchronise every message after it.
    if let Err(e) = body.write_json(&mut result) {
        log::error!("transport: a result that cannot be serialised: {e}");
        return write_message(
            w,
            &Message::Response(Response::new_err(
                id.clone(),
                ErrorCode::InternalError as i32,
                format!("the server built a result it cannot serialise: {e}"),
            )),
        );
    }
    let id = serde_json::to_string(id).map_err(io::Error::other)?;
    let head = format!(r#"{{"jsonrpc":"2.0","id":{id},"result":"#);
    // The closing brace is the one byte past the head and the result.
    write!(
        w,
        "Content-Length: {}\r\n\r\n",
        head.len() + result.len() + 1
    )?;
    w.write_all(head.as_bytes())?;
    w.write_all(&result)?;
    w.write_all(b"}")?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_server::Notification;
    use proptest::prelude::*;
    use serde_json::json;

    #[test]
    fn ascii_roundtrips() {
        let b = b"8=FIX.4.4\x0135=D";
        assert_eq!(text_to_bytes(&sanitize_incoming_body(b.to_vec())), b);
    }

    #[test]
    fn raw_invalid_byte_recovers() {
        let b = &[0x80u8, 0xff, 0x01, b'A'];
        assert_eq!(text_to_bytes(&sanitize_incoming_body(b.to_vec())), b);
    }

    #[test]
    fn json_escape_pua_maps_to_byte() {
        // Documented limitation: a genuine U+F781 in text (literal or ``)
        // recovers as the single byte 0x81 (0xF781 - 0xF700).
        assert_eq!(text_to_bytes("\u{F781}"), vec![0x81]);
    }

    #[test]
    fn frame_with_invalid_byte_in_string_parses_and_recovers() {
        let mut body = br#"{"jsonrpc":"2.0","method":"x","params":{"t":"A"#.to_vec();
        body.push(0x80); // raw invalid byte inside the JSON string value
        body.extend_from_slice(br#""}}"#);
        let mut framed = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        framed.extend_from_slice(&body);

        let mut cur = io::Cursor::new(framed);
        let frame = read_frame(&mut cur).unwrap().unwrap();
        let text = sanitize_incoming_body(frame);
        let Message::Notification(n) = serde_json::from_str::<Message>(&text).unwrap() else {
            panic!("expected a notification");
        };
        let t = n.params["t"].as_str().unwrap();
        assert_eq!(text_to_bytes(t), vec![b'A', 0x80]);
    }

    #[test]
    fn outgoing_message_is_plain_utf8_no_unescape() {
        let msg = Message::Notification(Notification {
            method: "m".into(),
            params: json!({ "s": "café ✓" }),
        });
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        let s = String::from_utf8(buf).expect("outgoing body is valid UTF-8");
        assert!(s.contains("café ✓"), "server text must not be mangled");
    }

    #[test]
    fn outgoing_message_carries_the_jsonrpc_member() {
        // `Message` is #[serde(untagged)], so serialising it directly drops the
        // mandatory JSON-RPC 2.0 member; every outgoing frame must still have it.
        let msg = Message::Notification(Notification {
            method: "m".into(),
            params: json!({}),
        });
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        let body = String::from_utf8(buf).unwrap();
        let body = body.split("\r\n\r\n").nth(1).expect("framed body");
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
    }

    #[test]
    fn frame_roundtrips_through_writer_and_reader() {
        let msg = Message::Notification(Notification {
            method: "ping".into(),
            params: json!({ "n": 1 }),
        });
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        let mut cur = io::Cursor::new(buf);
        let body = read_frame(&mut cur).unwrap().unwrap();
        let back = serde_json::from_slice::<Message>(&body).unwrap();
        assert_eq!(format!("{msg:?}"), format!("{back:?}"));
    }

    proptest! {
        // Round-trip is exact for any input without a genuine reserved-range
        // scalar. Those all UTF-8-encode with a `0xEF` lead byte, so excluding
        // `0xEF` from the generator guarantees none can form.
        #[test]
        fn roundtrip_without_reserved_lead(
            x in prop::collection::vec(
                any::<u8>().prop_filter("exclude reserved-range lead 0xEF", |b| *b != 0xEF),
                0..64,
            )
        ) {
            prop_assert_eq!(text_to_bytes(&sanitize_incoming_body(x.clone())), x);
        }
    }
}

#[cfg(test)]
mod frames {
    use super::*;
    use serde_json::json;

    fn body_of(written: Vec<u8>) -> Vec<u8> {
        read_frame(&mut io::Cursor::new(written))
            .expect("framed")
            .expect("a body")
    }

    /// A hand-written frame must read back as the response it stands for —
    /// that is the whole claim, since nothing else validates bytes the server
    /// assembled itself.
    #[test]
    fn a_lazy_result_reads_back_as_its_response() {
        for id in [
            RequestId::from(7),
            RequestId::from("a \"quoted\" ид".to_owned()),
        ] {
            let body = json!({ "data": [1, 2, 3] });
            let mut written = Vec::new();
            write_result(&mut written, &id, &body).expect("written");

            let Message::Response(resp) =
                serde_json::from_slice(&body_of(written)).expect("a message")
            else {
                panic!("a response");
            };
            assert_eq!(resp.id, id);
            assert_eq!(
                resp.response_result.expect("ok"),
                json!({ "data": [1, 2, 3] })
            );
        }
    }

    /// A body that will not serialise is about one response, not the
    /// connection. Answering it as an error keeps the writer alive; breaking
    /// the loop instead would leave the server reading input and answering
    /// into nothing, with `shutdown` never replied to either.
    #[test]
    fn a_result_that_cannot_be_serialised_becomes_an_error_and_not_a_dead_writer() {
        // `serde_json` refuses a map whose key is not a string.
        let body: std::collections::BTreeMap<(u8, u8), u8> = [((1, 2), 3)].into_iter().collect();
        let mut out = Vec::new();
        write_result(&mut out, &RequestId::from(7), &body).expect("the writer survives");

        let frame = String::from_utf8(out).expect("utf-8");
        let body = frame.split("\r\n\r\n").nth(1).expect("a body");
        let parsed: Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(parsed["id"], 7);
        assert_eq!(parsed["error"]["code"], ErrorCode::InternalError as i32);
        assert!(parsed.get("result").is_none(), "{parsed}");

        // And the frame after it is written normally.
        let mut out = Vec::new();
        write_result(&mut out, &RequestId::from(8), &json!([1, 2])).expect("written");
        assert!(
            String::from_utf8_lossy(&out).ends_with(r#"{"jsonrpc":"2.0","id":8,"result":[1,2]}"#)
        );
    }

    #[test]
    fn an_in_process_client_sees_the_same_response() {
        let out = Outbound::Result {
            id: RequestId::from(1),
            body: Box::new(json!([4, 5])),
        };
        let Message::Response(resp) = out.into_message() else {
            panic!("a response");
        };
        assert_eq!(resp.id, RequestId::from(1));
        assert_eq!(resp.response_result.expect("ok"), json!([4, 5]));
    }

    /// Both response forms and a notification share one queue, so what a
    /// handler sends is read in the order it sent it.
    #[test]
    fn a_sink_keeps_the_order_it_was_given() {
        let (tx, rx) = unbounded::<Message>();
        let sink = Sink::Memory(tx);
        sink.send(Notification::new("first".into(), json!({})));
        sink.send(Outbound::Result {
            id: RequestId::from(2),
            body: Box::new(json!([])),
        });
        sink.send(Notification::new("third".into(), json!({})));

        let seen: Vec<String> = rx
            .try_iter()
            .map(|message| match message {
                Message::Notification(note) => note.method,
                Message::Response(_) => "a response".to_owned(),
                Message::Request(req) => req.method,
            })
            .collect();
        assert_eq!(seen, ["first", "a response", "third"]);
    }
}
