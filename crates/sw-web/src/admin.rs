//! The admin application, served on the admin hostname.
//!
//! Placeholder until the web UI lands (M3): a status page only.

use axum::Router;
use axum::response::Html;
use axum::routing::get;

use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(|| async { "ok\n" }))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Sitewright</title></head><body><h1>Sitewright</h1>\
         <p>The daemon is running. The admin UI arrives with milestone M3; \
         until then, use the <code>sitewright</code> command line.</p></body></html>",
    )
}
