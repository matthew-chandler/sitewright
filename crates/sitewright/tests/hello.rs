//! M0 smoke test: the daemon answers `hello` for any host and path.

use tokio::net::TcpListener;

#[tokio::test]
async fn answers_hello_for_any_host() {
    sw_core::crypto::install_default_provider();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(sitewright::serve(listener, async {
        let _ = stopped.await;
    }));

    let client = reqwest::Client::new();
    for host in ["anything.dev.example.com", "admin.example.com"] {
        let res = client
            .get(format!("http://{addr}/some/path"))
            .header("host", host)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.text().await.unwrap(), "hello\n");
    }

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}
