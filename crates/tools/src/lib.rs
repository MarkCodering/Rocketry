//! Built-in tools and explicit execution boundaries.
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use rocketry_core::*;
use rocketry_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};
use tokio::{io::AsyncReadExt, process::Command};
pub mod mcp;
const MAX_BYTES: usize = 2 * 1024 * 1024;
pub struct HostBackend;
pub struct DockerBackend {
    pub image: String,
}
pub struct DisabledBackend;
#[async_trait]
impl ExecutionBackend for DisabledBackend {
    fn name(&self) -> &'static str {
        "disabled"
    }
    async fn execute(&self, _: &str, _: Value, _: ToolContext) -> Result<Value> {
        bail!("workspace execution disabled; select host explicitly or configure Docker")
    }
}
fn contained(root: &Path, path: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        !Path::new(path).is_absolute(),
        "path must be relative to workspace"
    );
    let root = root.canonicalize()?;
    let target = root.join(path);
    let mut existing = target.as_path();
    while !existing.exists() {
        existing = existing.parent().context("invalid path")?;
    }
    anyhow::ensure!(
        existing.canonicalize()?.starts_with(&root),
        "path escapes workspace"
    );
    anyhow::ensure!(
        !Path::new(path)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir)),
        "parent traversal is not allowed"
    );
    if target.exists() {
        anyhow::ensure!(
            target.canonicalize()?.starts_with(root),
            "symlink escapes workspace"
        );
    }
    Ok(target)
}
#[async_trait]
impl ExecutionBackend for HostBackend {
    fn name(&self) -> &'static str {
        "host"
    }
    async fn execute(&self, op: &str, args: Value, ctx: ToolContext) -> Result<Value> {
        if op == "execute" {
            let command = args["command"].as_str().context("command required")?;
            let mut c = Command::new("/bin/sh");
            c.arg("-c")
                .arg(command)
                .current_dir(&ctx.workspace)
                .env_clear()
                .env("PATH", "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin")
                .env("HOME", &ctx.workspace);
            return process(c, ctx.cancel).await;
        }
        let op = op.to_owned();
        tokio::task::spawn_blocking(move || file_operation(&op, args, ctx)).await?
    }
}
fn file_operation(op: &str, args: Value, ctx: ToolContext) -> Result<Value> {
    use std::io::Read;
    let path = contained(&ctx.workspace, args["path"].as_str().unwrap_or("."))?;
    match op {
        "read_file" => {
            let file = std::fs::File::open(&path)?;
            let truncated = file.metadata()?.len() > MAX_BYTES as u64;
            let mut bytes = Vec::new();
            file.take(MAX_BYTES as u64).read_to_end(&mut bytes)?;
            Ok(
                json!({"text":String::from_utf8_lossy(&bytes),"truncated":truncated,"limit_bytes":MAX_BYTES}),
            )
        }
        "write_file" => {
            let text = args["content"].as_str().context("content required")?;
            anyhow::ensure!(text.len() <= MAX_BYTES, "content exceeds 2 MiB");
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, text)?;
            Ok(json!({"written":text.len(),"path":args["path"]}))
        }
        "patch_file" => {
            anyhow::ensure!(
                path.metadata()?.len() <= MAX_BYTES as u64,
                "file exceeds 2 MiB"
            );
            let original = std::fs::read_to_string(&path)?;
            let old = args["old"].as_str().context("old required")?;
            let new = args["new"].as_str().context("new required")?;
            anyhow::ensure!(
                !old.is_empty() && original.matches(old).count() == 1,
                "patch requires exactly one matching nonempty block"
            );
            let result = original.replacen(old, new, 1);
            anyhow::ensure!(result.len() <= MAX_BYTES, "patched file exceeds 2 MiB");
            std::fs::write(path, result)?;
            Ok(json!({"patched":true,"path":args["path"]}))
        }
        "list_dir" => {
            let mut out = vec![];
            for entry in std::fs::read_dir(path)?.take(1000) {
                let entry = entry?;
                out.push(json!({"name":entry.file_name().to_string_lossy(),"directory":entry.file_type()?.is_dir()}));
            }
            out.sort_by_key(|v| v["name"].as_str().unwrap_or("").to_owned());
            Ok(json!(out))
        }
        "search" => {
            let query = args["query"].as_str().context("query required")?;
            let mut stack = vec![path];
            let mut out = vec![];
            let mut visited = 0;
            while let Some(path) = stack.pop() {
                anyhow::ensure!(!ctx.cancel.is_cancelled(), "cancelled");
                visited += 1;
                if visited > 10000 || out.len() >= 200 {
                    break;
                }
                if path.is_symlink() {
                    continue;
                }
                if path.is_dir() {
                    for entry in std::fs::read_dir(path)?.flatten() {
                        if visited + stack.len() >= 10000 {
                            break;
                        }
                        if ![".git", "node_modules", "target"]
                            .contains(&entry.file_name().to_string_lossy().as_ref())
                        {
                            stack.push(entry.path());
                        }
                    }
                } else if path.metadata()?.len() <= MAX_BYTES as u64
                    && let Ok(text) = std::fs::read_to_string(&path)
                {
                    for (i, line) in text.lines().enumerate() {
                        if line.contains(query) {
                            out.push(json!({"path":path.strip_prefix(&ctx.workspace).unwrap_or(&path),"line":i+1,"text":line.chars().take(500).collect::<String>()}));
                            if out.len() >= 200 {
                                break;
                            }
                        }
                    }
                }
            }
            Ok(json!({"matches":out,"visited":visited}))
        }
        _ => bail!("unknown operation"),
    }
}
#[cfg(unix)]
struct ProcessGroup(u32);
#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}
async fn read_output(mut stream: impl tokio::io::AsyncRead + Unpin) -> Result<(Vec<u8>, bool)> {
    let mut result = Vec::new();
    let mut buf = [0; 8192];
    let mut truncated = false;
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let take = n.min(MAX_BYTES.saturating_sub(result.len()));
        result.extend_from_slice(&buf[..take]);
        truncated |= take < n;
    }
    Ok((result, truncated))
}
pub async fn process(mut c: Command, cancel: tokio_util::sync::CancellationToken) -> Result<Value> {
    c.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    c.process_group(0);
    let mut child = c.spawn()?;
    #[cfg(unix)]
    let _group = ProcessGroup(child.id().context("missing process id")?);
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let work = async {
        let (status, out, err) = tokio::try_join!(
            async { Ok::<_, anyhow::Error>(child.wait().await?) },
            read_output(stdout),
            read_output(stderr)
        )?;
        Ok(
            json!({"exit_code":status.code(),"stdout":String::from_utf8_lossy(&out.0),"stderr":String::from_utf8_lossy(&err.0),"truncated":out.1||err.1}),
        )
    };
    tokio::select! {_=cancel.cancelled()=>{bail!("process cancelled")},r=tokio::time::timeout(std::time::Duration::from_secs(120),work)=>r.context("process exceeded 120 second limit")?}
}
struct ContainerCleanup(String);
impl Drop for ContainerCleanup {
    fn drop(&mut self) {
        let name = self.0.clone();
        tokio::spawn(async move {
            let _ = Command::new("docker")
                .args(["rm", "-f", &name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        });
    }
}
#[async_trait]
impl ExecutionBackend for DockerBackend {
    fn name(&self) -> &'static str {
        "docker"
    }
    async fn execute(&self, op: &str, args: Value, ctx: ToolContext) -> Result<Value> {
        let workspace = ctx.workspace.canonicalize()?;
        let name = format!("rocketry-{}", id());
        let _cleanup = ContainerCleanup(name.clone());
        let mut c = Command::new("docker");
        c.args([
            "run",
            "--rm",
            "--name",
            &name,
            "--network",
            "none",
            "--cpus",
            "1",
            "--memory",
            "256m",
            "--pids-limit",
            "64",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--read-only",
            "--tmpfs",
            "/tmp:rw,nosuid,size=32m",
            "--mount",
        ])
        .arg(format!(
            "type=bind,src={},dst=/workspace",
            workspace.display()
        ))
        .args(["--workdir", "/workspace"]);
        #[cfg(unix)]
        {
            c.arg("--user")
                .arg(format!("{}:{}", unsafe { libc::getuid() }, unsafe {
                    libc::getgid()
                }));
        }
        c.arg(&self.image);
        if op == "execute" {
            c.args([
                "/bin/sh",
                "-c",
                args["command"].as_str().context("command required")?,
            ]);
        } else {
            c.args([
                "python3",
                "-c",
                include_str!("sandbox.py"),
                op,
                &args.to_string(),
            ]);
        }
        let result = process(c, ctx.cancel).await?;
        if op == "execute" {
            Ok(result)
        } else {
            anyhow::ensure!(
                result["exit_code"] == 0,
                "container operation failed: {}",
                result["stderr"]
            );
            Ok(serde_json::from_str(
                result["stdout"].as_str().unwrap_or(""),
            )?)
        }
    }
}
pub struct Builtin {
    spec: ToolSpec,
    backend: Arc<dyn ExecutionBackend>,
    store: Store,
}
#[async_trait]
impl Tool for Builtin {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }
    async fn execute(&self, args: Value, ctx: ToolContext) -> Result<Value> {
        match self.spec.name.as_str() {
            "memory_put" => {
                self.store
                    .memory_put(
                        &ctx.namespace,
                        args["key"].as_str().context("key required")?,
                        args["value"].as_str().context("value required")?,
                    )
                    .await?;
                Ok(json!({"stored":true}))
            }
            "memory_search" => {
                self.store
                    .memory_search(
                        &ctx.namespace,
                        args["query"].as_str().context("query required")?,
                    )
                    .await
            }
            _ => self.backend.execute(&self.spec.name, args, ctx).await,
        }
    }
}
pub fn builtins(backend: Arc<dyn ExecutionBackend>, store: Store) -> ToolRegistry {
    let mut map = ToolRegistry::new();
    for (name, description, effect, fields) in [
        (
            "read_file",
            "Read a UTF-8 workspace file",
            Effect::Read,
            vec!["path"],
        ),
        (
            "write_file",
            "Write a UTF-8 workspace file",
            Effect::Write,
            vec!["path", "content"],
        ),
        (
            "patch_file",
            "Replace exactly one matching text block",
            Effect::Write,
            vec!["path", "old", "new"],
        ),
        (
            "list_dir",
            "List directory entries",
            Effect::Read,
            vec!["path"],
        ),
        (
            "search",
            "Search literal text recursively",
            Effect::Read,
            vec!["path", "query"],
        ),
        (
            "execute",
            "Execute a shell command in the selected backend",
            Effect::Process,
            vec!["command"],
        ),
        (
            "memory_put",
            "Store explicit session-namespace memory",
            Effect::Write,
            vec!["key", "value"],
        ),
        (
            "memory_search",
            "Search explicit memory",
            Effect::Read,
            vec!["query"],
        ),
    ] {
        let props: serde_json::Map<String, Value> = fields
            .iter()
            .map(|f| {
                (
                    f.to_string(),
                    json!({"type":"string","maxLength":MAX_BYTES}),
                )
            })
            .collect();
        let spec = ToolSpec {
            name: name.into(),
            description: description.into(),
            schema: json!({"type":"object","properties":props,"required":fields,"additionalProperties":false}),
            effect,
        };
        map.insert(
            name.into(),
            Arc::new(Builtin {
                spec,
                backend: backend.clone(),
                store: store.clone(),
            }) as Arc<dyn Tool>,
        );
    }
    map
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
#[derive(Default)]
pub enum BackendConfig {
    #[default]
    Disabled,
    Host,
    Docker {
        #[serde(default = "default_image")]
        image: String,
    },
}
fn default_image() -> String {
    "python:3.13-slim".into()
}
impl BackendConfig {
    pub fn build(&self) -> Arc<dyn ExecutionBackend> {
        match self {
            Self::Disabled => Arc::new(DisabledBackend),
            Self::Host => Arc::new(HostBackend),
            Self::Docker { image } => Arc::new(DockerBackend {
                image: image.clone(),
            }),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        assert!(contained(dir.path(), "../secret").is_err());
        assert!(contained(dir.path(), "/etc/passwd").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/tmp", dir.path().join("escape")).unwrap();
            assert!(contained(dir.path(), "escape/file").is_err());
        }
    }
    #[tokio::test]
    async fn process_output_and_cancel() {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", "printf hello"]);
        let r = process(c, tokio_util::sync::CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(r["stdout"], "hello");
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let mut c = Command::new("/bin/sh");
        c.args(["-c", "sleep 30"]);
        assert!(process(c, cancel).await.is_err());
    }
}
