use super::*;
use std::net::Shutdown;

fn parses(request: Vec<u8>) -> bool {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    let worker =
        std::thread::spawn(move || read_request(&server, &AtomicBool::new(false)).is_some());
    // Rejection can close the connection before the client finishes writing.
    let _ = client.write_all(&request);
    let _ = client.shutdown(Shutdown::Write);
    worker.join().unwrap()
}

/// Scenario: Send ordinary fixed-length and chunked requests, then oversized framing to the loopback receiver. Valid requests parse, while oversized request, header and chunk-size lines and aggregate headers are rejected.
#[test]
fn receiver_bounds_rejects_oversized_framing() {
    assert!(parses(
        b"POST /v1/traces HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}".to_vec()
    ));
    assert!(parses(
        b"POST /v1/traces HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n"
            .to_vec()
    ));
    let long = "x".repeat(8192);
    for (case, request) in [
        ("request line", format!("POST /{long} HTTP/1.1\r\n\r\n")),
        (
            "header line",
            format!("POST / HTTP/1.1\r\nX: {long}\r\n\r\n"),
        ),
        (
            "aggregate headers",
            format!("POST / HTTP/1.1\r\n{}\r\n", "X: value\r\n".repeat(8192)),
        ),
        (
            "chunk-size line",
            format!("POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0;{long}\r\n\r\n"),
        ),
    ] {
        assert!(!parses(request.into_bytes()), "accepted oversized {case}");
    }
}

/// Scenario: Send bodies over the receiver's limit, overflowing chunk lengths and invalid chunk terminators. The receiver rejects them without accepting the request or panicking.
#[test]
fn receiver_bounds_rejects_oversized_and_malformed_bodies() {
    let body = "x".repeat(MAX_BODY_BYTES + 1);
    for request in [
        format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
        format!(
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
            body.len()
        ),
        format!(
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n{:x}\r\n",
            usize::MAX
        ),
        format!(
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n1\r\nx\r\n0\r\n\r\n",
            MAX_BODY_BYTES,
            &body[..MAX_BODY_BYTES]
        ),
        "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx!!0\r\n\r\n".to_string(),
        "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n".to_string(),
    ] {
        assert!(!parses(request.into_bytes()));
    }
}

/// Scenario: Keep a partially sent request connected and signal receiver shutdown. Its read must finish promptly without waiting for the peer to close or finish the line.
#[test]
fn receiver_bounds_stop_interrupts_incomplete_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let (send, receive) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        send.send(read_request(&server, &stopped).is_none())
            .unwrap();
    });
    client.write_all(b"POST /unfinished").unwrap();
    stop.store(true, Ordering::Relaxed);
    let result = receive.recv_timeout(Duration::from_secs(1));
    // Release the peer before asserting, so a regression does not leave a worker behind.
    let _ = client.shutdown(Shutdown::Both);
    worker.join().unwrap();
    assert!(result.unwrap(), "shutdown must interrupt the read");
}
