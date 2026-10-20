use bytes::Bytes;
use http_body_util::Full;
use hyper::{body::Incoming, service::service_fn, Request, Response, StatusCode};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
};
use std::{convert::Infallible, future::Future};
use tokio::{net::TcpListener, task::JoinHandle};

/// Starts a local HTTP server on `127.0.0.1:0` that answers every request with
/// `handler`. Returns the `http://host:port` base URL and the accept-loop task;
/// callers abort the task when the test is done.
pub(crate) async fn start_test_server<H, F>(handler: H) -> (String, JoinHandle<()>)
where
    H: Fn(Request<Incoming>) -> F + Clone + Send + Sync + 'static,
    F: Future<Output = Result<Response<Full<Bytes>>, Infallible>> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test server binds");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else { continue };
            let handler = handler.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(socket);
                let service = service_fn(handler);
                let builder = Builder::new(TokioExecutor::new());
                let _ = builder.serve_connection(io, service).await;
            });
        }
    });
    (format!("http://{addr}"), handle)
}

pub(crate) fn test_response(status: StatusCode, body: &'static [u8]) -> Response<Full<Bytes>> {
    Response::builder().status(status).body(Full::new(Bytes::from_static(body))).expect("response builds")
}
