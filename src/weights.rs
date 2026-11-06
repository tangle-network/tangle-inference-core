//! Manifest-pinned model files, published together after every byte is verified.
//! The destination is an operator-owned cache. Origins require HTTPS unless the
//! caller explicitly enables trusted loopback HTTP fixtures. URLs are not logged.

use anyhow::{bail, ensure, Context, Result};
use reqwest::{redirect::Policy, Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MANIFEST_NAME: &str = ".tangle-weights-manifest.json";
const MANIFEST_VERSION: u32 = 1;
static PARTIAL_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WeightSource {
    pub manifest_sha: String,
    /// Sum of the listed file sizes; the manifest itself is not a model file.
    pub size_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct StagingLimits {
    pub max_total_bytes: u64,
    pub max_manifest_bytes: usize,
    pub max_files: usize,
    pub timeout: Duration,
    /// Explicitly opt in only for an operator-controlled local fixture origin.
    pub allow_loopback_http: bool,
}

impl Default for StagingLimits {
    fn default() -> Self {
        Self {
            max_total_bytes: 1 << 40,
            max_manifest_bytes: 4 << 20,
            max_files: 4096,
            timeout: Duration::from_secs(2 * 60 * 60),
            allow_loopback_http: false,
        }
    }
}

#[derive(Debug)]
pub struct VerifiedWeights {
    directory: PathBuf,
}

impl VerifiedWeights {
    pub fn model_dir(&self) -> &Path {
        &self.directory
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    files: Vec<Shard>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Shard {
    path: String,
    url: String,
    size_bytes: u64,
    sha256: String,
}

/// Verify the pinned manifest and every model file, then return its local directory.
/// A cache hit is verified completely without consulting the origin. Invalid cache
/// entries are refused rather than removed. Cancellation removes only this call's
/// private partial directory. No model-family or engine resolution happens here.
pub async fn stage_weights(
    manifest_url: &str,
    source: &WeightSource,
    destination: &Path,
    limits: &StagingLimits,
) -> Result<VerifiedWeights> {
    validate_sha(&source.manifest_sha)?;
    ensure!(
        source.size_bytes > 0 && source.size_bytes <= limits.max_total_bytes,
        "model size exceeds staging policy or is empty"
    );
    ensure!(
        limits.max_files > 0 && limits.max_manifest_bytes > 0 && !limits.timeout.is_zero(),
        "invalid staging limits"
    );
    let destination = prepare_directory(destination)?;
    let digest = source.manifest_sha.to_ascii_lowercase();
    tokio::time::timeout(limits.timeout, async {
        let published = destination.join(&digest);
        if fs::symlink_metadata(&published).is_ok() {
            verify_cache(&published, source, limits).await?;
            return Ok(VerifiedWeights {
                directory: published,
            });
        }
        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .timeout(limits.timeout)
            .build()?;
        let raw = fetch_manifest(&client, manifest_url, limits).await?;
        let manifest = parse_manifest(&raw, source, limits)?;
        let mut partial = PartialDirectory::create(&destination, &digest)?;
        for shard in &manifest.files {
            download_shard(&client, shard, partial.path(), limits).await?;
        }
        let manifest_file = create_file(&partial.path().join(MANIFEST_NAME))?;
        let mut manifest_file = tokio::fs::File::from_std(manifest_file);
        manifest_file.write_all(&raw).await?;
        manifest_file.sync_all().await?;
        drop(manifest_file);
        fs::File::open(partial.path())?.sync_all()?;
        match fs::rename(partial.path(), &published) {
            Ok(()) => partial.0 = None,
            Err(error) => {
                if fs::symlink_metadata(&published).is_err() {
                    return Err(error).context("could not publish verified weights");
                }
                verify_cache(&published, source, limits).await?;
            }
        }
        fs::File::open(&destination)?.sync_all()?;
        Ok(VerifiedWeights {
            directory: published,
        })
    })
    .await
    .context("weight staging timed out")?
}

fn validate_sha(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "expected a SHA-256 hexadecimal digest"
    );
    Ok(())
}

fn parse_manifest(raw: &[u8], source: &WeightSource, limits: &StagingLimits) -> Result<Manifest> {
    ensure!(
        source
            .manifest_sha
            .eq_ignore_ascii_case(&format!("{:x}", Sha256::digest(raw))),
        "manifest digest mismatch"
    );
    let manifest: Manifest = serde_json::from_slice(raw).context("invalid weights manifest")?;
    ensure!(
        manifest.version == MANIFEST_VERSION,
        "unsupported weights manifest version"
    );
    ensure!(
        !manifest.files.is_empty() && manifest.files.len() <= limits.max_files,
        "invalid manifest file count"
    );
    let mut paths = BTreeSet::new();
    let mut total = 0_u64;
    for shard in &manifest.files {
        let parts: Vec<_> = shard.path.split('/').collect();
        ensure!(
            shard.path.len() <= 4096
                && parts.len() <= 32
                && !shard.path.chars().any(char::is_control)
                && parts.iter().all(|part| !part.is_empty()
                    && *part != "."
                    && *part != ".."
                    && part.len() <= 255
                    && !part.contains(['\\', ':']))
                && parts[0] != MANIFEST_NAME,
            "unsafe or reserved manifest path"
        );
        ensure!(
            paths.insert(PathBuf::from(&shard.path)),
            "duplicate manifest path"
        );
        validate_sha(&shard.sha256)?;
        checked_url(&shard.url, limits)?;
        total = total
            .checked_add(shard.size_bytes)
            .context("manifest size overflow")?;
        ensure!(
            total <= source.size_bytes,
            "manifest total exceeds pinned model size"
        );
    }
    ensure!(
        total == source.size_bytes,
        "manifest total differs from pinned model size"
    );
    for path in &paths {
        let mut parent = path.parent();
        while let Some(directory) = parent {
            ensure!(
                !paths.contains(directory),
                "file and directory paths overlap"
            );
            parent = directory.parent();
        }
    }
    Ok(manifest)
}

fn checked_url(value: &str, limits: &StagingLimits) -> Result<Url> {
    let url = Url::parse(value).context("invalid weights origin URL")?;
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "URL credentials are not supported"
    );
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    ensure!(
        url.scheme() == "https"
            || (limits.allow_loopback_http && url.scheme() == "http" && loopback),
        "weights origins require HTTPS; loopback fixtures require explicit policy"
    );
    ensure!(url.fragment().is_none(), "URL fragments are not supported");
    Ok(url)
}

async fn response(client: &Client, url: &str, limits: &StagingLimits) -> Result<reqwest::Response> {
    let response = client
        .get(checked_url(url, limits)?)
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    ensure!(
        response.status() == StatusCode::OK,
        "weights origin returned HTTP {}",
        response.status()
    );
    Ok(response)
}

async fn fetch_manifest(client: &Client, url: &str, limits: &StagingLimits) -> Result<Vec<u8>> {
    let mut response = response(client, url, limits).await?;
    ensure!(
        response
            .content_length()
            .is_none_or(|size| size <= limits.max_manifest_bytes as u64),
        "manifest exceeds size limit"
    );
    let mut raw = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(reqwest::Error::without_url)?
    {
        ensure!(
            chunk.len() <= limits.max_manifest_bytes.saturating_sub(raw.len()),
            "manifest exceeds size limit"
        );
        raw.extend_from_slice(&chunk);
    }
    Ok(raw)
}

async fn download_shard(
    client: &Client,
    shard: &Shard,
    directory: &Path,
    limits: &StagingLimits,
) -> Result<()> {
    let mut response = response(client, &shard.url, limits).await?;
    ensure!(
        response
            .content_length()
            .is_none_or(|size| size == shard.size_bytes),
        "shard length header mismatch"
    );
    let path = directory.join(&shard.path);
    private_directory(path.parent().context("shard parent missing")?, true)?;
    let mut file = tokio::fs::File::from_std(create_file(&path)?);
    let mut digest = Sha256::new();
    let mut size = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(reqwest::Error::without_url)?
    {
        size = size
            .checked_add(chunk.len() as u64)
            .context("shard size overflow")?;
        ensure!(size <= shard.size_bytes, "shard exceeds pinned size");
        digest.update(&chunk);
        file.write_all(&chunk).await?;
    }
    ensure!(size == shard.size_bytes, "shard size mismatch");
    ensure!(
        shard
            .sha256
            .eq_ignore_ascii_case(&format!("{:x}", digest.finalize())),
        "shard digest mismatch"
    );
    file.sync_all().await?;
    Ok(())
}

async fn verify_cache(
    directory: &Path,
    source: &WeightSource,
    limits: &StagingLimits,
) -> Result<()> {
    ensure!(
        fs::symlink_metadata(directory)?.file_type().is_dir(),
        "cache directory is not a regular directory"
    );
    let raw_file = open_regular(&directory.join(MANIFEST_NAME))?;
    ensure!(
        raw_file.metadata()?.len() <= limits.max_manifest_bytes as u64,
        "cached manifest exceeds size limit"
    );
    let mut raw = Vec::new();
    tokio::fs::File::from_std(raw_file)
        .take(
            (limits.max_manifest_bytes as u64)
                .checked_add(1)
                .context("manifest limit overflow")?,
        )
        .read_to_end(&mut raw)
        .await?;
    ensure!(
        raw.len() <= limits.max_manifest_bytes,
        "cached manifest exceeds size limit"
    );
    let manifest = parse_manifest(&raw, source, limits)?;
    let mut entries = BTreeMap::from([(PathBuf::from(MANIFEST_NAME), false)]);
    for shard in &manifest.files {
        let path = PathBuf::from(&shard.path);
        entries.insert(path.clone(), false);
        for parent in path
            .ancestors()
            .skip(1)
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            entries.insert(parent.to_owned(), true);
        }
    }
    let mut directories = vec![PathBuf::new()];
    while let Some(relative) = directories.pop() {
        for entry in fs::read_dir(directory.join(&relative))? {
            let entry = entry?;
            let path = relative.join(entry.file_name());
            let is_directory = entries.remove(&path).context("unlisted cache entry")?;
            let kind = fs::symlink_metadata(entry.path())?.file_type();
            ensure!(
                if is_directory {
                    kind.is_dir()
                } else {
                    kind.is_file()
                },
                "non-regular cache entry"
            );
            if is_directory {
                directories.push(path);
            }
        }
    }
    ensure!(entries.is_empty(), "cached model file is missing");
    let mut buffer = [0; 64 * 1024];
    for shard in &manifest.files {
        let file = open_regular(&directory.join(&shard.path))?;
        ensure!(
            file.metadata()?.len() == shard.size_bytes,
            "cached shard size mismatch"
        );
        let mut file = tokio::fs::File::from_std(file);
        let mut digest = Sha256::new();
        let mut size = 0_u64;
        loop {
            let count = file.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            size = size
                .checked_add(count as u64)
                .context("cached shard size overflow")?;
            ensure!(size <= shard.size_bytes, "cached shard exceeds pinned size");
            digest.update(&buffer[..count]);
        }
        ensure!(
            size == shard.size_bytes
                && shard
                    .sha256
                    .eq_ignore_ascii_case(&format!("{:x}", digest.finalize())),
            "cached shard digest mismatch"
        );
    }
    Ok(())
}

fn prepare_directory(destination: &Path) -> Result<PathBuf> {
    let absolute = if destination.is_absolute() {
        destination.to_owned()
    } else {
        std::env::current_dir()?.join(destination)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                ensure!(normalized.pop(), "invalid cache destination");
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    let mut prefix = PathBuf::new();
    for component in normalized.components() {
        prefix.push(component.as_os_str());
        match fs::symlink_metadata(&prefix) {
            Ok(metadata) => ensure!(
                metadata.file_type().is_dir(),
                "cache path contains a symlink or non-directory"
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if let Err(error) = private_directory(&prefix, false) {
                    ensure!(
                        fs::symlink_metadata(&prefix)
                            .is_ok_and(|metadata| metadata.file_type().is_dir()),
                        "cannot create cache directory: {error}"
                    );
                }
            }
            Err(error) => return Err(error).context("cannot inspect cache directory"),
        }
    }
    Ok(normalized)
}

fn private_directory(path: &Path, recursive: bool) -> std::io::Result<()> {
    let mut options = fs::DirBuilder::new();
    options.recursive(recursive);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        options.mode(0o700);
    }
    options.create(path)
}

fn create_file(path: &Path) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    Ok(options.open(path)?)
}

fn open_regular(path: &Path) -> Result<fs::File> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "model cache entry is not a regular file"
    );
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "model cache entry is not a regular file"
    );
    Ok(file)
}

struct PartialDirectory(Option<PathBuf>);

impl PartialDirectory {
    fn create(destination: &Path, digest: &str) -> Result<Self> {
        for _ in 0..64 {
            let path = destination.join(format!(
                ".{digest}.partial-{}-{}",
                std::process::id(),
                PARTIAL_ID.fetch_add(1, Ordering::Relaxed)
            ));
            match private_directory(&path, false) {
                Ok(()) => return Ok(Self(Some(path))),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).context("could not create private staging directory")
                }
            }
        }
        bail!("private staging directory collision limit exceeded")
    }

    fn path(&self) -> &Path {
        self.0
            .as_deref()
            .expect("partial directory exists before publication")
    }
}

impl Drop for PartialDirectory {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_dir_all(path);
        }
    }
}
