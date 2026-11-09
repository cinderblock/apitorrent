//! A server can't make the client buffer an answer without end.

use apitorrent_client::{ClientError, MAX_RESPONSE, Remote, Trust};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A loopback server that answers every request with `head` and then `body_bytes` bytes.
async fn server(head: String, body_bytes: usize) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let head = head.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(head.as_bytes()).await;
                let chunk = vec![b'x'; 1 << 20];
                let mut left = body_bytes;
                while left > 0 {
                    let n = left.min(chunk.len());
                    if sock.write_all(&chunk[..n]).await.is_err() {
                        return;
                    }
                    left -= n;
                }
            });
        }
    });
    addr
}

#[tokio::test]
async fn an_announced_huge_body_is_refused_unread() {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        MAX_RESPONSE + 1
    );
    let addr = server(head, 0).await;
    let remote = Remote::new(&format!("http://{addr}"), &Trust::Os, None).unwrap();
    let err = remote
        .request("GET", "/api/v1/info", None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, ClientError::TooLarge(_)), "{err:?}");
}

#[tokio::test]
async fn an_unannounced_huge_body_is_cut_off() {
    // Chunked, with no length up front: the limit is enforced while reading.
    let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
    let addr = server(head.to_owned(), MAX_RESPONSE + (1 << 20)).await;
    let remote = Remote::new(&format!("http://{addr}"), &Trust::Os, None).unwrap();
    let err = remote
        .request("GET", "/api/v1/info", None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, ClientError::TooLarge(_)), "{err:?}");
}
