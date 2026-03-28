use super::*;

#[tokio::test]
async fn test_health_server_responds() {
    let (tx, rx) = watch::channel(HealthStatus {
        mode: "Builder".to_string(),
        l2_head: 10,
        l1_derivation_head: 5,
        pending_submissions: 2,
        ..HealthStatus::default()
    });

    // Bind to port 0 to get an OS-assigned port
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    // Start health server in background
    let port = addr.port();
    tokio::spawn(async move {
        let _ = run_health_server(port, rx).await;
    });

    // Give the server a moment to start
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Connect and read response
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    // Send a minimal HTTP request
    use tokio::io::AsyncReadExt;
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();

    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await.unwrap();
    let response = String::from_utf8_lossy(&buf[..n]);
    assert!(response.contains("200 OK"));
    assert!(response.contains(r#""mode":"Builder""#));
    assert!(response.contains(r#""l2_head":10"#));

    // Update status and verify it changes
    let _ = tx.send(HealthStatus {
        mode: "Sync".to_string(),
        l2_head: 20,
        l1_derivation_head: 15,
        ..HealthStatus::default()
    });

    let mut stream2 = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream2
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut buf2 = vec![0u8; 4096];
    let n2 = stream2.read(&mut buf2).await.unwrap();
    let response2 = String::from_utf8_lossy(&buf2[..n2]);
    assert!(response2.contains(r#""mode":"Sync""#));
    assert!(response2.contains(r#""l2_head":20"#));
}

// --- Iteration 16: Health endpoint edge cases ---

// --- Issue #189: is_healthy() tests ---

#[test]
fn test_is_healthy_default_fresh_start() {
    // Fresh start (last_l2_head_advance = None) should be healthy
    let status = HealthStatus::default();
    assert!(status.is_healthy());
}

#[test]
fn test_is_healthy_recent_advance() {
    let status = HealthStatus {
        last_l2_head_advance: Some(Instant::now()),
        ..HealthStatus::default()
    };
    assert!(status.is_healthy());
}

#[test]
fn test_is_unhealthy_excessive_rewind_cycles() {
    // consecutive_rewind_cycles > MAX_REWIND_CYCLES (10) -> unhealthy
    let status = HealthStatus {
        consecutive_rewind_cycles: 11,
        last_l2_head_advance: Some(Instant::now()),
        ..HealthStatus::default()
    };
    assert!(!status.is_healthy());
    let json = status.to_string();
    assert!(json.contains(r#""healthy":false"#));
}

#[test]
fn test_is_unhealthy_stale_head() {
    // l2_head hasn't advanced for longer than STALENESS_THRESHOLD
    let stale_time = Instant::now() - STALENESS_THRESHOLD - std::time::Duration::from_secs(1);
    let status = HealthStatus {
        last_l2_head_advance: Some(stale_time),
        ..HealthStatus::default()
    };
    assert!(!status.is_healthy());
    let json = status.to_string();
    assert!(json.contains(r#""healthy":false"#));
}

// --- Re-run Iteration 16: additional edge cases ---

#[tokio::test]
async fn test_health_server_concurrent_requests() {
    // Multiple concurrent connections should all get valid responses.
    let (tx, rx) = watch::channel(HealthStatus {
        mode: "Fullnode".to_string(),
        l2_head: 500,
        l1_derivation_head: 490,
        pending_submissions: 1,
        ..HealthStatus::default()
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let port = addr.port();
    tokio::spawn(async move {
        let _ = run_health_server(port, rx).await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Spawn 10 concurrent requests
    let mut handles = Vec::new();
    for _ in 0..10 {
        handles.push(tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        }));
    }

    // All should succeed with valid responses
    for handle in handles {
        let response = handle.await.unwrap();
        assert!(
            response.contains("200 OK"),
            "concurrent request should succeed"
        );
        assert!(response.contains(r#""mode":"Fullnode""#));
        assert!(response.contains(r#""l2_head":500"#));
    }

    // Update status mid-flight and verify next request sees the update
    let _ = tx.send(HealthStatus {
        mode: "Builder".to_string(),
        l2_head: 501,
        ..HealthStatus::default()
    });

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await.unwrap();
    let response = String::from_utf8_lossy(&buf[..n]);
    assert!(response.contains(r#""mode":"Builder""#));
    assert!(response.contains(r#""l2_head":501"#));
}
