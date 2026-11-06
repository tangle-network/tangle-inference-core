use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

fn git(root: &Path, args: &[&str]) -> Option<Output> {
    Command::new("git")
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["-c", "core.fsmonitor=false", "-C"])
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()
}

fn git_text(root: &Path, args: &[&str]) -> Option<String> {
    let output = git(root, args)?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())?
        .map(|s| s.trim().to_owned())
}

fn watch(path: &Path) {
    if let Some(path) = path.to_str().filter(|path| !path.contains(['\n', '\r'])) {
        println!("cargo:rerun-if-changed={path}");
    }
}

fn git_path(root: &Path, name: &str) -> Option<PathBuf> {
    Some(root.join(git_text(root, &["rev-parse", "--git-path", name])?))
}

fn revision(value: &str) -> Option<String> {
    matches!(value.len(), 40 | 64)
        .then_some(value)
        .filter(|sha| sha.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
}

fn source(root: &Path) -> Option<(String, Option<bool>, &'static str)> {
    let top = git_text(root, &["rev-parse", "--show-toplevel"])
        .and_then(|path| fs::canonicalize(path).ok());
    if top.as_deref() == Some(root) {
        for name in ["HEAD", "index", "packed-refs"] {
            if let Some(path) = git_path(root, name).filter(|path| path.exists()) {
                watch(&path);
            }
        }
        if let Some(reference) = git_text(root, &["symbolic-ref", "-q", "HEAD"]) {
            let path = git_path(root, &reference)?;
            if let Some(existing) = path.ancestors().find(|path| path.exists()) {
                watch(existing);
            }
        }
        if let Some(files) = git(root, &["ls-files", "-z"]).filter(|output| output.status.success())
        {
            for file in std::str::from_utf8(&files.stdout)
                .ok()?
                .split('\0')
                .filter(|file| !file.is_empty())
            {
                watch(&root.join(file));
            }
        }
        let sha = revision(&git_text(
            root,
            &["rev-parse", "--verify", "HEAD^{commit}"],
        )?)?;
        let dirty = git(
            root,
            &[
                "diff",
                "--quiet",
                "--no-ext-diff",
                "--no-textconv",
                "HEAD",
                "--",
            ],
        )?;
        return Some((
            sha,
            match dirty.status.code() {
                Some(0) => Some(false),
                Some(1) => Some(true),
                _ => None,
            },
            "git_checkout",
        ));
    }
    None
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let source = fs::canonicalize(env!("CARGO_MANIFEST_DIR"))
        .ok()
        .and_then(|root| source(&root));
    let (revision, dirty, origin) = source
        .map(|(sha, dirty, origin)| (Some(sha), dirty, origin))
        .unwrap_or((None, None, "unknown"));
    let output = format!("const SOURCE_REVISION: Option<&str> = {revision:?};\nconst SOURCE_DIRTY: Option<bool> = {dirty:?};\nconst SOURCE_ORIGIN: &str = {origin:?};\n");
    fs::write(
        PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo output directory"))
            .join("core_source.rs"),
        output,
    )
    .expect("write core source metadata");
}
