//! End-to-end tests: real HTTP, real sockets, mock upstreams.

mod support;

use std::time::{Duration, Instant};

use axum::body::Body;
use http_body_util::BodyExt;
use support::*;

#[tokio::test]
async fn forwards_a_chat_completion_and_names_the_upstream() {
    let mock = start_mock("board-a", &["qwen2.5:0.5b"]).await;
    let addr = start_router(&format!(
        r#"
        [health]
        interval_secs = 0
        [[upstream]]
        name = "board-a"
        url = "{}"
        models = ["qwen2.5:0.5b"]
        "#,
        mock.base_url()
    ))
    .await;

    let res = chat(addr, "qwen2.5:0.5b").await;
    assert_eq!(res.status, 200, "body: {}", res.body);
    assert_eq!(res.upstream(), Some("board-a"));
    assert_eq!(res.json()["served_by"], "board-a");
    assert_eq!(mock.hits(), 1);
}

#[tokio::test]
async fn injects_the_upstream_api_key_and_hides_the_client_one() {
    let mock = start_mock("board-a", &[]).await;
    let addr = start_router(&format!(
        r#"
        [health]
        interval_secs = 0
        [server.auth]
        require_auth = true
        api_keys = ["sk-client"]
        [[upstream]]
        name = "board-a"
        url = "{}"
        api_key = "sk-upstream"
        "#,
        mock.base_url()
    ))
    .await;

    let res = request(
        "POST",
        &format!("http://{addr}/v1/chat/completions"),
        Some(serde_json::json!({"model": "m", "messages": []})),
        Some("sk-client"),
    )
    .await;

    assert_eq!(res.status, 200);
    // The upstream must see its own credential, never the client's.
    assert_eq!(mock.last_auth().as_deref(), Some("Bearer sk-upstream"));
}

#[tokio::test]
async fn rejects_requests_without_a_valid_key() {
    let mock = start_mock("board-a", &[]).await;
    let addr = start_router(&format!(
        r#"
        [health]
        interval_secs = 0
        [server.auth]
        require_auth = true
        api_keys = ["sk-good"]
        [[upstream]]
        name = "board-a"
        url = "{}"
        "#,
        mock.base_url()
    ))
    .await;

    let missing = chat(addr, "m").await;
    assert_eq!(missing.status, 401);
    assert_eq!(missing.json()["error"]["type"], "authentication_error");

    let wrong = request(
        "POST",
        &format!("http://{addr}/v1/chat/completions"),
        Some(serde_json::json!({"model": "m", "messages": []})),
        Some("sk-bad"),
    )
    .await;
    assert_eq!(wrong.status, 401);
    assert_eq!(
        mock.hits(),
        0,
        "an unauthorized request must not reach an upstream"
    );

    let good = request(
        "POST",
        &format!("http://{addr}/v1/chat/completions"),
        Some(serde_json::json!({"model": "m", "messages": []})),
        Some("sk-good"),
    )
    .await;
    assert_eq!(good.status, 200);
    assert_eq!(mock.hits(), 1);
}

#[tokio::test]
async fn fails_over_to_the_next_upstream() {
    let bad = start_mock("broken", &["shared"]).await;
    let good = start_mock("working", &["shared"]).await;
    bad.set_status(503);

    let addr = start_router(&format!(
        r#"
        [health]
        interval_secs = 0
        [balance]
        strategy = "first-available"
        retries = 2
        [[upstream]]
        name = "broken"
        url = "{}"
        models = ["shared"]
        [[upstream]]
        name = "working"
        url = "{}"
        models = ["shared"]
        "#,
        bad.base_url(),
        good.base_url()
    ))
    .await;

    let res = chat(addr, "shared").await;
    assert_eq!(res.status, 200, "body: {}", res.body);
    assert_eq!(res.upstream(), Some("working"));
    assert_eq!(
        bad.hits(),
        1,
        "the broken upstream should have been tried first"
    );
    assert_eq!(good.hits(), 1);
}

#[tokio::test]
async fn reports_upstream_failure_when_every_attempt_fails() {
    let a = start_mock("a", &["shared"]).await;
    let b = start_mock("b", &["shared"]).await;
    a.set_status(503);
    b.set_status(503);

    let addr = start_router(&format!(
        r#"
        [health]
        interval_secs = 0
        [balance]
        retries = 3
        [[upstream]]
        name = "a"
        url = "{}"
        models = ["shared"]
        [[upstream]]
        name = "b"
        url = "{}"
        models = ["shared"]
        "#,
        a.base_url(),
        b.base_url()
    ))
    .await;

    let res = chat(addr, "shared").await;
    assert_eq!(res.status, 503);
    let body = res.json();
    assert_eq!(body["error"]["type"], "upstream_unavailable");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("503"),
        "message should name the upstream status: {body}"
    );
}

#[tokio::test]
async fn refuses_a_model_no_upstream_serves() {
    let mock = start_mock("board-a", &["qwen2.5:0.5b"]).await;
    let addr = start_router(&format!(
        r#"
        [health]
        interval_secs = 0
        [[upstream]]
        name = "board-a"
        url = "{}"
        models = ["qwen2.5:0.5b"]
        "#,
        mock.base_url()
    ))
    .await;

    let res = chat(addr, "llama3.1:405b").await;
    assert_eq!(res.status, 503);
    assert!(
        res.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("llama3.1:405b"),
        "error should name the model: {}",
        res.body
    );
    assert_eq!(mock.hits(), 0);
}

#[tokio::test]
async fn aggregates_models_across_upstreams_and_advertises_aliases() {
    let a = start_mock("a", &["qwen2.5:0.5b", "shared"]).await;
    let b = start_mock("b", &["smollm2:135m", "shared"]).await;

    let addr = start_router(&format!(
        r#"
        [health]
        interval_secs = 0
        [alias]
        "gpt-3.5-turbo" = "qwen2.5:0.5b"
        "nonexistent-alias" = "not-served-anywhere"
        [[upstream]]
        name = "a"
        url = "{}"
        models = ["qwen2.5:0.5b", "shared"]
        [[upstream]]
        name = "b"
        url = "{}"
        models = ["smollm2:135m", "shared"]
        "#,
        a.base_url(),
        b.base_url()
    ))
    .await;

    let res = get(&format!("http://{addr}/v1/models")).await;
    assert_eq!(res.status, 200);
    let body = res.json();
    assert_eq!(body["object"], "list");
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();

    assert_eq!(
        ids,
        vec!["gpt-3.5-turbo", "qwen2.5:0.5b", "shared", "smollm2:135m"],
        "catalogue should be the sorted union plus resolvable aliases"
    );
    assert!(
        !ids.contains(&"nonexistent-alias"),
        "an alias pointing at nothing must not be advertised"
    );

    // `shared` lives on both boards; the extension field should say so.
    let shared = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "shared")
        .unwrap();
    let ups: Vec<&str> = shared["upstreams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u.as_str().unwrap())
        .collect();
    assert_eq!(ups, vec!["a", "b"]);

    // Single-model lookup, and a 404 for something nobody serves.
    let one = get(&format!("http://{addr}/v1/models/shared")).await;
    assert_eq!(one.status, 200);
    assert_eq!(one.json()["id"], "shared");
    let missing = get(&format!("http://{addr}/v1/models/nope")).await;
    assert_eq!(missing.status, 404);
}

#[tokio::test]
async fn rewrites_an_alias_before_forwarding() {
    let mock = start_mock("board-a", &["qwen2.5:0.5b"]).await;
    let addr = start_router(&format!(
        r#"
        [health]
        interval_secs = 0
        [alias]
        "gpt-3.5-turbo" = "qwen2.5:0.5b"
        [[upstream]]
        name = "board-a"
        url = "{}"
        models = ["qwen2.5:0.5b"]
        "#,
        mock.base_url()
    ))
    .await;

    let res = chat(addr, "gpt-3.5-turbo").await;
    assert_eq!(res.status, 200, "body: {}", res.body);
    assert_eq!(
        mock.last_model().as_deref(),
        Some("qwen2.5:0.5b"),
        "the upstream should receive the real model name, not the alias"
    );
}

#[tokio::test]
async fn streams_without_buffering_the_response() {
    let mock = start_mock("board-a", &[]).await;
    let addr = start_router(&format!(
        r#"
        [health]
        interval_secs = 0
        [[upstream]]
        name = "board-a"
        url = "{}"
        "#,
        mock.base_url()
    ))
    .await;

    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("http://{addr}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({"model": "m", "messages": [], "stream": true}).to_string(),
        ))
        .unwrap();

    let started = Instant::now();
    let resp = client().request(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/event-stream"
    );

    let mut body = resp.into_body();
    let mut first_chunk_at = None;
    let mut collected = String::new();
    let mut frames = 0;
    while let Some(frame) = body.frame().await {
        let frame = frame.unwrap();
        if let Some(data) = frame.data_ref() {
            if first_chunk_at.is_none() {
                first_chunk_at = Some(started.elapsed());
            }
            frames += 1;
            collected.push_str(&String::from_utf8_lossy(data));
        }
    }

    let total = started.elapsed();
    let first = first_chunk_at.expect("at least one chunk should arrive");

    assert!(collected.contains("tok0"), "got: {collected}");
    assert!(collected.contains("[DONE]"), "got: {collected}");
    assert!(frames > 1, "expected multiple frames, got {frames}");
    // The mock spaces its chunks out. If the router buffered the body, the
    // first chunk would land at the same time as the last one.
    assert!(
        first < total / 2,
        "first chunk at {first:?} but stream finished at {total:?}: body looks buffered"
    );
}

#[tokio::test]
async fn max_concurrency_is_enforced_per_upstream() {
    let mock = start_mock("small-board", &[]).await;
    mock.set_delay(Duration::from_millis(150));

    let addr = start_router(&format!(
        r#"
        [health]
        interval_secs = 0
        [server]
        queue_timeout_secs = 30
        [[upstream]]
        name = "small-board"
        url = "{}"
        max_concurrency = 1
        "#,
        mock.base_url()
    ))
    .await;

    let mut tasks = Vec::new();
    for _ in 0..4 {
        tasks.push(tokio::spawn(async move { chat(addr, "m").await }));
    }
    for t in tasks {
        let res = t.await.unwrap();
        assert_eq!(res.status, 200, "body: {}", res.body);
    }

    assert_eq!(mock.hits(), 4);
    assert_eq!(
        mock.peak_inflight(),
        1,
        "max_concurrency = 1 must serialise requests, peak was {}",
        mock.peak_inflight()
    );
}

#[tokio::test]
async fn queue_timeout_sheds_load_instead_of_hanging() {
    let mock = start_mock("slow-board", &[]).await;
    mock.set_delay(Duration::from_millis(600));

    let addr = start_router(&format!(
        r#"
        [health]
        interval_secs = 0
        [server]
        queue_timeout_secs = 1
        [balance]
        retries = 0
        [[upstream]]
        name = "slow-board"
        url = "{}"
        max_concurrency = 1
        "#,
        mock.base_url()
    ))
    .await;

    // Three requests, each taking 600 ms, against one slot and a 1 s queue:
    // the last one cannot possibly be served in time.
    let tasks: Vec<_> = (0..3)
        .map(|_| tokio::spawn(async move { chat(addr, "m").await }))
        .collect();
    let mut statuses: Vec<u16> = Vec::new();
    for t in tasks {
        statuses.push(t.await.unwrap().status.as_u16());
    }
    statuses.sort_unstable();
    assert_eq!(
        statuses,
        vec![200, 200, 429],
        "expected the third request to be shed with 429"
    );
}

#[tokio::test]
async fn fallback_only_upstream_is_held_back() {
    let primary = start_mock("primary", &["shared"]).await;
    let cloud = start_mock("cloud", &["shared", "premium"]).await;

    let addr = start_router(&format!(
        r#"
        [health]
        interval_secs = 0
        [[upstream]]
        name = "primary"
        url = "{}"
        models = ["shared"]
        [[upstream]]
        name = "cloud"
        url = "{}"
        models = ["shared", "premium"]
        fallback_only = true
        "#,
        primary.base_url(),
        cloud.base_url()
    ))
    .await;

    // A model both can serve goes to the local board.
    for _ in 0..5 {
        let res = chat(addr, "shared").await;
        assert_eq!(res.upstream(), Some("primary"));
    }
    assert_eq!(cloud.hits(), 0);

    // A model only the fallback serves reaches it.
    let res = chat(addr, "premium").await;
    assert_eq!(res.status, 200);
    assert_eq!(res.upstream(), Some("cloud"));
    assert_eq!(cloud.hits(), 1);
}

#[tokio::test]
async fn health_probing_discovers_models_and_removes_dead_boards() {
    let mock = start_mock("board-a", &["discovered-model"]).await;
    let addr = start_router(&format!(
        r#"
        [health]
        interval_secs = 1
        timeout_secs = 1
        failure_threshold = 1
        success_threshold = 1
        cooldown_secs = 30
        [[upstream]]
        name = "board-a"
        url = "{}"
        "#,
        mock.base_url()
    ))
    .await;

    let models_url = format!("http://{addr}/v1/models");
    wait_until("model discovery", Duration::from_secs(10), || {
        let url = models_url.clone();
        async move { get(&url).await.body.contains("discovered-model") }
    })
    .await;

    // Kill the board and wait for it to drop out of rotation.
    mock.set_health_status(500);
    let ready_url = format!("http://{addr}/readyz");
    wait_until(
        "upstream to be taken out of rotation",
        Duration::from_secs(10),
        || {
            let url = ready_url.clone();
            async move { get(&url).await.status == 503 }
        },
    )
    .await;

    let admin = get(&format!("http://{addr}/admin/upstreams")).await;
    assert_eq!(admin.json()["upstreams"][0]["health"], "down");
    assert!(admin.json()["upstreams"][0]["last_error"]
        .as_str()
        .unwrap()
        .contains("500"));
}

#[tokio::test]
async fn admin_and_metrics_report_traffic() {
    let mock = start_mock("board-a", &["m"]).await;
    let addr = start_router(&format!(
        r#"
        [health]
        interval_secs = 0
        [balance]
        strategy = "least-conn"
        [[upstream]]
        name = "board-a"
        url = "{}"
        models = ["m"]
        max_concurrency = 3
        "#,
        mock.base_url()
    ))
    .await;

    assert_eq!(get(&format!("http://{addr}/healthz")).await.status, 200);
    assert_eq!(get(&format!("http://{addr}/readyz")).await.status, 200);

    for _ in 0..3 {
        assert_eq!(chat(addr, "m").await.status, 200);
    }

    let metrics = get(&format!("http://{addr}/metrics")).await;
    assert_eq!(metrics.status, 200);
    assert!(
        metrics.body.contains("mini_router_requests_total 3"),
        "{}",
        metrics.body
    );
    assert!(metrics
        .body
        .contains("mini_router_responses_total{class=\"2xx\"} 3"));
    assert!(metrics
        .body
        .contains("mini_router_upstream_requests_total{upstream=\"board-a\"} 3"));
    assert!(metrics
        .body
        .contains("mini_router_upstream_up{upstream=\"board-a\"} 1"));
    assert!(metrics
        .body
        .contains("mini_router_upstream_inflight{upstream=\"board-a\"} 0"));

    let admin = get(&format!("http://{addr}/admin/upstreams")).await;
    let body = admin.json();
    assert_eq!(body["strategy"], "least-conn");
    assert_eq!(body["upstreams"][0]["name"], "board-a");
    assert_eq!(body["upstreams"][0]["total_requests"], 3);
    assert_eq!(body["upstreams"][0]["max_concurrency"], 3);
    assert_eq!(body["upstreams"][0]["health"], "up");
}

#[tokio::test]
async fn round_robin_spreads_traffic_evenly() {
    let a = start_mock("a", &["m"]).await;
    let b = start_mock("b", &["m"]).await;
    let addr = start_router(&format!(
        r#"
        [health]
        interval_secs = 0
        [balance]
        strategy = "round-robin"
        [[upstream]]
        name = "a"
        url = "{}"
        models = ["m"]
        [[upstream]]
        name = "b"
        url = "{}"
        models = ["m"]
        "#,
        a.base_url(),
        b.base_url()
    ))
    .await;

    for _ in 0..10 {
        assert_eq!(chat(addr, "m").await.status, 200);
    }
    assert_eq!(a.hits(), 5);
    assert_eq!(b.hits(), 5);
}

#[tokio::test]
async fn unknown_endpoints_and_oversized_bodies_are_rejected() {
    let mock = start_mock("board-a", &[]).await;
    let addr = start_router(&format!(
        r#"
        [health]
        interval_secs = 0
        [server]
        max_body_bytes = 1024
        [[upstream]]
        name = "board-a"
        url = "{}"
        "#,
        mock.base_url()
    ))
    .await;

    let unknown = get(&format!("http://{addr}/not-a-thing")).await;
    assert_eq!(unknown.status, 404);
    assert_eq!(unknown.json()["error"]["type"], "not_found_error");

    let huge = request(
        "POST",
        &format!("http://{addr}/v1/chat/completions"),
        Some(serde_json::json!({"model": "m", "prompt": "x".repeat(4096)})),
        None,
    )
    .await;
    assert_eq!(huge.status, 413);
    assert_eq!(mock.hits(), 0);
}

#[tokio::test]
async fn forwards_endpoints_it_does_not_know_about() {
    let mock = start_mock("board-a", &[]).await;
    let addr = start_router(&format!(
        r#"
        [health]
        interval_secs = 0
        [[upstream]]
        name = "board-a"
        url = "{}"
        "#,
        mock.base_url()
    ))
    .await;

    let res = request(
        "POST",
        &format!("http://{addr}/v1/embeddings"),
        Some(serde_json::json!({"model": "embed-me", "input": "hello"})),
        None,
    )
    .await;
    assert_eq!(res.status, 200, "body: {}", res.body);
    assert_eq!(res.upstream(), Some("board-a"));
    assert_eq!(mock.last_model().as_deref(), Some("embed-me"));
}
