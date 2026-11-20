mod support;

use std::time::{Duration, Instant};

use reqwest::{Client, Response, StatusCode};
use serde_json::{Value, json};
use support::fake_llm::{FakeLlm, Script};

fn script(source: Value) -> Script {
    let rules: Script = serde_json::from_value(source).unwrap();
    serde_json::from_value(serde_json::to_value(&rules).unwrap()).unwrap()
}

fn client() -> Client {
    Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap()
}

async fn post(client: &Client, server: &FakeLlm, path: &str, body: &Value) -> Response {
    client.post(format!("{}{path}", server.base_url())).header("x-test", "fake-llm").json(body).send().await.unwrap()
}

async fn success(response: Response) -> Value {
    response.error_for_status().unwrap().json().await.unwrap()
}

#[tokio::test]
async fn anthropic_messages() {
    let server = FakeLlm::start(script(json!([
        {"match":{"model_id":"claude-fake","contains":"find it"},
         "response":{"type":"message","thinking":"look it up","tool_calls":[
             {"name":"lookup","arguments":{"id":7}},
             {"name":"lookup","arguments":{"id":8}}
         ]}},
        {"match":{"contains":"found seven"},
         "response":{"type":"message","text":"done"}},
        {"response":{"type":"error","status":529,"message":"try later","retry_after":2}}
    ])))
    .await
    .unwrap();
    let client = client();
    let path = "/v1/messages";
    let first = json!({
        "model":"claude-fake", "max_tokens":100, "messages":[{"role":"user","content":"find it"}]
    });
    let body = success(post(&client, &server, path, &first).await).await;
    assert_eq!(body["type"], "message");
    assert_eq!(body["model"], "claude-fake");
    assert_eq!(body["role"], "assistant");
    assert_eq!(body["stop_reason"], "tool_use");
    assert_eq!(body["usage"], json!({"input_tokens":1,"output_tokens":1}));

    let blocks = body["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 3);
    assert_eq!(blocks[0]["type"], "thinking");
    assert_eq!(blocks[0]["thinking"], "look it up");
    assert!(blocks[0]["signature"].is_string());
    let first_id = blocks[1]["id"].as_str().unwrap();
    let second_id = blocks[2]["id"].as_str().unwrap();
    assert!(first_id.starts_with("toolu_"));
    assert_ne!(first_id, second_id);
    assert_eq!(blocks[1]["type"], "tool_use");
    assert_eq!(blocks[1]["name"], "lookup");
    assert_eq!(blocks[1]["input"], json!({"id":7}));

    let followup = json!({
        "model":"claude-fake","max_tokens":100,"messages":[
            first["messages"][0],
            {"role":"assistant","content":blocks},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":first_id,
                 "content":[{"type":"text","text":"found seven"}]},
                {"type":"tool_result","tool_use_id":second_id,"content":"found eight"}
            ]}
        ]
    });
    let body = success(post(&client, &server, path, &followup).await).await;
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["content"], json!([{"type":"text","text":"done"}]));

    let error = post(&client, &server, path, &first).await;
    assert_eq!(error.status().as_u16(), 529);
    assert_eq!(error.headers()["retry-after"], "2");
    assert_eq!(
        error.json::<Value>().await.unwrap(),
        json!({"type":"error","error":{"type":"overloaded_error","message":"try later"}})
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    for (request, expected) in requests.iter().zip([&first, &followup, &first]) {
        assert_eq!(request.path, path);
        assert_eq!(request.headers["x-test"], "fake-llm");
        assert_eq!(&request.body, expected);
    }
}

#[tokio::test]
async fn openai_completions_and_script_updates() {
    let server = FakeLlm::start(script(json!([
        {"match":{"model_id":"other","contains":"find it"},"repeat":true,
         "response":{"type":"message","text":"wrong model"}},
        {"match":{"contains":"find it"},"response":{"type":"message",
            "thinking":"search first","tool_calls":[{"name":"search","arguments":{"query":"seven"}}]}},
        {"match":{"contains":"found seven"},"repeat":true,"response":{"type":"message","text":"done"}}
    ])))
    .await
    .unwrap();
    let client = client();
    let path = "/chat/completions";
    let first = json!({"model":"openai-fake","messages":[
        {"role":"user","content":[{"type":"text","text":"find it"}]}
    ]});
    let body = success(post(&client, &server, path, &first).await).await;
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["model"], "openai-fake");
    assert_eq!(body["usage"], json!({"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}));
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    let message = &body["choices"][0]["message"];
    assert_eq!(message["role"], "assistant");
    assert!(message["content"].is_null());
    assert_eq!(message["reasoning_content"], "search first");
    let call = &message["tool_calls"][0];
    assert_eq!(call["type"], "function");
    assert_eq!(call["function"]["name"], "search");
    assert_eq!(
        serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap()).unwrap(),
        json!({"query":"seven"})
    );

    let followup = json!({"model":"openai-fake","messages":[
        first["messages"][0],
        message,
        {"role":"tool","tool_call_id":call["id"],"content":"found seven"}
    ]});
    let versioned_path = "/v1/chat/completions";
    for _ in 0..2 {
        let body = success(post(&client, &server, versioned_path, &followup).await).await;
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
        assert_eq!(body["choices"][0]["message"]["content"], "done");
    }

    let unmatched = post(&client, &server, path, &first).await;
    assert_eq!(unmatched.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let error: Value = unmatched.json().await.unwrap();
    let diagnostic = error["error"]["message"].as_str().unwrap();
    assert!(diagnostic.contains("no fake LLM rule matched POST /chat/completions"));
    assert!(diagnostic.contains(&first.to_string()));

    let later_input = json!({"model":"openai-fake","messages":[
        followup["messages"][2],
        {"role":"user","content":"new question"},
        {"role":"assistant","content":"found seven"}
    ]});
    let unmatched = post(&client, &server, path, &later_input).await;
    assert_eq!(unmatched.status(), StatusCode::INTERNAL_SERVER_ERROR);

    server.extend_script(script(json!([
        {"response":{"type":"error","status":429,"message":"slow down"}}
    ])));
    let error = post(&client, &server, path, &first).await;
    assert_eq!(error.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(!error.headers().contains_key("retry-after"));
    assert_eq!(
        error.json::<Value>().await.unwrap(),
        json!({"error":{"type":"rate_limit_error","message":"slow down","param":null,"code":null}})
    );

    server.replace_script(script(json!([
        {"response":{"type":"message","text":"replacement"}}
    ])));
    let body = success(post(&client, &server, versioned_path, &followup).await).await;
    assert_eq!(body["choices"][0]["message"]["content"], "replacement");
    let requests = server.requests();
    assert_eq!(requests.len(), 7);
    assert_eq!(requests[0].path, path);
    assert_eq!(requests[1].path, versioned_path);
    assert_eq!(requests[1].body, followup);
}

#[tokio::test]
async fn delayed_one_shot_is_consumed_before_response() {
    let server = FakeLlm::start(script(json!([
        {"delay_ms":100,"response":{"type":"message","text":"first"}},
        {"repeat":true,"response":{"type":"message","text":"fallback"}}
    ])))
    .await
    .unwrap();
    let client = client();
    let path = "/chat/completions";
    let first = json!({"model":"first"});
    let second = json!({"model":"second"});
    let start = Instant::now();

    let delayed = async {
        let body = success(post(&client, &server, path, &first).await).await;
        assert_eq!(body["choices"][0]["message"]["content"], "first");
        assert!(start.elapsed() >= Duration::from_millis(100));
    };
    let subsequent = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while server.requests().is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("first request should be recorded");
        let body = success(post(&client, &server, path, &second).await).await;
        assert_eq!(body["choices"][0]["message"]["content"], "fallback");
    };
    tokio::join!(delayed, subsequent);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body, first);
    assert_eq!(requests[1].body, second);
}
