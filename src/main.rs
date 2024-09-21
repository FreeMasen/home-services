use std::{
    convert::Infallible,
    fmt::Display,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use axum::{
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        Html,
    },
    Router,
};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::fs::DirEntry;
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing_subscriber::{EnvFilter, FmtSubscriber};

static ERROR_HTML_TEMPLATE: &str = include_str!("error.template.html");
static INDEX_HTML_TEMPLATE: &str = include_str!("index.template.html");
const ENV_VAR_CFG_DIR: &str = "HOME_SERVICE_CFG_DIR";
static CFG_PATH: OnceLock<PathBuf> = OnceLock::new();
type ResponsePair = (StatusCode, Html<String>);

#[tokio::main]
async fn main() {
    let subscriber = FmtSubscriber::builder()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            println!("no filter in env!!");
            EnvFilter::builder()
                .parse("debug,home_services=trace")
                .inspect_err(|e| {
                    println!("Error parsing default filter: {e}");
                })
                .unwrap_or_default()
        }))
        // .with_max_level(Level::DEBUG)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");
    let cfg_path = std::env::var(ENV_VAR_CFG_DIR)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./cfg"));
    CFG_PATH.set(cfg_path).unwrap();
    let static_files_service = ServeDir::new("assets").append_index_html_on_directories(false);
    let app = Router::new()
        .route("/", axum::routing::get(index))
        .route("/index.html", axum::routing::get(index))
        .route("/sse", axum::routing::get(sse))
        .nest_service("/assets", static_files_service)
        .fallback(axum::routing::get(index))
        .layer(TraceLayer::new_for_http());
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    tracing::debug!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

async fn index() -> Result<ResponsePair, ResponsePair> {
    let cfg = read_cfg().await.map_err(|e| err(e, "reading cfg"))?;
    Ok((
        StatusCode::OK,
        Html(INDEX_HTML_TEMPLATE.replace("{{services-list}}", &cfg.as_html())),
    ))
}

async fn read_cfg() -> Result<Services, String> {
    let path = CFG_PATH
        .get()
        .ok_or_else(|| "CFG_PATH is unset!".to_string())?;
    let mut services = Services::default();
    if !path.exists() {
        tokio::fs::create_dir(path)
            .await
            .map_err(|e| format!("Error creating cfg dir: {e}"))?;
    } else {
        read_all_cfg_files(path, &mut services).await
    }
    Ok(services)
}

async fn read_all_cfg_files(base_path: impl AsRef<Path>, services: &mut Services) {
    let mut r = match tokio::fs::read_dir(base_path.as_ref()).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                "read dir failed for `{}`: {e}",
                base_path.as_ref().display()
            );
            return;
        }
    };
    loop {
        let entry: tokio::fs::DirEntry = match r.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(e) => {
                tracing::error!("Error from read-dir: {e}");
                continue;
            }
        };
        let Some(service) = read_single_cfg(entry).await else {
            continue;
        };
        services.services.push(service);
    }
}

async fn read_single_cfg(entry: DirEntry) -> Option<Service> {
    let s = tokio::fs::read_to_string(entry.path())
        .await
        .inspect_err(|e| tracing::warn!("Error reading `{}`:{e}", entry.path().display()))
        .ok()?;
    toml::from_str(&s)
        .inspect_err(|e| {
            tracing::warn!("Failed to serialize `{}`: {e}", entry.path().display());
            tracing::debug!("bad toml:\n`{s}`");
        })
        .ok()
}

fn err(e: impl Display, context: impl Display) -> ResponsePair {
    tracing::warn!("Generating error html:\n{e}\n{context}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Html(
            ERROR_HTML_TEMPLATE
                .replace("{{context}", &format!("{context}"))
                .replace("{{e}}", &format!("{e}")),
        ),
    )
}

async fn sse() -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ResponsePair> {
    tracing::debug!("GET: /sse");
    let watcher = inotify::Inotify::init().map_err(|e| {
        tracing::warn!("Error initing inotify: {e}");
        err(e, "setting up inotify")
    })?;
    watcher
        .watches()
        .add(
            CFG_PATH.get().unwrap(),
            inotify::WatchMask::CREATE | inotify::WatchMask::MODIFY,
        )
        .map_err(|e| {
            tracing::warn!(
                "error setting up inotify for cfg path `{}`: {e}",
                CFG_PATH.get().unwrap().display()
            );
            err(e, "setting up inotify for cfg path")
        })?;
    let buf = [0u8; 65_535];
    let stream = watcher
        .into_event_stream(buf)
        .map_err(|e| err(e, "watcher into event stream"))?;
    tracing::debug!("Completing sse handshake");
    Ok(Sse::new(stream.map(|_| {
        tracing::debug!("Sending update event");
        Ok(Event::default().data("update"))
    }))
    .keep_alive(KeepAlive::default()))
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Services {
    #[serde(rename = "service")]
    services: Vec<Service>,
}

impl Services {
    fn as_html(&self) -> String {
        self.services
            .iter()
            .map(Service::as_html)
            .map(|s| format!("<li>{s}</li>"))
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct Service {
    name: String,
    url: String,
    desc: String,
}

impl Service {
    fn as_html(&self) -> String {
        let Self { name, url, desc } = self;
        format!(
            r#"<article class="service-entry" onclick="goto('{url}')"><h2>{name}</h2><span>{desc}</span></article>"#
        )
    }
}
