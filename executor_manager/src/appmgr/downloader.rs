/*
Copyright 2025 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at
    http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use tonic::transport::ClientTlsConfig;

use common::FlameError;

const HTTP_TIMEOUT_SECS: u64 = 300;
const GRPC_CONNECT_TIMEOUT_SECS: u64 = 30;

#[async_trait]
pub trait PackageDownloader: Send + Sync {
    async fn download(&self, url: &url::Url, dest_path: &Path) -> Result<(), FlameError>;
}

pub struct FileDownloader;

#[async_trait]
impl PackageDownloader for FileDownloader {
    async fn download(&self, url: &url::Url, dest_path: &Path) -> Result<(), FlameError> {
        let src_path = url
            .to_file_path()
            .map_err(|_| FlameError::InvalidConfig(format!("invalid file url: {}", url)))?;
        tokio::fs::copy(&src_path, dest_path).await.map_err(|e| {
            FlameError::Internal(format!(
                "failed to copy package from {}: {}",
                src_path.display(),
                e
            ))
        })?;
        Ok(())
    }
}

pub struct HttpDownloader {
    timeout: Duration,
}

impl HttpDownloader {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

#[async_trait]
impl PackageDownloader for HttpDownloader {
    async fn download(&self, url: &url::Url, dest_path: &Path) -> Result<(), FlameError> {
        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .build()
            .map_err(|e| FlameError::Internal(format!("failed to create HTTP client: {}", e)))?;

        let response = client
            .get(url.as_str())
            .send()
            .await
            .map_err(|e| FlameError::Internal(format!("failed to download package: {}", e)))?;

        if !response.status().is_success() {
            return Err(FlameError::Internal(format!(
                "failed to download package: HTTP {}",
                response.status()
            )));
        }

        let temp_path = dest_path.with_extension("tmp");
        let mut file = tokio::fs::File::create(&temp_path)
            .await
            .map_err(|e| FlameError::Internal(format!("failed to create temp file: {}", e)))?;

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|e| FlameError::Internal(format!("failed to read chunk: {}", e)))?;
            file.write_all(&chunk)
                .await
                .map_err(|e| FlameError::Internal(format!("failed to write chunk: {}", e)))?;
        }

        file.sync_all()
            .await
            .map_err(|e| FlameError::Internal(format!("failed to sync file: {}", e)))?;
        drop(file);

        tokio::fs::rename(&temp_path, dest_path)
            .await
            .map_err(|e| FlameError::Internal(format!("failed to rename temp file: {}", e)))?;

        Ok(())
    }
}

pub struct GrpcDownloader {
    connect_timeout: Duration,
    tls_config: Option<ClientTlsConfig>,
}

impl GrpcDownloader {
    pub fn new(connect_timeout: Duration) -> Self {
        Self {
            connect_timeout,
            tls_config: None,
        }
    }

    pub fn with_tls(connect_timeout: Duration, tls_config: ClientTlsConfig) -> Self {
        Self {
            connect_timeout,
            tls_config: Some(tls_config),
        }
    }
}

#[async_trait]
impl PackageDownloader for GrpcDownloader {
    async fn download(&self, url: &url::Url, dest_path: &Path) -> Result<(), FlameError> {
        use arrow::array::{Array, BinaryArray};
        use arrow_flight::FlightClient;
        use futures_util::TryStreamExt;
        use tonic::transport::Channel;

        let host = url
            .host_str()
            .ok_or_else(|| FlameError::InvalidConfig("missing host in grpc URL".to_string()))?;
        let port = url.port().unwrap_or(9090);

        let key = url.path().trim_start_matches('/');

        let channel = if url.scheme() == "grpcs" {
            let endpoint = format!("https://{}:{}", host, port);
            let mut channel_builder = Channel::from_shared(endpoint)
                .map_err(|e| FlameError::Internal(format!("invalid endpoint: {}", e)))?
                .connect_timeout(self.connect_timeout);

            if let Some(ref tls_config) = self.tls_config {
                channel_builder = channel_builder
                    .tls_config(tls_config.clone())
                    .map_err(|e| FlameError::Internal(format!("TLS config error: {}", e)))?;
            }

            channel_builder.connect().await.map_err(|e| {
                FlameError::Internal(format!("failed to connect to cache (TLS): {}", e))
            })?
        } else {
            let endpoint = format!("http://{}:{}", host, port);
            Channel::from_shared(endpoint)
                .map_err(|e| FlameError::Internal(format!("invalid endpoint: {}", e)))?
                .connect_timeout(self.connect_timeout)
                .connect()
                .await
                .map_err(|e| FlameError::Internal(format!("failed to connect to cache: {}", e)))?
        };

        let mut client = FlightClient::new(channel);

        let ticket = arrow_flight::Ticket::new(format!("{}:0", key));
        let mut stream = client
            .do_get(ticket)
            .await
            .map_err(|e| FlameError::Internal(format!("do_get failed: {}", e)))?;

        let temp_path = dest_path.with_extension("tmp");
        let mut file = tokio::fs::File::create(&temp_path)
            .await
            .map_err(|e| FlameError::Internal(format!("failed to create temp file: {}", e)))?;

        let mut total_size = 0usize;
        while let Some(batch) = stream
            .try_next()
            .await
            .map_err(|e| FlameError::Internal(format!("stream error: {}", e)))?
        {
            if let Some(array) = batch.column_by_name("data") {
                if let Some(binary_array) = array.as_any().downcast_ref::<BinaryArray>() {
                    for i in 0..binary_array.len() {
                        let chunk = binary_array.value(i);
                        file.write_all(chunk).await.map_err(|e| {
                            FlameError::Internal(format!("failed to write chunk: {}", e))
                        })?;
                        total_size += chunk.len();
                    }
                }
            }
        }

        if total_size == 0 {
            tokio::fs::remove_file(&temp_path).await.ok();
            return Err(FlameError::Internal(format!("object not found: {}", key)));
        }

        file.sync_all()
            .await
            .map_err(|e| FlameError::Internal(format!("failed to sync file: {}", e)))?;
        drop(file);

        tokio::fs::rename(&temp_path, dest_path)
            .await
            .map_err(|e| FlameError::Internal(format!("failed to rename temp file: {}", e)))?;

        tracing::info!(
            "Downloaded package via gRPC: {} ({} bytes)",
            key,
            total_size
        );
        Ok(())
    }
}

pub struct DownloaderRegistry {
    downloaders: HashMap<String, Box<dyn PackageDownloader>>,
}

impl DownloaderRegistry {
    pub fn new() -> Self {
        Self::new_with_tls(None)
    }

    pub fn new_with_tls(tls_config: Option<ClientTlsConfig>) -> Self {
        let mut downloaders: HashMap<String, Box<dyn PackageDownloader>> = HashMap::new();

        downloaders.insert("file".to_string(), Box::new(FileDownloader));
        downloaders.insert(
            "http".to_string(),
            Box::new(HttpDownloader::new(Duration::from_secs(HTTP_TIMEOUT_SECS))),
        );
        downloaders.insert(
            "https".to_string(),
            Box::new(HttpDownloader::new(Duration::from_secs(HTTP_TIMEOUT_SECS))),
        );

        let grpc_downloader = GrpcDownloader::new(Duration::from_secs(GRPC_CONNECT_TIMEOUT_SECS));
        downloaders.insert("grpc".to_string(), Box::new(grpc_downloader));

        let grpcs_downloader = if let Some(tls) = tls_config {
            GrpcDownloader::with_tls(Duration::from_secs(GRPC_CONNECT_TIMEOUT_SECS), tls)
        } else {
            GrpcDownloader::new(Duration::from_secs(GRPC_CONNECT_TIMEOUT_SECS))
        };
        downloaders.insert("grpcs".to_string(), Box::new(grpcs_downloader));

        Self { downloaders }
    }

    pub async fn download(&self, url: &str, dest_path: &Path) -> Result<(), FlameError> {
        let parsed_url = url::Url::parse(url)
            .map_err(|e| FlameError::InvalidConfig(format!("invalid url: {}", e)))?;

        let scheme = parsed_url.scheme();
        let downloader = self.downloaders.get(scheme).ok_or_else(|| {
            FlameError::InvalidConfig(format!(
                "unsupported scheme: {}. Supported: file, http, https, grpc, grpcs",
                scheme
            ))
        })?;

        downloader.download(&parsed_url, dest_path).await
    }
}

impl Default for DownloaderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::io::AsyncWriteExt;
    use tonic::transport::ClientTlsConfig;

    #[test]
    fn test_registry_has_all_schemes() {
        let registry = DownloaderRegistry::new();
        assert!(registry.downloaders.contains_key("file"));
        assert!(registry.downloaders.contains_key("http"));
        assert!(registry.downloaders.contains_key("https"));
        assert!(registry.downloaders.contains_key("grpc"));
        assert!(registry.downloaders.contains_key("grpcs"));
    }

    #[test]
    fn test_registry_default() {
        let registry = DownloaderRegistry::default();
        assert!(registry.downloaders.contains_key("file"));
        assert!(registry.downloaders.contains_key("grpcs"));
    }

    #[test]
    fn test_registry_with_tls_config() {
        let tls_config = ClientTlsConfig::new();
        let registry = DownloaderRegistry::new_with_tls(Some(tls_config));
        assert!(registry.downloaders.contains_key("grpcs"));
    }

    #[test]
    fn test_registry_with_none_tls_config() {
        let registry = DownloaderRegistry::new_with_tls(None);
        assert!(registry.downloaders.contains_key("grpcs"));
    }

    #[tokio::test]
    async fn test_registry_unsupported_scheme() {
        let registry = DownloaderRegistry::new();
        let temp_dir = TempDir::new().unwrap();
        let dest_path = temp_dir.path().join("test.tar.gz");

        let result = registry
            .download("ftp://host/file.tar.gz", &dest_path)
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("unsupported scheme"));
    }

    #[tokio::test]
    async fn test_registry_invalid_url() {
        let registry = DownloaderRegistry::new();
        let temp_dir = TempDir::new().unwrap();
        let dest_path = temp_dir.path().join("test.tar.gz");

        let result = registry.download("not-a-valid-url", &dest_path).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("invalid url"));
    }

    #[tokio::test]
    async fn test_file_downloader() {
        let temp_dir = TempDir::new().unwrap();
        let src_path = temp_dir.path().join("source.tar.gz");
        let dest_path = temp_dir.path().join("dest.tar.gz");

        let mut src_file = tokio::fs::File::create(&src_path).await.unwrap();
        src_file.write_all(b"test content").await.unwrap();
        drop(src_file);

        let registry = DownloaderRegistry::new();
        let url = format!("file://{}", src_path.display());

        registry.download(&url, &dest_path).await.unwrap();

        assert!(dest_path.exists());
        let content = tokio::fs::read(&dest_path).await.unwrap();
        assert_eq!(content, b"test content");
    }

    #[tokio::test]
    async fn test_file_downloader_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let dest_path = temp_dir.path().join("dest.tar.gz");

        let registry = DownloaderRegistry::new();
        let url = format!("file://{}/nonexistent.tar.gz", temp_dir.path().display());

        let result = registry.download(&url, &dest_path).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_file_downloader_invalid_url() {
        let downloader = FileDownloader;
        let temp_dir = TempDir::new().unwrap();
        let dest_path = temp_dir.path().join("dest.tar.gz");

        let url = url::Url::parse("file://").unwrap();
        let result = downloader.download(&url, &dest_path).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_http_downloader_new() {
        let downloader = HttpDownloader::new(Duration::from_secs(30));
        assert_eq!(downloader.timeout, Duration::from_secs(30));
    }

    #[tokio::test]
    async fn test_http_downloader_connection_refused() {
        let downloader = HttpDownloader::new(Duration::from_secs(1));
        let temp_dir = TempDir::new().unwrap();
        let dest_path = temp_dir.path().join("dest.tar.gz");

        let url = url::Url::parse("http://127.0.0.1:59999/nonexistent.tar.gz").unwrap();
        let result = downloader.download(&url, &dest_path).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("failed to download"));
    }

    #[test]
    fn test_grpc_downloader_new() {
        let downloader = GrpcDownloader::new(Duration::from_secs(30));
        assert_eq!(downloader.connect_timeout, Duration::from_secs(30));
        assert!(downloader.tls_config.is_none());
    }

    #[test]
    fn test_grpc_downloader_with_tls() {
        let tls_config = ClientTlsConfig::new();
        let downloader = GrpcDownloader::with_tls(Duration::from_secs(30), tls_config);
        assert_eq!(downloader.connect_timeout, Duration::from_secs(30));
        assert!(downloader.tls_config.is_some());
    }

    #[tokio::test]
    async fn test_grpc_downloader_missing_host() {
        let downloader = GrpcDownloader::new(Duration::from_secs(1));
        let temp_dir = TempDir::new().unwrap();
        let dest_path = temp_dir.path().join("dest.tar.gz");

        let url = url::Url::parse("grpc:///path/to/file").unwrap();
        let result = downloader.download(&url, &dest_path).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing host"));
    }

    #[tokio::test]
    async fn test_grpc_downloader_connection_failed() {
        let downloader = GrpcDownloader::new(Duration::from_secs(1));
        let temp_dir = TempDir::new().unwrap();
        let dest_path = temp_dir.path().join("dest.tar.gz");

        let url = url::Url::parse("grpc://127.0.0.1:59999/app/pkg/file.tar.gz").unwrap();
        let result = downloader.download(&url, &dest_path).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("failed to connect"));
    }

    #[tokio::test]
    async fn test_grpcs_downloader_connection_failed() {
        let downloader = GrpcDownloader::new(Duration::from_secs(1));
        let temp_dir = TempDir::new().unwrap();
        let dest_path = temp_dir.path().join("dest.tar.gz");

        let url = url::Url::parse("grpcs://127.0.0.1:59999/app/pkg/file.tar.gz").unwrap();
        let result = downloader.download(&url, &dest_path).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("failed to connect"));
    }

    #[tokio::test]
    async fn test_grpcs_downloader_with_tls_connection_failed() {
        let tls_config = ClientTlsConfig::new();
        let downloader = GrpcDownloader::with_tls(Duration::from_secs(1), tls_config);
        let temp_dir = TempDir::new().unwrap();
        let dest_path = temp_dir.path().join("dest.tar.gz");

        let url = url::Url::parse("grpcs://127.0.0.1:59999/app/pkg/file.tar.gz").unwrap();
        let result = downloader.download(&url, &dest_path).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("failed to connect"));
    }

    #[tokio::test]
    async fn test_grpc_downloader_default_port() {
        let downloader = GrpcDownloader::new(Duration::from_secs(1));
        let temp_dir = TempDir::new().unwrap();
        let dest_path = temp_dir.path().join("dest.tar.gz");

        let url = url::Url::parse("grpc://127.0.0.1/app/pkg/file.tar.gz").unwrap();
        assert!(url.port().is_none());
        let result = downloader.download(&url, &dest_path).await;
        assert!(result.is_err());
    }
}
