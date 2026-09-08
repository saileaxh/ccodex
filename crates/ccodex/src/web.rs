//! Embedded frontend assets: web/dist is compiled into the binary; SPA routes fall back to index.html.

use axum::body::Body;
use axum::extract::State;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;
use std::sync::Arc;

use crate::gateway::AppState;

#[derive(RustEmbed)]
#[folder = "../../web/dist"]
struct WebAssets;

fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "map" => "application/json",
        _ => "application/octet-stream",
    }
}

fn serve_path(path: &str) -> Option<Response> {
    WebAssets::get(path).map(|content| {
        (
            [(header::CONTENT_TYPE, mime_for(path))],
            Body::from(content.data.into_owned()),
        )
            .into_response()
    })
}

pub async fn static_handler(State(_state): State<Arc<AppState>>, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    // API-looking paths that matched no route are real 404s, not SPA pages — a client
    // hitting a wrong endpoint must not receive HTML with a 200.
    const API_PREFIXES: &[&str] = &[
        "v1/",
        "admin/",
        "backend-api/",
        "responses",
        "models",
        "health",
    ];
    if API_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = if path.is_empty() { "index.html" } else { path };
    serve_path(path)
        // SPA client-side routing fallback (refreshing /accounts etc. serves index.html).
        .or_else(|| serve_path("index.html"))
        .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}
