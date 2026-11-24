//! The listener: HTTP/1.1 only, with the limits a service in a 64 MiB sandbox needs. The accept
//! loop is ours rather than `axum::serve` so that connections are counted and a client that is
//! slow to send its request line and headers is dropped.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::Router;
use hyper1::body::Incoming;
use hyper1::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tower::ServiceExt;

/// Connections open at once (an event stream counts as one); the next is closed at once.
pub const MAX_CONNECTIONS: usize = 64;
/// The most time a client has to send the start of a request.
pub const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// The most a request line and its headers may take.
const MAX_HEAD: usize = 16 * 1024;

/// Serve `app` on `listener` until the task is dropped.
pub async fn serve(listener: TcpListener, app: Router) {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(_) => {
                // out of descriptors, or a reset while accepting: do not spin
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            drop(stream);
            continue;
        };
        let app = app.clone();
        tokio::spawn(async move {
            let service = TowerToHyperService::new(
                app.map_request(|request: hyper1::Request<Incoming>| request.map(Body::new)),
            );
            let connection = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_TIMEOUT)
                .max_buf_size(MAX_HEAD)
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades();
            let _ = connection.await;
            drop(slot);
        });
    }
}
