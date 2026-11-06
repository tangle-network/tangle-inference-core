//! Weight staging through its public export and a real HTTP origin.
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tangle_inference_core::{
    engine::{EngineConfig, EngineProcess},
    weights::{stage_weights, StagingLimits, WeightSource},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{Barrier, Notify},
    task::JoinHandle,
};

const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

#[derive(Clone)]
struct Reply {
    wire: Vec<u8>,
    pause: bool,
    barrier: Option<Arc<Barrier>>,
}

struct Origin {
    url: String,
    routes: Arc<Mutex<BTreeMap<String, Reply>>>,
    requests: Arc<Mutex<Vec<String>>>,
    changed: Arc<Notify>,
    task: JoinHandle<()>,
}

impl Origin {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let routes = Arc::new(Mutex::new(BTreeMap::<String, Reply>::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let changed = Arc::new(Notify::new());
        let (responses, observed, notification) =
            (routes.clone(), requests.clone(), changed.clone());
        let task = tokio::spawn(async move {
            let mut workers = tokio::task::JoinSet::new();
            while let Ok((mut stream, _)) = listener.accept().await {
                let (responses, observed, notification) =
                    (responses.clone(), observed.clone(), notification.clone());
                workers.spawn(async move {
                    let mut request = Vec::new();
                    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                        let mut chunk = [0; 1024];
                        let Ok(count) = stream.read(&mut chunk).await else { return };
                        if count == 0 || request.len() + count > 8192 { return }
                        request.extend_from_slice(&chunk[..count]);
                    }
                    let path = std::str::from_utf8(&request).unwrap().split_whitespace().nth(1).unwrap().to_owned();
                    observed.lock().unwrap().push(path.clone());
                    notification.notify_one();
                    let reply = responses.lock().unwrap().get(&path).cloned();
                    let reply = reply.unwrap_or(Reply { wire:b"HTTP/1.1 404 Missing\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),pause:false,barrier:None });
                    if let Some(barrier) = reply.barrier { barrier.wait().await; }
                    let _ = stream.write_all(&reply.wire).await;
                    if reply.pause { std::future::pending::<()>().await; }
                });
            }
        });
        Self {
            url,
            routes,
            requests,
            changed,
            task,
        }
    }

    fn serve(&self, path: &str, bytes: Vec<u8>) {
        let mut wire = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            bytes.len()
        )
        .into_bytes();
        wire.extend(bytes);
        self.reply(path, wire, false, None);
    }

    fn reply(&self, path: &str, wire: Vec<u8>, pause: bool, barrier: Option<Arc<Barrier>>) {
        self.routes.lock().unwrap().insert(
            path.into(),
            Reply {
                wire,
                pause,
                barrier,
            },
        );
    }

    fn file(&self, path: &str) -> serde_json::Value {
        serde_json::json!({"path":path,"url":format!("{}/weights",self.url),"sizeBytes":5,"sha256":HELLO_SHA256})
    }

    fn manifest(&self, files: serde_json::Value, size_bytes: u64) -> WeightSource {
        let raw = serde_json::to_vec(&serde_json::json!({"version":1,"files":files})).unwrap();
        let source = WeightSource {
            manifest_sha: format!("{:x}", Sha256::digest(&raw)),
            size_bytes,
        };
        self.serve("/manifest", raw);
        source
    }

    fn take_requests(&self) -> Vec<String> {
        std::mem::take(&mut *self.requests.lock().unwrap())
    }

    async fn wait_for_request(&self, path: &str) {
        loop {
            let changed = self.changed.notified();
            if self
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request == path)
            {
                return;
            }
            changed.await;
        }
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn fixture_limits() -> StagingLimits {
    StagingLimits {
        allow_loopback_http: true,
        ..Default::default()
    }
}
fn assert_empty(path: &std::path::Path) {
    assert!(
        std::fs::read_dir(path).unwrap().next().is_none(),
        "stage left published or partial files"
    );
}

async fn reject(
    url: &str,
    source: &WeightSource,
    destination: &std::path::Path,
    limits: &StagingLimits,
) {
    let result = stage_weights(url, source, destination, limits).await;
    let _ = result.expect_err("staging unexpectedly succeeded");
}

#[tokio::test]
async fn whole_manifest_is_checked_before_any_shard_fetch() {
    let origin = Origin::start().await;
    let limits = fixture_limits();
    let destination = tempfile::tempdir().unwrap();
    let mut cases: Vec<_> = [
        "../escape.bin",
        "a/../escape.bin",
        "/absolute.bin",
        r"C:\outside.bin",
        ".tangle-weights-manifest.json",
    ]
    .into_iter()
    .map(|path| (serde_json::json!([origin.file(path)]), 5))
    .collect();
    cases.extend([
        (
            serde_json::json!([origin.file("same"), origin.file("same")]),
            10,
        ),
        (
            serde_json::json!([origin.file("parent"), origin.file("parent/child")]),
            10,
        ),
        (serde_json::json!([origin.file("weights.bin")]), 6),
    ]);
    for (files, size) in cases {
        let source = origin.manifest(files, size);
        reject(
            &format!("{}/manifest", origin.url),
            &source,
            destination.path(),
            &limits,
        )
        .await;
        assert_eq!(origin.take_requests(), ["/manifest"]);
        assert_empty(destination.path());
    }
    let source = origin.manifest(serde_json::json!([origin.file("weights.bin")]), 5);
    let mut manifest =
        serde_json::to_vec(&serde_json::json!({"version":1,"files":[origin.file("weights.bin")]}))
            .unwrap();
    manifest.push(b' ');
    origin.serve("/manifest", manifest);
    reject(
        &format!("{}/manifest", origin.url),
        &source,
        destination.path(),
        &limits,
    )
    .await;
    assert_eq!(origin.take_requests(), ["/manifest"]);
    assert_empty(destination.path());
}

#[tokio::test]
async fn origin_policy_and_byte_limits_refuse_downloads_before_publication() {
    let origin = Origin::start().await;
    let source = origin.manifest(serde_json::json!([origin.file("weights.bin")]), 5);
    origin.serve("/weights", b"hello".to_vec());
    let destination = tempfile::tempdir().unwrap();
    let url = format!("{}/manifest", origin.url);
    for limits in [
        StagingLimits::default(),
        StagingLimits {
            max_total_bytes: 4,
            ..fixture_limits()
        },
    ] {
        reject(&url, &source, destination.path(), &limits).await;
        assert!(origin.take_requests().is_empty());
        assert_empty(destination.path());
    }
    let limits = StagingLimits {
        max_manifest_bytes: 8,
        ..fixture_limits()
    };
    reject(&url, &source, destination.path(), &limits).await;
    assert_eq!(origin.take_requests(), ["/manifest"]);
    let source = origin.manifest(
        serde_json::json!([origin.file("one"), origin.file("two")]),
        10,
    );
    let limits = StagingLimits {
        max_files: 1,
        ..fixture_limits()
    };
    reject(&url, &source, destination.path(), &limits).await;
    assert_eq!(origin.take_requests(), ["/manifest"]);
    assert_empty(destination.path());
}

#[tokio::test]
async fn invalid_shards_prevent_staging_and_engine_child_start() {
    let origin = Origin::start().await;
    let source = origin.manifest(serde_json::json!([origin.file("weights.bin")]), 5);
    let destination = tempfile::tempdir().unwrap();
    let child = tempfile::tempdir().unwrap();
    let script = child.path().join("engine.py");
    let marker = child.path().join("spawned");
    std::fs::write(
        &script,
        "import pathlib,sys\npathlib.Path(sys.argv[1]).write_text('spawned')\n",
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let config: EngineConfig = serde_json::from_value(serde_json::json!({
        "model":"acme/derived", "command":format!("python3 {} {}",script.display(),marker.display()),
        "port":port, "startup_timeout_secs":1,
        "weights":{"manifest_url":format!("{}/manifest",origin.url),"manifestSha":source.manifest_sha,
            "sizeBytes":source.size_bytes,"cache_dir":destination.path(),"allow_loopback_http":true}
    })).unwrap();
    for wire in [
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nHELLO".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhe".to_vec(),
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n1\r\n!\r\n0\r\n\r\n".to_vec(),
    ] {
        origin.reply("/weights",wire,false,None);
        assert!(stage_weights(&format!("{}/manifest",origin.url),&source,destination.path(),&fixture_limits()).await.is_err());
        assert_eq!(origin.take_requests(),["/manifest","/weights"]);
        assert_empty(destination.path());
        let refused = match EngineProcess::spawn(config.clone()).await {
            Err(_) => true,
            Ok(process) => {
                let refused = process.wait_ready().await.is_err();
                process.shutdown().await;
                refused
            }
        };
        assert!(!marker.exists(), "invalid shard reached the engine child process");
        assert!(refused, "invalid shard did not prevent engine startup");
        assert_eq!(origin.take_requests(),["/manifest","/weights"]);
        assert_empty(destination.path());
    }
}

#[tokio::test]
async fn cache_changes_are_refused_offline_without_repairing_or_deleting_data() {
    let origin = Origin::start().await;
    let source = origin.manifest(serde_json::json!([origin.file("nested/weights.bin")]), 5);
    origin.serve("/weights", b"hello".to_vec());
    let destination = tempfile::tempdir().unwrap();
    let url = format!("{}/manifest", origin.url);
    let verified = stage_weights(&url, &source, destination.path(), &fixture_limits())
        .await
        .unwrap();
    drop(origin);
    let cached = stage_weights(&url, &source, destination.path(), &fixture_limits())
        .await
        .unwrap();
    assert_eq!(cached.model_dir(), verified.model_dir());
    let file = verified.model_dir().join("nested/weights.bin");
    std::fs::write(&file, b"HELLO").unwrap();
    reject(&url, &source, destination.path(), &fixture_limits()).await;
    assert_eq!(std::fs::read(&file).unwrap(), b"HELLO");
    std::fs::write(&file, b"hello").unwrap();
    std::fs::write(verified.model_dir().join("unlisted.bin"), b"unexpected").unwrap();
    reject(&url, &source, destination.path(), &fixture_limits()).await;
    std::fs::remove_file(verified.model_dir().join("unlisted.bin")).unwrap();
    let manifest = verified.model_dir().join(".tangle-weights-manifest.json");
    let mut changed = std::fs::read(&manifest).unwrap();
    changed.push(b' ');
    std::fs::write(&manifest, changed).unwrap();
    reject(&url, &source, destination.path(), &fixture_limits()).await;
}

#[cfg(unix)]
#[tokio::test]
async fn cache_and_destination_symlinks_are_never_followed() {
    let origin = Origin::start().await;
    let source = origin.manifest(serde_json::json!([origin.file("weights.bin")]), 5);
    origin.serve("/weights", b"hello".to_vec());
    let destination = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("weights.bin");
    std::fs::write(&outside_file, b"hello").unwrap();
    let url = format!("{}/manifest", origin.url);
    let verified = stage_weights(&url, &source, destination.path(), &fixture_limits())
        .await
        .unwrap();
    origin.take_requests();
    let cached = verified.model_dir().join("weights.bin");
    std::fs::remove_file(&cached).unwrap();
    std::os::unix::fs::symlink(&outside_file, &cached).unwrap();
    reject(&url, &source, destination.path(), &fixture_limits()).await;
    let alias = destination.path().join("alias");
    std::os::unix::fs::symlink(outside.path(), &alias).unwrap();
    reject(&url, &source, &alias, &fixture_limits()).await;
    assert!(origin.take_requests().is_empty());
    assert_eq!(std::fs::read(outside_file).unwrap(), b"hello");
}

#[tokio::test]
async fn cancellation_cleans_only_its_private_partial_directory() {
    let origin = Origin::start().await;
    let source = origin.manifest(serde_json::json!([origin.file("weights.bin")]), 5);
    origin.reply(
        "/weights",
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhe".to_vec(),
        true,
        None,
    );
    let destination = tempfile::tempdir().unwrap();
    let keep = destination.path().join("keep");
    std::fs::create_dir(&keep).unwrap();
    std::fs::write(keep.join("data"), b"owned elsewhere").unwrap();
    let target = destination.path().to_owned();
    let url = format!("{}/manifest", origin.url);
    let staging =
        tokio::spawn(async move { stage_weights(&url, &source, &target, &fixture_limits()).await });
    tokio::time::timeout(Duration::from_secs(5), origin.wait_for_request("/weights"))
        .await
        .unwrap();
    staging.abort();
    assert!(staging.await.unwrap_err().is_cancelled());
    assert_eq!(
        std::fs::read(keep.join("data")).unwrap(),
        b"owned elsewhere"
    );
    assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_publishers_reuse_the_verified_winner() {
    let origin = Origin::start().await;
    let source = origin.manifest(serde_json::json!([origin.file("weights.bin")]), 5);
    origin.reply(
        "/weights",
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello".to_vec(),
        false,
        Some(Arc::new(Barrier::new(2))),
    );
    let destination = tempfile::tempdir().unwrap();
    let url = format!("{}/manifest", origin.url);
    let limits = fixture_limits();
    let (first, second) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            stage_weights(&url, &source, destination.path(), &limits),
            stage_weights(&url, &source, destination.path(), &limits)
        )
    })
    .await
    .unwrap();
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_eq!(first.model_dir(), second.model_dir());
    assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn empty_cache_created_during_download_is_never_replaced() {
    let origin = Origin::start().await;
    let source = origin.manifest(serde_json::json!([origin.file("weights.bin")]), 5);
    let barrier = Arc::new(Barrier::new(2));
    origin.reply(
        "/weights",
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello".to_vec(),
        false,
        Some(barrier.clone()),
    );
    let destination = tempfile::tempdir().unwrap();
    let published = destination.path().join(&source.manifest_sha);
    let target = destination.path().to_owned();
    let url = format!("{}/manifest", origin.url);
    let staging =
        tokio::spawn(async move { stage_weights(&url, &source, &target, &fixture_limits()).await });
    tokio::time::timeout(Duration::from_secs(5), origin.wait_for_request("/weights"))
        .await
        .unwrap();
    std::fs::create_dir(&published).unwrap();
    barrier.wait().await;
    assert!(staging.await.unwrap().is_err(), "replaced existing cache");
    assert_empty(&published);
    assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 1);
}
