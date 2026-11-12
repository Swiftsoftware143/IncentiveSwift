use crate::state::AppState;
use axum::{middleware, routing, Router};

pub mod handlers;

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/",
            routing::get(handlers::list_workspaces).post(handlers::create_workspace),
        )
        .route(
            "/:id",
            routing::patch(handlers::rename_workspace).delete(handlers::delete_workspace),
        )
        .route(
            "/:id/default",
            routing::post(handlers::set_default_workspace),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::access::claims::claims_middleware,
        ))
}
