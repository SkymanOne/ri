//! The pi-messages, classifier and image APIs against pi-ai's own requests
//! and results, recorded by `tests/fixtures/pi/generator/models-api.mjs` into
//! `tests/fixtures/pi/models-api/cases.json`. Each case runs against a local
//! server that answers as the recording's stubbed fetch did.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use ri_ai::api::Apis;
use ri_ai::api::classify::{ClassifyOptions, classify};
use ri_ai::api::images::{ImagesOptions, generate_images};
use ri_ai::stream::{CacheRetention, Request, StreamOptions};
use ri_types::classify::{ClassifierContext, ImagesContext};
use ri_types::message::{Message, ThinkingLevel};
use ri_types::model::{ClassifierModel, ImageModel, Model};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn cases() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/pi/models-api/cases.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A request the server received: path, lowercase headers and body.
#[derive(Clone, Debug)]
struct Received {
    path: String,
    headers: IndexMap<String, String>,
    body: String,
}

type Handler = Arc<dyn Fn(&str, &Value) -> (u16, Value) + Send + Sync>;

/// Serves `handler` on a local port until the test ends.
async fn serve(handler: Handler) -> (String, Arc<Mutex<Vec<Received>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let received: Arc<Mutex<Vec<Received>>> = Arc::default();
    let log = Arc::clone(&received);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let handler = Arc::clone(&handler);
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, mut body) = loop {
                    let read = socket.read(&mut chunk).await.unwrap();
                    buffer.extend_from_slice(&chunk[..read]);
                    if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
                        break (head, buffer[end + 4..].to_vec());
                    }
                };
                let mut lines = head.split("\r\n");
                let path = lines.next().unwrap().split(' ').nth(1).unwrap().to_owned();
                let headers: IndexMap<String, String> = lines
                    .filter_map(|line| line.split_once(": "))
                    .map(|(name, value)| (name.to_lowercase(), value.to_owned()))
                    .collect();
                let length: usize = headers
                    .get("content-length")
                    .map_or(0, |value| value.parse().unwrap());
                while body.len() < length {
                    let read = socket.read(&mut chunk).await.unwrap();
                    body.extend_from_slice(&chunk[..read]);
                }
                let body = String::from_utf8(body).unwrap();
                let json = serde_json::from_str(&body).unwrap_or(Value::Null);
                let (status, reply) = handler(&path, &json);
                log.lock().unwrap().push(Received {
                    path,
                    headers,
                    body,
                });
                let (content_type, text) = match reply {
                    Value::String(text) => ("text/event-stream", text),
                    other => ("application/json", other.to_string()),
                };
                let reason = match status {
                    200 => "OK",
                    400 => "Bad Request",
                    401 => "Unauthorized",
                    429 => "Too Many Requests",
                    _ => "",
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    (url, received)
}

/// The recorded reply for every request of a case: its only one.
fn fixed(case: &Value) -> Handler {
    let status = case["status"].as_u64().unwrap() as u16;
    let body = case["reply"].clone();
    Arc::new(move |_, _| (status, body.clone()))
}

/// The path and body of each request pi made.
fn recorded(case: &Value) -> Vec<(String, String)> {
    case["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|request| {
            let url = url::Url::parse(request["url"].as_str().unwrap()).unwrap();
            (
                url.path().to_owned(),
                request["body"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn sent(received: &Arc<Mutex<Vec<Received>>>) -> Vec<(String, String)> {
    received
        .lock()
        .unwrap()
        .iter()
        .map(|request| (request.path.clone(), request.body.clone()))
        .collect()
}

fn to_value(value: &impl serde::Serialize) -> Value {
    serde_json::from_str(&ri_types::json::to_string(value).unwrap()).unwrap()
}

fn without_timestamp(mut value: Value) -> Value {
    value.as_object_mut().unwrap().remove("timestamp");
    if let Some(diagnostics) = value.get_mut("diagnostics").and_then(Value::as_array_mut) {
        for diagnostic in diagnostics {
            let object = diagnostic.as_object_mut().unwrap();
            object.remove("timestamp");
            object["details"]
                .as_object_mut()
                .unwrap()
                .remove("timestampMs");
            object["error"].as_object_mut().unwrap().remove("stack");
        }
    }
    value
}

/// Rewrites the recorded origin in `value`'s strings to the local server.
fn local(value: &Value, recorded_origin: &str, origin: &str) -> Value {
    serde_json::from_str(&value.to_string().replace(recorded_origin, origin)).unwrap()
}

#[tokio::test]
async fn pi_messages_matches_pi() {
    let cases = cases();
    let transcript: Vec<Message> = serde_json::from_value(json!([
        {"role": "system", "content": "Be brief.", "timestamp": 0, "toolsAdded": [
            {"name": "read", "description": "Read a file", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}
        ]},
        {"role": "user", "content": "hi", "timestamp": 1}
    ]))
    .unwrap();
    for (name, status, reply) in [
        ("pi_messages", 200, Value::String(sse_events())),
        (
            "pi_messages_http",
            429,
            json!({"error": {"message": "slow down", "code": "rate_limited"}}),
        ),
    ] {
        let case = &cases[name];
        let (origin, received) = serve(fixed(&json!({"status": status, "reply": reply}))).await;
        let model: Model = serde_json::from_value(json!({
            "id": "balanced", "name": "Balanced", "api": "pi-messages", "provider": "radius",
            "baseUrl": format!("{origin}/v1"), "reasoning": true, "input": ["text"],
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            "contextWindow": 200_000, "maxTokens": 32_000
        }))
        .unwrap();
        let mut options = StreamOptions {
            api_key: Some("tok".into()),
            session_id: Some("s1".into()),
            max_tokens: Some(100),
            reasoning: Some(ThinkingLevel::Low),
            cache_retention: Some(CacheRetention::Long),
            ..StreamOptions::default()
        };
        options.headers.insert("x-extra".into(), Some("1".into()));
        let message = Apis::default()
            .stream(Request {
                model,
                messages: transcript.clone(),
                options,
            })
            .result()
            .await
            .unwrap();
        let actual = without_timestamp(to_value(&Message::Assistant(Box::new(message))));
        let expected = local(&case["result"], "https://gw.example", &origin);
        assert_eq!(actual, expected, "{name}");
        assert_eq!(sent(&received), recorded(case), "{name}");
        let headers = &received.lock().unwrap()[0].headers;
        for (header, value) in case["requests"][0]["headers"].as_object().unwrap() {
            assert_eq!(
                headers.get(header),
                value.as_str().map(str::to_owned).as_ref(),
                "{name} {header}"
            );
        }
    }
}

/// The event stream of the `pi_messages` case.
fn sse_events() -> String {
    [
        r#"{"type":"start"}"#,
        r#"{"type":"thinking_start","contentIndex":0}"#,
        r#"{"type":"thinking_delta","contentIndex":0,"delta":"Let me"}"#,
        r#"{"type":"thinking_end","contentIndex":0,"content":"Let me look.","contentSignature":"sig-1"}"#,
        r#"{"type":"text_start","contentIndex":1}"#,
        r#"{"type":"text_delta","contentIndex":1,"delta":"Reading"}"#,
        r#"{"type":"text_end","contentIndex":1,"content":"Reading it."}"#,
        r#"{"type":"toolcall_start","contentIndex":2,"id":"call_1","toolName":"read"}"#,
        r#"{"type":"toolcall_delta","contentIndex":2,"delta":"{\"path\":"}"#,
        r#"{"type":"toolcall_delta","contentIndex":2,"delta":"\"a.txt\"}"}"#,
        r#"{"type":"toolcall_end","contentIndex":2,"toolCall":{"type":"toolCall","id":"call_1","name":"read","arguments":{"path":"a.txt"}}}"#,
        r#"{"type":"done","reason":"toolUse","usage":{"input":10,"output":5,"cacheRead":2,"cacheWrite":0,"totalTokens":17,"cost":{"input":0.1,"output":0.2,"cacheRead":0,"cacheWrite":0,"total":0.3}},"responseId":"resp_1","providerThinkingLevel":"low"}"#,
    ]
    .iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect()
}

fn context() -> ClassifierContext {
    serde_json::from_value(json!({
        "state": {"text": "hi"},
        "questions": {
            "tone": {"type": "choice", "instructions": "What tone?", "criteria": {"warm": "Friendly", "cold": ""}},
            "risk": {"type": "score", "instructions": "How risky?", "criteria": ["none", "some", "high"]},
            "spam": {"type": "bool", "instructions": "Is it spam?", "criteria": {"true": "Unwanted", "false": ""}}
        }
    }))
    .unwrap()
}

fn answers() -> Value {
    json!({
        "tone": {"type": "choice", "choice": "warm", "probabilities": {"warm": 0.9, "cold": 0.1}, "confidence": 0.8},
        "risk": {"type": "score", "score": 0.4, "confidence": 0.6},
        "spam": {"type": "noul", "noul": 0.05}
    })
}

fn classifier(
    api: &str,
    provider: &str,
    id: &str,
    base_url: &str,
    headers: Value,
) -> ClassifierModel {
    let mut model = json!({
        "type": "classifier", "id": id, "name": "Jev", "api": api, "provider": provider,
        "baseUrl": base_url, "input": ["text"],
        "cost": {"input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 64_000
    });
    if !headers.is_null() {
        model["headers"] = headers;
    }
    serde_json::from_value(model).unwrap()
}

#[tokio::test]
async fn system_one_classifiers_match_pi() {
    let cases = cases();
    let typesafe_answers =
        json!({"answers": answers(), "usage": {"input_tokens": 1000, "output_tokens": 10}});
    let cloudflare_answers = json!({"success": true, "result": {"state": "Completed", "result": {"answers": answers()}}});
    let runs = [
        ("typesafe", 200, typesafe_answers),
        (
            "typesafe_missing",
            200,
            json!({"answers": {"tone": answers()["tone"]}}),
        ),
        ("typesafe_http", 401, json!({"error": "bad key"})),
        ("cloudflare", 200, cloudflare_answers),
        (
            "cloudflare_failed",
            200,
            json!({"success": false, "errors": [{"message": "quota"}, {"message": "later"}]}),
        ),
        (
            "cloudflare_running",
            200,
            json!({"success": true, "result": {"state": "Running"}}),
        ),
    ];
    for (name, status, reply) in runs {
        let case = &cases[name];
        let (origin, received) = serve(fixed(&json!({"status": status, "reply": reply}))).await;
        let model = if name.starts_with("typesafe") {
            classifier(
                "typesafe-system-one",
                "typesafe",
                "jev-latest",
                &format!("{origin}/v1/"),
                json!({"x-model": "1"}),
            )
        } else {
            classifier(
                "cloudflare-workers-ai-system-one",
                "cloudflare-workers-ai",
                "@cf/typesafe/jev",
                &format!("{origin}/client/v4/accounts/acct/ai"),
                Value::Null,
            )
        };
        let mut options = ClassifyOptions {
            api_key: Some("k".into()),
            ..ClassifyOptions::default()
        };
        if name == "typesafe" {
            options.headers.insert("X-Opt".into(), Some("2".into()));
        }
        let result = classify(&model, &context(), &options).await;
        let actual = without_timestamp(to_value(&result));
        assert_eq!(actual, case["result"], "{name}");
        assert_eq!(sent(&received), recorded(case), "{name}");
        let headers = &received.lock().unwrap()[0].headers;
        for (header, value) in case["requests"][0]["headers"].as_object().unwrap() {
            assert_eq!(
                headers.get(header),
                value.as_str().map(str::to_owned).as_ref(),
                "{name} {header}"
            );
        }
    }
}

#[tokio::test]
async fn llama_classifier_matches_pi() {
    let case = &cases()["llama"];
    let vocabulary: IndexMap<&str, Vec<i64>> = [
        ("\n", vec![10]),
        ("\nA", vec![10, 65]),
        ("\nB", vec![10, 66]),
        ("\nYes", vec![10, 900]),
        ("\nNo", vec![10, 901]),
        ("\n0", vec![10, 48]),
        ("\n1", vec![10, 49]),
        ("\n2", vec![10, 50]),
    ]
    .into_iter()
    .collect();
    let handler: Handler = Arc::new(move |path, body| {
        if path.ends_with("/tokenize") {
            let tokens = vocabulary
                .get(body["content"].as_str().unwrap())
                .cloned()
                .unwrap_or_else(|| vec![1, 2]);
            return (200, json!({ "tokens": tokens }));
        }
        if path.ends_with("/apply-template") {
            // JavaScript string length: UTF-16 code units.
            let length = body["messages"][1]["content"]
                .as_str()
                .unwrap()
                .encode_utf16()
                .count();
            return (
                200,
                json!({ "prompt": format!("<|im_start|>{length}<think>") }),
            );
        }
        let top: Vec<Value> = [
            (65, -0.2),
            (66, -1.8),
            (900, -3.0),
            (901, -0.05),
            (48, -1.0),
            (49, -0.7),
            (50, -2.5),
        ]
        .iter()
        .map(|(id, logprob)| json!({"id": id, "logprob": logprob}))
        .collect();
        (
            200,
            json!({"completion_probabilities": [{"top_logprobs": top}]}),
        )
    });
    let (origin, received) = serve(handler).await;
    let mut model = classifier(
        "llama-cpp-classify",
        "llama.cpp",
        "qwen",
        &format!("{origin}/v1"),
        Value::Null,
    );
    model.name = "qwen".into();
    model.context_window = 4096;
    model.cost.input = 0.0;
    model.cost.output = 0.0;
    let options = ClassifyOptions {
        api_key: Some("local".into()),
        temperature: Some(2.0),
        ..ClassifyOptions::default()
    };
    let result = classify(&model, &context(), &options).await;
    assert_eq!(without_timestamp(to_value(&result)), case["result"]);
    // The label lookups run concurrently, so compare the requests as sets.
    let mut actual = sent(&received);
    let mut expected = recorded(case);
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn openrouter_images_match_pi() {
    let cases = cases();
    let reply = json!({
        "id": "gen-1",
        "choices": [{"message": {"role": "assistant", "content": "Here", "images": [
            {"image_url": {"url": "data:image/png;base64,QUJD"}},
            {"image_url": "https://x/y.png"}
        ]}}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 1290, "prompt_tokens_details": {"cached_tokens": 20, "cache_write_tokens": 5}}
    });
    for (name, status, reply, input) in [
        (
            "images",
            200,
            reply,
            json!([{"type": "text", "text": "a cat"}, {"type": "image", "data": "AAA", "mimeType": "image/png"}]),
        ),
        (
            "images_http",
            400,
            json!({"error": {"message": "No endpoints", "code": 400}}),
            json!([{"type": "text", "text": "a cat"}]),
        ),
    ] {
        let case = &cases[name];
        let (origin, received) = serve(fixed(&json!({"status": status, "reply": reply}))).await;
        let model: ImageModel = serde_json::from_value(json!({
            "type": "image", "id": "google/gemini-2.5-flash-image", "name": "Nano",
            "api": "openrouter-images", "provider": "openrouter", "baseUrl": format!("{origin}/api/v1"),
            "input": ["text", "image"], "output": ["image", "text"],
            "cost": {"input": 0.3, "output": 2.5, "cacheRead": 0.03, "cacheWrite": 0.08}
        }))
        .unwrap();
        let context: ImagesContext = serde_json::from_value(json!({ "input": input })).unwrap();
        let options = ImagesOptions {
            api_key: Some("k".into()),
            ..ImagesOptions::default()
        };
        let result = generate_images(&model, &context, &options).await;
        assert_eq!(
            without_timestamp(to_value(&result)),
            case["result"],
            "{name}"
        );
        assert_eq!(sent(&received), recorded(case), "{name}");
        let headers = &received.lock().unwrap()[0].headers;
        for header in ["authorization", "content-type", "accept"] {
            assert_eq!(
                headers.get(header).map(String::as_str),
                case["requests"][0]["headers"][header].as_str(),
                "{name} {header}"
            );
        }
    }
}
