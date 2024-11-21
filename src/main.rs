use axum::{
    extract::{Multipart, Path, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
    Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use thiserror::Error;
use tokio::sync::RwLock;
use tower_http::trace::TraceLayer;
use tracing::{error, info};

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Package {
    name: String,
    releases: Vec<Release>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Release {
    version: String,
    filename: String,
    upload_time: DateTime<Utc>,
}

#[derive(Error, Debug)]
enum AppError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Package not found: {0}")]
    NotFound(String),
    #[error("Invalid package format: {0}")]
    InvalidFormat(String),
    #[error("Multipart error: {0}")]
    Multipart(#[from] axum::extract::multipart::MultipartError),
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let status = match &self {
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        error!("Error: {}", self);
        status.into_response()
    }
}

#[derive(Clone)]
struct PackageIndex {
    packages: Arc<RwLock<HashMap<String, Package>>>,
    storage: PackageStorage,
}

impl PackageIndex {
    async fn new(base_path: PathBuf) -> Result<Self, AppError> {
        let storage = PackageStorage::new(base_path.clone())?;
        let packages = Arc::new(RwLock::new(storage.load_index().await?.unwrap_or_default()));

        Ok(Self { packages, storage })
    }

    async fn add_release(
        &self,
        name: String,
        version: String,
        filename: String,
    ) -> Result<(), AppError> {
        let mut packages = self.packages.write().await;
        let package = packages.entry(name.clone()).or_insert_with(|| Package {
            name: name.clone(),
            releases: Vec::new(),
        });

        package.releases.push(Release {
            version,
            filename,
            upload_time: Utc::now(),
        });

        package
            .releases
            .sort_by(|a, b| b.upload_time.cmp(&a.upload_time));
        self.storage.save_index(&packages).await?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct PackageStorage {
    base_path: PathBuf,
    packages_dir: PathBuf,
}

impl PackageStorage {
    fn new(base_path: PathBuf) -> Result<Self, AppError> {
        let packages_dir = base_path.join("packages");
        std::fs::create_dir_all(&packages_dir)?;
        std::fs::create_dir_all(&base_path)?;

        Ok(Self {
            base_path,
            packages_dir,
        })
    }

    async fn load_index(&self) -> Result<Option<HashMap<String, Package>>, AppError> {
        let index_path = self.base_path.join("index.json");
        if !index_path.exists() {
            return Ok(None);
        }

        let content = tokio::fs::read_to_string(index_path).await?;
        Ok(Some(serde_json::from_str(&content)?))
    }

    async fn save_index(&self, packages: &HashMap<String, Package>) -> Result<(), AppError> {
        let content = serde_json::to_string_pretty(packages)?;
        tokio::fs::write(self.base_path.join("index.json"), content).await?;
        Ok(())
    }

    async fn store_package(
        &self,
        name: &str,
        filename: &str,
        contents: Vec<u8>,
    ) -> Result<(), AppError> {
        let package_dir = self.packages_dir.join(name);
        tokio::fs::create_dir_all(&package_dir).await?;
        tokio::fs::write(package_dir.join(filename), contents).await?;
        Ok(())
    }
}

async fn render_html(title: &str, content: String) -> Html<String> {
    Html(format!(
        r#"<!DOCTYPE html>
<html>
<style>
    body {{
        background-color: #1e1e1e;
        color: #d4d4d4;
        font-family: Arial, sans-serif;
        margin: 0;
        padding: 0;
    }}
</style>
<head><title>{title}</title></head>
<body>
    <h1>{title}</h1>
    {content}
</body>
</html>"#
    ))
}

async fn list_packages(State(index): State<PackageIndex>) -> Result<Html<String>, AppError> {
    let packages = index.packages.read().await;
    let links = packages
        .keys()
        .map(|name| format!("<a href='/simple/{0}/'>{0}</a><br>\n", name))
        .collect();

    Ok(render_html("Package Index", links).await)
}

async fn package_details(
    State(index): State<PackageIndex>,
    Path(name): Path<String>,
) -> Result<Html<String>, AppError> {
    let packages = index.packages.read().await;
    let package = packages
        .get(&name)
        .ok_or_else(|| AppError::NotFound(name.clone()))?;

    let links = package
        .releases
        .iter()
        .map(|r| {
            format!(
                "<a href='/packages/{0}/{1}'>{1}</a> (uploaded: {}) Uploaded: {2}<br>\n",
                package.name,
                r.filename,
                r.upload_time.format("%Y-%m-%d %H:%M:%S UTC")
            )
        })
        .collect();

    Ok(render_html(&format!("{} Versions", name), links).await)
}

async fn upload_package(
    State(index): State<PackageIndex>,
    mut multipart: Multipart,
) -> Result<StatusCode, AppError> {
    while let Some(field) = multipart.next_field().await? {
        // Now this will use From<MultipartError>
        if let Some(filename) = field.file_name() {
            if !filename.ends_with(".whl") {
                continue;
            }

            let parts: Vec<&str> = filename.split('-').collect();
            if parts.len() < 2 {
                return Err(AppError::InvalidFormat(
                    "Invalid package filename format".into(),
                ));
            }

            let package_name = parts[0].to_string();
            let version = parts[1].to_string();
            // let contents = field.bytes().await?; // This will now use From<MultipartError> too
            let contents = vec![];

            index
                .storage
                .store_package(&package_name, filename, contents.to_vec())
                .await?;
            index
                .add_release(package_name.clone(), version, filename.to_string())
                .await?;

            info!("Successfully uploaded package: {}", package_name);
        }
    }

    Ok(StatusCode::OK)
}

#[tokio::main]
async fn main() -> Result<(), AppError> {
    tracing_subscriber::fmt()
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(true)
        .with_target(false)
        .init();

    // default to show all logs
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    std::env::set_var("RUST_LOG", &filter);

    let index = PackageIndex::new(PathBuf::from("data")).await?;

    let app = Router::new()
        .route(
            "/",
            get(|| async {
                Html(
                    r#"<!DOCTYPE html>
<html>
<style>
    body {
        background-color: #1e1e1e;
        color: #d4d4d4;
        font-family: Arial, sans-serif;
        margin: 0;
        padding: 0;
    }
</style>
<head><title>{title}</title></head>
<body>
    <h1>Simple PyPI Server</h1>
    <p>Use /simple/ for package listing</p>
    <p>Upload packages using POST to /upload</p>
</body>
</html>
"#,
                )
            }),
        )
        .route("/simple/", get(list_packages))
        .route("/simple/:package/", get(package_details))
        .route("/upload", post(upload_package))
        .layer(TraceLayer::new_for_http())
        .with_state(index);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();
    tracing::debug!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();

    Ok(())
}
