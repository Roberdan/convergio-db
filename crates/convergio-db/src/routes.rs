//! HTTP API routes for convergio-db.

use axum::Router;

/// Returns the router for this crate's API endpoints.
pub fn routes() -> Router {
    Router::new()
    // .route("/api/db/health", get(health))
}
