//! M5.2 against local stdio, Unix-socket and Streamable HTTP servers.

mod support;

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use gllmh::mcp::{self, Error};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixListener};
use tokio::process::Command;
use tokio::time::timeout;

const TEST_TIMEOUT: Duration = Duration::from_secs(10);
const VERSION: &str = "2025-06-18";

fn stdio_server(mode: &str) -> String {
    let path = support::fixture("mcp_stdio.sh");
    format!("sh {} {}", shell_words::quote(path.to_str().unwrap()), shell_words::quote(mode))
}

#[tokio::test]
async fn stdio_lists_all_pages_and_reports_missing_descriptions() {
    let tools = timeout(TEST_TIMEOUT, mcp::list(&stdio_server("stdio fixture"))).await.unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(tools).unwrap(),
        json!([
            {"name": "echo", "description": "Echo text", "input_schema": {"type": "object"}},
            {"name": "fail", "description": null, "input_schema": {"type": "object"}}
        ])
    );
}

#[tokio::test]
async fn stdio_early_exit_returns_an_error() {
    let error = timeout(TEST_TIMEOUT, mcp::list(&stdio_server("exit early"))).await.unwrap().unwrap_err();
    assert!(
        matches!(error, Error::EndOfStream)
            || matches!(error, Error::Io(ref e) if e.kind() == std::io::ErrorKind::BrokenPipe),
        "{error}"
    );
}

#[tokio::test]
async fn binary_reads_stdin_and_writes_output_even_for_tool_errors() {
    let server = stdio_server("stdio fixture");
    let mixed_output = format!("hello world{}", json!({"type": "image", "mimeType": "image/png", "data": "AA=="}));
    for (input, success, expected_stdout) in [
        (json!({"tool": "echo", "arguments": {"text": "hello world"}}), true, mixed_output.as_str()),
        (json!({"tool": "fail", "arguments": {}}), false, "fixture failure"),
    ] {
        let mut child = Command::new(support::gllmh_bin())
            .args(["mcp", "call", "--server", &server])
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input.to_string().as_bytes()).await.unwrap();
        drop(stdin);

        let output = timeout(TEST_TIMEOUT, child.wait_with_output()).await.unwrap().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.success(), success, "{stderr}");
        assert_eq!(output.stdout, expected_stdout.as_bytes());
        if success {
            assert!(stderr.is_empty(), "{stderr}");
        } else {
            assert!(stderr.contains("isError"), "{stderr}");
        }
    }
}

fn rpc_result(request: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
}

fn initialize_reply(request: &Value) -> Value {
    assert_eq!(request["jsonrpc"], "2.0");
    assert_eq!(request["method"], "initialize");
    assert_eq!(request["params"]["protocolVersion"], VERSION);
    assert_eq!(request["params"]["clientInfo"]["name"], "gllmh");
    assert_eq!(request["params"]["capabilities"], json!({}));
    rpc_result(
        request,
        json!({"protocolVersion": VERSION, "capabilities": {"tools": {}},
               "serverInfo": {"name": "test-fixture", "version": "1"}}),
    )
}

fn assert_initialized(notification: &Value) {
    assert_eq!(notification, &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
}

async fn write_line(writer: &mut (impl AsyncWrite + Unpin), value: Value) {
    writer.write_all(format!("{value}\n").as_bytes()).await.unwrap();
}

#[tokio::test]
async fn unix_socket_call_ignores_notifications_and_answers_server_requests() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.sock");
    let listener = UnixListener::bind(&path).unwrap();
    for endpoint in [path.display().to_string(), format!("unix:{}", path.display())] {
        let server = async {
            let (socket, _) = listener.accept().await.unwrap();
            let (read, mut write) = socket.into_split();
            let mut lines = BufReader::new(read).lines();

            let initialize = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            write_line(&mut write, initialize_reply(&initialize)).await;
            let notification = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_initialized(&notification);

            let call: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(call["method"], "tools/call");
            assert_eq!(call["params"], json!({"name": "echo", "arguments": {"text": "hi"}}));
            write_line(&mut write, json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})).await;
            write_line(&mut write, json!({"jsonrpc": "2.0", "id": "ping", "method": "ping"})).await;
            let pong: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(pong, json!({"jsonrpc": "2.0", "id": "ping", "result": {}}));
            write_line(&mut write, json!({"jsonrpc": "2.0", "id": "server-1", "method": "roots/list"})).await;
            let answer: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(answer["id"], "server-1");
            assert_eq!(answer["error"]["code"], -32601);
            write_line(&mut write, rpc_result(&call, json!({"content": [{"type": "text", "text": "hi"}]}))).await;
        };

        let (result, ()) =
            timeout(TEST_TIMEOUT, async { tokio::join!(mcp::call(&endpoint, "echo", json!({"text": "hi"})), server) })
                .await
                .unwrap();
        let result = result.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.output(), "hi");
    }
}

struct HttpRequest {
    socket: TcpStream,
    request_line: String,
    headers: HashMap<String, String>,
    body: Value,
}

impl HttpRequest {
    async fn accept(listener: &TcpListener) -> Self {
        let (socket, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(socket);
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
        let request_line = line.trim_end().to_owned();
        let mut headers = HashMap::new();
        loop {
            line.clear();
            assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            let (name, value) = line.split_once(':').unwrap();
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
        let body = match headers.get("content-length") {
            Some(length) => {
                let mut bytes = vec![0; length.parse().unwrap()];
                reader.read_exact(&mut bytes).await.unwrap();
                serde_json::from_slice(&bytes).unwrap()
            }
            None => Value::Null,
        };
        Self { socket: reader.into_inner(), request_line, headers, body }
    }

    fn assert_post(&self) {
        assert_eq!(self.request_line, "POST /mcp HTTP/1.1");
        assert_eq!(self.headers["accept"], "application/json, text/event-stream");
        assert_eq!(self.headers["content-type"], "application/json");
    }

    fn assert_session(&self, session: &str) {
        assert_eq!(self.headers["mcp-session-id"], session);
        assert_eq!(self.headers["mcp-protocol-version"], VERSION);
    }

    async fn json(mut self, headers: &str, body: Value) {
        let reply = format!(
            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\n\r\n{body}",
            body.to_string().len()
        );
        self.socket.write_all(reply.as_bytes()).await.unwrap();
    }

    async fn accepted(mut self) {
        self.socket
            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
    }

    async fn events(mut self) -> TcpStream {
        self.socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nTransfer-Encoding: chunked\r\n\r\n")
            .await
            .unwrap();
        self.socket
    }
}

async fn write_http_chunk(socket: &mut TcpStream, chunk: &str) {
    socket.write_all(format!("{:x}\r\n{chunk}\r\n", chunk.len()).as_bytes()).await.unwrap();
}

async fn http_handshake(listener: &TcpListener, session: &str) {
    let initialize = HttpRequest::accept(listener).await;
    initialize.assert_post();
    assert!(!initialize.headers.contains_key("mcp-session-id"));
    let reply = initialize_reply(&initialize.body);
    initialize.json(&format!("Mcp-Session-Id: {session}\r\n"), reply).await;

    let notification = HttpRequest::accept(listener).await;
    notification.assert_post();
    notification.assert_session(session);
    assert_initialized(&notification.body);
    notification.accepted().await;
}

async fn http_shutdown(listener: &TcpListener, session: &str) {
    let shutdown = HttpRequest::accept(listener).await;
    assert_eq!(shutdown.request_line, "DELETE /mcp HTTP/1.1");
    shutdown.assert_session(session);
    shutdown.accepted().await;
}

#[tokio::test]
async fn http_list_handles_json_and_streaming_replies_in_one_session() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let server = async {
        let session = "list-session";
        http_handshake(&listener, session).await;

        let first = HttpRequest::accept(&listener).await;
        first.assert_post();
        first.assert_session(session);
        assert_eq!(first.body["method"], "tools/list");
        assert_eq!(first.body["params"], json!({}));
        let reply = rpc_result(
            &first.body,
            json!({
                "tools": [{"name": "echo", "inputSchema": {"type": "object"}}], "nextCursor": "next"
            }),
        );
        first.json("", reply).await;

        let second = HttpRequest::accept(&listener).await;
        second.assert_post();
        second.assert_session(session);
        assert_eq!(second.body["params"], json!({"cursor": "next"}));
        let reply = rpc_result(&second.body, json!({"tools": [{"name": "fail", "inputSchema": {"type": "object"}}]}));
        let mut events = second.events().await;
        let request = json!({"jsonrpc": "2.0", "id": 2, "method": "sampling/createMessage"});
        write_http_chunk(&mut events, &format!(": ping\r\n\r\nevent: message\r\ndata: {request}\r\n\r\n")).await;

        let answer = HttpRequest::accept(&listener).await;
        answer.assert_post();
        answer.assert_session(session);
        assert_eq!(answer.body["id"], 2);
        assert_eq!(answer.body["error"]["code"], -32601);
        answer.accepted().await;

        let reply = reply.to_string();
        let (left, right) = reply.split_at(reply.len() / 2);
        write_http_chunk(&mut events, &format!("data: {left}")).await;
        write_http_chunk(&mut events, &format!("{right}\n\n")).await;

        // Keep the event stream open until the client has read the reply and begun shutdown.
        http_shutdown(&listener, session).await;
    };
    let (tools, ()) = timeout(TEST_TIMEOUT, async { tokio::join!(mcp::list(&endpoint), server) }).await.unwrap();
    let names: Vec<_> = tools.unwrap().into_iter().map(|tool| tool.name).collect();
    assert_eq!(names, ["echo", "fail"]);
}

#[tokio::test]
async fn http_call_preserves_tool_errors() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let server = async {
        let session = "call-session";
        http_handshake(&listener, session).await;

        let call = HttpRequest::accept(&listener).await;
        call.assert_post();
        call.assert_session(session);
        assert_eq!(call.body["method"], "tools/call");
        assert_eq!(call.body["params"], json!({"name": "fail", "arguments": {}}));
        let reply = rpc_result(
            &call.body,
            json!({
                "isError": true, "content": [{"type": "text", "text": "HTTP fixture failure"}]
            }),
        );
        call.json("", reply).await;

        http_shutdown(&listener, session).await;
    };

    let (result, ()) =
        timeout(TEST_TIMEOUT, async { tokio::join!(mcp::call(&endpoint, "fail", json!({})), server) }).await.unwrap();
    let result = result.unwrap();
    assert!(result.is_error);
    assert_eq!(result.output(), "HTTP fixture failure");
}
