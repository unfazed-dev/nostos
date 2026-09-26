use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{extract::State, http::HeaderMap, routing::post, Json, Router};
use serde_json::{json, Value};

type Observations = Arc<Mutex<Vec<(String, Value)>>>;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind test port")
        .local_addr()
        .expect("test address")
        .port()
}

#[tokio::test]
async fn appwrite_gateway_forwards_only_authenticated_sync_to_fixed_function() {
    let observed: Observations = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route(
            "/functions/atlet_sync/executions",
            post(
                |State(observed): State<Observations>,
                 headers: HeaderMap,
                 Json(body): Json<Value>| async move {
                    assert_eq!(headers["x-appwrite-project"], "test-project");
                    let jwt = headers["x-appwrite-jwt"]
                        .to_str()
                        .expect("forwarded JWT")
                        .to_string();
                    observed
                        .lock()
                        .expect("observations")
                        .push((jwt.clone(), body));
                    Json(json!({
                        "responseStatusCode": 200,
                        "responseBody": "{\"head\":\"0\"}",
                        "requestHeaders": {"x-appwrite-jwt": jwt}
                    }))
                },
            ),
        )
        .with_state(Arc::clone(&observed));
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("upstream listener");
    let upstream_port = upstream.local_addr().expect("upstream address").port();
    let upstream_task = tokio::spawn(async move {
        axum::serve(upstream, app).await.expect("upstream server");
    });

    let gateway_port = free_port();
    let mut gateway = tokio::process::Command::new(env!("CARGO_BIN_EXE_nostos-server"))
        .env("NOSTOS_BACKEND", "appwrite")
        .env("NOSTOS_BIND", format!("127.0.0.1:{gateway_port}"))
        .env(
            "NOSTOS_APPWRITE_ENDPOINT",
            format!("http://127.0.0.1:{upstream_port}"),
        )
        .env("NOSTOS_APPWRITE_PROJECT_ID", "test-project")
        .env("NOSTOS_APPWRITE_FUNCTION_ID", "atlet_sync")
        .env("NOSTOS_CORS_ORIGINS", "http://127.0.0.1:8765")
        .kill_on_drop(true)
        .spawn()
        .expect("gateway process");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("test client");
    let base = format!("http://127.0.0.1:{gateway_port}");
    let mut ready = false;
    for _ in 0..40 {
        if client.get(format!("{base}/healthz")).send().await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ready, "gateway did not start");

    let unsigned = client
        .post(format!("{base}/appwrite/sync/pull"))
        .json(&json!({"after":"0","limit":100}))
        .send()
        .await
        .expect("unsigned request");
    assert_eq!(unsigned.status(), reqwest::StatusCode::UNAUTHORIZED);
    let preflight = client
        .request(
            reqwest::Method::OPTIONS,
            format!("{base}/appwrite/sync/pull"),
        )
        .header("Origin", "http://127.0.0.1:8765")
        .header("Access-Control-Request-Method", "POST")
        .header(
            "Access-Control-Request-Headers",
            "authorization,content-type",
        )
        .send()
        .await
        .expect("browser preflight");
    assert!(preflight.status().is_success());
    assert_eq!(
        preflight.headers()["access-control-allow-origin"],
        "http://127.0.0.1:8765"
    );
    let signed = client
        .post(format!("{base}/appwrite/sync/pull"))
        .header("Origin", "http://127.0.0.1:8765")
        .bearer_auth("test-jwt")
        .json(&json!({"after":"0","limit":100}))
        .send()
        .await
        .expect("signed request");
    assert_eq!(signed.status(), reqwest::StatusCode::OK);
    let envelope = signed.json::<Value>().await.expect("envelope");
    assert_eq!(envelope["responseStatusCode"], 200);
    assert!(
        envelope.get("requestHeaders").is_none(),
        "JWT must not be echoed"
    );
    let push = client
        .post(format!("{base}/appwrite/sync/push"))
        .bearer_auth("test-jwt")
        .json(&json!({"mutation_id":"fixture","table":"sessions"}))
        .send()
        .await
        .expect("push request");
    assert_eq!(push.status(), reqwest::StatusCode::OK);
    let other = client
        .post(format!("{base}/appwrite/sync/other"))
        .bearer_auth("test-jwt")
        .json(&json!({}))
        .send()
        .await
        .expect("other route");
    assert_eq!(other.status(), reqwest::StatusCode::NOT_FOUND);
    {
        let calls = observed.lock().expect("observations");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "test-jwt");
        assert_eq!(calls[0].1["path"], "/sync/pull");
        assert_eq!(calls[0].1["method"], "POST");
        assert_eq!(calls[0].1["body"], "{\"after\":\"0\",\"limit\":100}");
        assert_eq!(calls[1].1["path"], "/sync/push");
    }
    gateway.kill().await.expect("stop gateway");
    upstream_task.abort();
}
