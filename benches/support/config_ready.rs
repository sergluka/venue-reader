use std::time::{Duration, Instant};

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use serde_json::{json, Value};

const REQUEST_FAILED: i32 = lsp_server::ErrorCode::RequestFailed as i32;

pub fn wait(client: &Connection, uri: &str, mut notification: impl FnMut(Notification)) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut request_number = -1;
    loop {
        let id = RequestId::from(request_number);
        client
            .sender
            .send(Message::Request(Request::new(
                id.clone(),
                "workspace/executeCommand".into(),
                json!({
                    "command": "fix.describeOverrides", "arguments": [uri],
                }),
            )))
            .expect("send readiness request");
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match client
                .receiver
                .recv_timeout(remaining)
                .expect("configuration ready within 5s")
            {
                Message::Response(response) if response.id == id => {
                    match response.response_result {
                        Ok(_) => return,
                        Err(error) if error.code == REQUEST_FAILED => break,
                        Err(error) => panic!("readiness request failed: {error:?}"),
                    }
                }
                Message::Request(request) => {
                    let result = if request.method == "workspace/configuration" {
                        json!([null])
                    } else {
                        Value::Null
                    };
                    client
                        .sender
                        .send(Message::Response(Response::new_ok(request.id, result)))
                        .expect("answer server request");
                }
                Message::Notification(note) => notification(note),
                Message::Response(response) => panic!(
                    "unexpected response during fixture setup: {:?}",
                    response.id
                ),
            }
        }
        assert!(
            Instant::now() < deadline,
            "document configuration did not become ready"
        );
        request_number -= 1;
        std::thread::sleep(Duration::from_millis(1));
    }
}
