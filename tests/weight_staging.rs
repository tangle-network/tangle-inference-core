//! Weight staging through its public export and a real HTTP origin.
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tangle_inference_core::weights::{stage_weights, StagingLimits, WeightSource};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

struct Origin {
    url: String,
    routes: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    task: JoinHandle<()>,
}

impl Origin {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let routes = Arc::new(Mutex::new(BTreeMap::<String, Vec<u8>>::new()));
        let responses = routes.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let responses = responses.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                        let mut chunk = [0; 1024];
                        let Ok(count) = stream.read(&mut chunk).await else {
                            return;
                        };
                        if count == 0 || request.len() + count > 8192 {
                            return;
                        }
                        request.extend_from_slice(&chunk[..count]);
                    }
                    let path = std::str::from_utf8(&request)
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap();
                    let body = responses.lock().unwrap().get(path).cloned();
                    let (status, body) = body.map_or((404, Vec::new()), |body| (200, body));
                    let headers = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    if stream.write_all(headers.as_bytes()).await.is_ok() {
                        let _ = stream.write_all(&body).await;
                    }
                });
            }
        });
        Self { url, routes, task }
    }

    fn serve(&self, path: &str, bytes: Vec<u8>) {
        self.routes.lock().unwrap().insert(path.into(), bytes);
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn tampered_shard_never_publishes_and_verified_cache_works_offline() {
    let origin = Origin::start().await;
    let manifest = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "files": [{"path":"weights.bin","url":format!("{}/weights",origin.url),
            "sizeBytes":5,"sha256":HELLO_SHA256}]
    }))
    .unwrap();
    let source = WeightSource {
        manifest_sha: format!("{:x}", Sha256::digest(&manifest)),
        size_bytes: 5,
    };
    origin.serve("/manifest", manifest);
    origin.serve("/weights", b"HELLO".to_vec());
    let destination = tempfile::tempdir().unwrap();
    let limits = StagingLimits {
        allow_loopback_http: true,
        ..Default::default()
    };
    let manifest_url = format!("{}/manifest", origin.url);
    let tampered = stage_weights(&manifest_url, &source, destination.path(), &limits).await;
    assert!(tampered.is_err(), "same-sized tampered shard was accepted");
    assert!(
        std::fs::read_dir(destination.path())
            .unwrap()
            .next()
            .is_none(),
        "failed stage left published or partial files"
    );
    origin.serve("/weights", b"hello".to_vec());
    let verified = stage_weights(&manifest_url, &source, destination.path(), &limits)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(verified.model_dir().join("weights.bin")).unwrap(),
        b"hello"
    );
    drop(origin);
    let cached = stage_weights(&manifest_url, &source, destination.path(), &limits)
        .await
        .unwrap();
    assert_eq!(cached.model_dir(), verified.model_dir());
}
