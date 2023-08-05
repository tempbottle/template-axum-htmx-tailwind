#![forbid(unsafe_code)]
mod api_error;
mod routes;
mod state;

use std::{error, net::SocketAddr, str::FromStr, time::Duration};

use axum::{
    extract::{Path, State},
    http::{
        header::{ACCEPT, CONTENT_ENCODING, CONTENT_TYPE},
        HeaderMap, HeaderName, HeaderValue, Method, StatusCode,
    },
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use axum_extra::extract::cookie::Key;
use lib::{
    asset_cache::AssetCache, cache_control::CacheControlLayer, config::Config, leak_alloc,
    mime::MimeType,
};
use minijinja::Environment;
use routes::{
    index::{about, index},
    not_found::not_found,
};
use sqlx::PgPool;
use state::SharedState;
use tower_http::{
    compression::{predicate::SizeAbove, CompressionLayer},
    cors::CorsLayer,
    CompressionLevel,
};
use tracing_subscriber::{prelude::*, EnvFilter};

use crate::{routes::BaseTemplateData, state::AppState};

fn main() -> Result<(), Box<dyn error::Error>> {
    dotenvy::dotenv().ok();

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let config = Config::new();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(serve(&config))
}

async fn serve(config: &Config) -> Result<(), Box<dyn error::Error>> {
    let addr = SocketAddr::from_str(&format!("{}:{}", config.host, config.port))?;
    let pg = PgPool::connect(&config.postgres_url).await?;
    let assets = leak_alloc(AssetCache::load_files().await);
    let base_template_data = leak_alloc(BaseTemplateData::new(assets));
    let env = import_templates()?;

    let app_state = leak_alloc(AppState {
        pg,
        assets,
        base_template_data,
        env,
        encryption_key: Key::from(config.encryption_key.as_bytes()),
    });

    let router = Router::new()
        .merge(route_handler(app_state))
        .nest("/api", api_handler(app_state))
        .nest("/assets", static_file_handler(app_state));

    tracing::info!("Listening on {addr}");

    let listener = tokio::net::TcpListener::bind(&addr).await?;

    axum::serve(
        listener,
        router
            .layer(
                CorsLayer::new()
                    .allow_credentials(true)
                    .allow_headers([ACCEPT, CONTENT_TYPE, HeaderName::from_static("csrf-token")])
                    .max_age(Duration::from_secs(86400))
                    .allow_origin(config.cors_origin.parse::<HeaderValue>()?)
                    .allow_methods([
                        Method::GET,
                        Method::POST,
                        Method::PUT,
                        Method::DELETE,
                        Method::OPTIONS,
                        Method::HEAD,
                        Method::PATCH,
                    ]),
            )
            .layer(
                CompressionLayer::new()
                    .quality(CompressionLevel::Precise(4))
                    .compress_when(SizeAbove::new(512)),
            )
            .into_make_service(),
    )
    .await?;

    Ok(())
}

fn static_file_handler(state: SharedState) -> Router {
    const PRECOMPRESSED_MIME_TYPES: &[MimeType; 2] = &[MimeType::CSS, MimeType::JS];

    Router::new()
        .route(
            "/:file",
            get(|state: State<SharedState>, path: Path<String>| async move {
                let Some(asset) = state.assets.get_from_path(&path) else {
                    return StatusCode::NOT_FOUND.into_response();
                };

                let mut headers = HeaderMap::new();

                // We set the content type explicitly here as it will otherwise
                // be inferred as an `octet-stream`
                headers.insert(
                    CONTENT_TYPE,
                    HeaderValue::from_static(asset.content_type.as_str()),
                );

                if PRECOMPRESSED_MIME_TYPES.contains(&asset.content_type) {
                    headers.insert(CONTENT_ENCODING, HeaderValue::from_static("br"));
                }

                // `bytes::Bytes` clones are cheap
                (headers, asset.contents.clone()).into_response()
            }),
        )
        .layer(CacheControlLayer::new())
        .with_state(state)
}

fn route_handler(state: SharedState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/about", get(about))
        .route("/robots.txt", get(robots))
        .fallback(not_found)
        .with_state(state)
        .layer(CacheControlLayer::new())
}

fn api_handler(state: SharedState) -> Router {
    Router::new()
        .route("/health", get(|| async { Html("OK") }))
        .fallback(not_found)
        .with_state(state)
}

async fn robots(state: State<SharedState>) -> impl IntoResponse {
    let Some(asset) = state.assets.get("robots.txt") else {
        return StatusCode::NOT_FOUND.into_response();
    };

    if let Ok(string) = String::from_utf8(asset.contents.clone().to_vec()) {
        return string.into_response();
    }

    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

fn import_templates() -> Result<Environment<'static>, Box<dyn error::Error>> {
    let mut env = Environment::new();

    let files = std::fs::read_dir("templates")?;

    for file in files {
        let file = file?;
        let path = file.path();

        if path.is_file() {
            let path = path.to_str().expect("failed to convert path to str");

            if path.ends_with(".html") {
                let name = path.replace("templates/", "");
                let data = std::fs::read_to_string(path)?;

                env.add_template_owned(name, data)?;
            }
        }
    }

    Ok(env)
}