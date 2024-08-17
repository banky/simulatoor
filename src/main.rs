use dashmap::DashMap;
use std::{env, sync::Arc};
use warp::Filter;

use simulatoor::{
    config::config, errors::handle_rejection, simulate_routes, SharedSimulationState,
};

#[tokio::main]
async fn main() {
    env::set_var("RUST_LOG", "ts::api=info");
    pretty_env_logger::init();

    let config = config();

    let port = config.port;
    let fork_url = config.fork_url.clone();

    log::info!(
        target: "ts::api",
        "Forking from {fork_url}"
    );

    let api_base = warp::path("api").and(warp::path("v1")).boxed();

    let shared_state = Arc::new(SharedSimulationState {
        evms: Arc::new(DashMap::new()),
    });

    let routes = api_base
        .and(simulate_routes(config, shared_state))
        .recover(handle_rejection)
        .with(warp::log("ts::api"));

    log::info!(
        target: "ts::api",
        "Starting server on port {port}"
    );
    warp::serve(routes).run(([0, 0, 0, 0], port)).await;
}
