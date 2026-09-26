use atlet_harness::appwrite_client::AppwriteClient;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[tokio::test]
async fn requests_send_the_project_and_server_key_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request = [0_u8; 4096];
        let count = socket.read(&mut request).await.expect("read");
        let request = String::from_utf8_lossy(&request[..count]);
        assert!(request.starts_with("GET /v1/tablesdb/atlet HTTP/1.1"));
        assert!(request.contains("x-appwrite-project: test-project"));
        assert!(request.contains("x-appwrite-key: test-secret"));
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 16\r\n\r\n{\"name\":\"Atlet\"}")
            .await
            .expect("respond");
    });

    let client = AppwriteClient::new(
        &format!("http://{address}/v1"),
        "test-project",
        "test-secret",
    )
    .expect("client");
    let result = client.get("tablesdb/atlet").await.expect("get");
    assert_eq!(result.expect("body")["name"], "Atlet");
    server.await.expect("server");
}
