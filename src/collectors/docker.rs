//! Minimal container engine API client over the unix socket. Works with
//! Docker and Podman (which speaks the same REST API on its own socket).
//! Deliberately hand-rolled (no bollard/hyper/tokio) to keep the binary tiny.

#![cfg(unix)]

use crate::state::{Container, Docker};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_CONTAINERS: usize = 50;

enum Probe {
    Found(PathBuf),
    /// A socket exists but we may not connect — worth telling the user,
    /// since its presence proves an engine is installed.
    PermissionDenied(PathBuf),
    NotFound,
}

/// First socket that accepts a connection wins: Docker, then rootful
/// Podman, then rootless Podman. An explicit override skips probing.
fn resolve_socket(over: Option<&Path>) -> Probe {
    let candidates: Vec<PathBuf> = match over {
        Some(p) => vec![p.to_path_buf()],
        None => {
            let mut v = vec![
                PathBuf::from("/var/run/docker.sock"),
                PathBuf::from("/run/podman/podman.sock"),
            ];
            if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
                if !dir.is_empty() {
                    v.push(PathBuf::from(dir).join("podman/podman.sock"));
                }
            }
            v
        }
    };
    let mut denied = None;
    for p in candidates {
        match UnixStream::connect(&p) {
            Ok(_) => return Probe::Found(p),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => denied = Some(p),
            Err(_) => {}
        }
    }
    match denied {
        Some(p) => Probe::PermissionDenied(p),
        None => Probe::NotFound,
    }
}

/// Previous-tick CPU counters per container id: (cpu_total_ns, system_cpu_ns).
pub type PrevCpu = HashMap<String, (u64, u64)>;

#[derive(Deserialize)]
struct ContainerSummary {
    #[serde(rename = "Id", default)]
    id: String,
    #[serde(rename = "Names", default)]
    names: Vec<String>,
    #[serde(rename = "Image", default)]
    image: String,
    #[serde(rename = "State", default)]
    state: String,
    #[serde(rename = "Ports", default)]
    ports: Vec<PortMapping>,
}

#[derive(Deserialize)]
struct PortMapping {
    #[serde(rename = "PublicPort", default)]
    public_port: Option<u16>,
    #[serde(rename = "PrivatePort", default)]
    private_port: Option<u16>,
}

impl PortMapping {
    /// The host-reachable port: the published one, or the private port for
    /// host-network containers (no explicit mapping but the service listens
    /// on the host itself).
    fn host_port(&self) -> Option<u16> {
        self.public_port.or(self.private_port)
    }
}

#[derive(Deserialize, Default)]
struct Stats {
    #[serde(default)]
    cpu_stats: CpuStats,
    #[serde(default)]
    memory_stats: MemoryStats,
}

#[derive(Deserialize, Default)]
struct CpuStats {
    #[serde(default)]
    cpu_usage: CpuUsage,
    #[serde(default)]
    system_cpu_usage: u64,
    #[serde(default)]
    online_cpus: u64,
}

#[derive(Deserialize, Default)]
struct CpuUsage {
    #[serde(default)]
    total_usage: u64,
}

#[derive(Deserialize, Default)]
struct MemoryStats {
    #[serde(default)]
    usage: u64,
    #[serde(default)]
    limit: u64,
    #[serde(default)]
    stats: MemStatsInner,
}

#[derive(Deserialize, Default)]
struct MemStatsInner {
    #[serde(default)]
    inactive_file: u64,
    #[serde(default)]
    cache: u64,
}

/// Collect container stats. On failure returns (None, hint) where the hint
/// tells the dashboard why the panel is empty — the panel stays visible so
/// users can self-diagnose instead of wondering where it went.
/// `prev_cpu` is replaced with this tick's counters for next-tick deltas.
pub fn collect(
    prev_cpu: &mut PrevCpu,
    socket_override: Option<&Path>,
) -> (Option<Docker>, Option<String>) {
    let socket = match resolve_socket(socket_override) {
        Probe::Found(p) => p,
        Probe::PermissionDenied(p) => {
            return (
                None,
                Some(format!(
                    "{} found but permission denied \u{2014} add this user to the docker group",
                    p.display()
                )),
            );
        }
        Probe::NotFound => {
            return (None, Some("no Docker/Podman socket found".to_string()));
        }
    };
    match collect_from(&socket, prev_cpu) {
        Some(docker) => (Some(docker), None),
        None => (
            None,
            Some("engine API error \u{2014} see server log".to_string()),
        ),
    }
}

fn collect_from(socket: &Path, prev_cpu: &mut PrevCpu) -> Option<Docker> {
    // Unversioned paths: the daemon serves them at its own current API
    // version. Versioned paths break both ways — old daemons don't know
    // new versions, and new daemons drop old ones (Docker 29 rejects
    // anything below v1.44).
    let body = get(socket, "/containers/json")?;
    let summaries: Vec<ContainerSummary> = serde_json::from_slice(&body).ok()?;

    let mut next_cpu = PrevCpu::new();
    let mut containers = Vec::new();
    for s in summaries.into_iter().take(MAX_CONTAINERS) {
        let name = s
            .names
            .first()
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_else(|| s.id.chars().take(12).collect());
        let short_id: String = s.id.chars().take(12).collect();

        let mut ports: Vec<u16> = s.ports.iter().filter_map(PortMapping::host_port).collect();
        ports.sort_unstable();
        ports.dedup();

        let mut c = Container {
            id: short_id,
            name,
            image: s.image,
            state: s.state.clone(),
            cpu_pct: 0.0,
            mem_bytes: 0,
            mem_limit: 0,
            ports,
        };

        if s.state == "running" {
            if let Some(stats) = get(
                socket,
                &format!("/containers/{}/stats?stream=false&one-shot=true", s.id),
            )
            .and_then(|b| serde_json::from_slice::<Stats>(&b).ok())
            {
                let cur = (
                    stats.cpu_stats.cpu_usage.total_usage,
                    stats.cpu_stats.system_cpu_usage,
                );
                if let Some(&(prev_total, prev_sys)) = prev_cpu.get(&s.id) {
                    let cpu_delta = cur.0.saturating_sub(prev_total) as f64;
                    let sys_delta = cur.1.saturating_sub(prev_sys) as f64;
                    if sys_delta > 0.0 {
                        let cores = stats.cpu_stats.online_cpus.max(1) as f64;
                        c.cpu_pct = (cpu_delta / sys_delta * cores * 100.0 * 10.0).round() / 10.0;
                    }
                }
                next_cpu.insert(s.id.clone(), cur);

                let m = &stats.memory_stats;
                // cgroup v2 reports inactive_file; v1 reports cache. Subtract
                // whichever is present so page cache doesn't count as "used".
                let reclaimable = if m.stats.inactive_file > 0 {
                    m.stats.inactive_file
                } else {
                    m.stats.cache
                };
                c.mem_bytes = m.usage.saturating_sub(reclaimable);
                c.mem_limit = m.limit;
            }
        }
        containers.push(c);
    }

    *prev_cpu = next_cpu;
    Some(Docker { containers })
}

/// One HTTP/1.1 request over the unix socket. Connection: close, so we read
/// to EOF and then deal with Content-Length vs chunked framing. Returns the
/// body only when the engine answers 200; any non-200 (404/409/500) is
/// treated as "no usable payload" and yields None.
fn http_call(socket: &Path, method: &str, path: &str) -> Option<Vec<u8>> {
    let mut stream = UnixStream::connect(socket).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .ok()?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n"
    )
    .ok()?;

    let mut raw = Vec::with_capacity(8192);
    stream.take(8 * 1024 * 1024).read_to_end(&mut raw).ok()?;

    let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let headers = std::str::from_utf8(&raw[..header_end]).ok()?;
    let status = headers.split_whitespace().nth(1)?;
    if status != "200" {
        // Log once so API rejections (version mismatch, permissions) are
        // diagnosable instead of silently hiding the panel.
        use std::sync::atomic::{AtomicBool, Ordering};
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            eprintln!("container engine returned HTTP {status} for {method} {path}");
        }
        return None;
    }
    let body = &raw[header_end..];

    if headers
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        decode_chunked(body)
    } else {
        Some(body.to_vec())
    }
}

/// GET helper used by the live metrics collector.
fn get(socket: &Path, path: &str) -> Option<Vec<u8>> {
    http_call(socket, "GET", path)
}

// ─────────────────────────────────────────────────────────────────────────
// Orphan-image garbage collection
// ─────────────────────────────────────────────────────────────────────────

/// Hard cap on re-scan iterations per GC pass. Removing a child image can
/// orphan its parent (no longer referenced by any container), so we re-scan
/// and remove again until a full scan deletes nothing; this bounds that loop.
const MAX_GC_PASSES: usize = 16;

#[derive(Deserialize)]
struct ImageSummary {
    #[serde(rename = "Id", default)]
    id: String,
}

#[derive(Deserialize)]
struct ContainerRef {
    #[serde(rename = "ImageID", default)]
    image_id: String,
}

/// Strip a leading `sha256:` so IDs compare regardless of how the engine
/// reports them (both `/images/json` and `/containers/json` use it).
fn norm(id: &str) -> &str {
    id.strip_prefix("sha256:").unwrap_or(id)
}

/// Pure filter: which image IDs have no container (running, stopped, or
/// otherwise) backed by them. `referenced` holds normalized container
/// ImageIDs; anything not present is an orphan eligible for removal.
fn orphan_ids(image_ids: &[String], referenced: &HashSet<String>) -> Vec<String> {
    image_ids
        .iter()
        .filter(|id| !referenced.contains(norm(id)))
        .cloned()
        .collect()
}

/// Run one GC pass over the engine at `socket`. Returns how many images were
/// removed and, on a structural failure (no socket / API error), a hint
/// explaining why collection was skipped.
pub fn gc_pass(socket_override: Option<&Path>) -> (usize, Option<String>) {
    let socket = match resolve_socket(socket_override) {
        Probe::Found(p) => p,
        Probe::PermissionDenied(p) => {
            return (
                0,
                Some(format!(
                    "{} found but permission denied \u{2014} add this user to the docker group",
                    p.display()
                )),
            );
        }
        Probe::NotFound => return (0, Some("no Docker/Podman socket found".to_string())),
    };

    let referenced = match collect_referenced_ids(&socket) {
        Some(s) => s,
        None => {
            return (
                0,
                Some("engine API error \u{2014} see server log".to_string()),
            )
        }
    };

    let mut removed = 0usize;
    for _ in 0..MAX_GC_PASSES {
        let images = match list_image_ids(&socket) {
            Some(v) => v,
            None => break,
        };
        let orphans = orphan_ids(&images, &referenced);
        if orphans.is_empty() {
            break;
        }
        let mut progressed = false;
        for id in orphans {
            if delete_image(&socket, &id) {
                removed += 1;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    (removed, None)
}

/// Background loop: one GC pass every `interval`, exiting promptly on
/// shutdown. Owned `socket_override` so it can run on its own thread. Waits a
/// short warmup first so startup churn (pulls in flight) isn't deleted.
pub fn gc_loop(interval: Duration, socket_override: Option<PathBuf>) {
    let override_ref = socket_override.as_deref();
    let warmup = interval.min(Duration::from_secs(30));
    if !sleep_interruptible(warmup) {
        return;
    }
    loop {
        let (removed, hint) = gc_pass(override_ref);
        match hint {
            Some(h) => eprintln!("docker image gc: paused ({h})"),
            None if removed > 0 => {
                eprintln!("docker image gc: removed {removed} unused image(s)")
            }
            _ => {}
        }
        if !sleep_interruptible(interval) {
            return;
        }
    }
}

/// Returns false if a shutdown was requested during the wait.
fn sleep_interruptible(dur: Duration) -> bool {
    let step = Duration::from_millis(500);
    let mut remaining = dur;
    while remaining > Duration::ZERO {
        if crate::shutdown::requested() {
            return false;
        }
        let nap = remaining.min(step);
        std::thread::sleep(nap);
        remaining -= nap;
    }
    !crate::shutdown::requested()
}

fn list_image_ids(socket: &Path) -> Option<Vec<String>> {
    // `RepoTags: null` dangling images are still returned here, so they're
    // caught as orphans if nothing references them.
    let body = http_call(socket, "GET", "/images/json")?;
    let imgs: Vec<ImageSummary> = serde_json::from_slice(&body).ok()?;
    Some(imgs.into_iter().map(|i| i.id).collect())
}

fn collect_referenced_ids(socket: &Path) -> Option<HashSet<String>> {
    // `all=true` is what makes this safe: stopped/exited/created containers
    // still count as "using" their image, so we never delete under them.
    let body = http_call(socket, "GET", "/containers/json?all=true")?;
    let conts: Vec<ContainerRef> = serde_json::from_slice(&body).ok()?;
    Some(
        conts
            .into_iter()
            .map(|c| norm(&c.image_id).to_string())
            .collect(),
    )
}

fn delete_image(socket: &Path, id: &str) -> bool {
    // Plain DELETE (no `force`): the engine refuses to remove an image that
    // still backs a container, which is exactly the safety we want. A non-200
    // (409 in-use, 404 already gone, 400 bad id) is "not removed".
    http_call(socket, "DELETE", &format!("/images/{id}")).is_some()
}

/// Decode HTTP/1.1 chunked transfer encoding: hex-size line, chunk bytes,
/// CRLF, repeated until a zero-size chunk.
fn decode_chunked(mut body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(body.len());
    loop {
        let line_end = body.windows(2).position(|w| w == b"\r\n")?;
        let size_str = std::str::from_utf8(&body[..line_end]).ok()?;
        let size = usize::from_str_radix(size_str.trim().split(';').next()?.trim(), 16).ok()?;
        if size == 0 {
            return Some(out);
        }
        let start = line_end + 2;
        let end = start + size;
        if end > body.len() {
            return None;
        }
        out.extend_from_slice(&body[start..end]);
        body = body.get(end + 2..)?;
    }
}

#[cfg(test)]
mod tests {
    use super::{decode_chunked, norm, orphan_ids};
    use std::collections::HashSet;

    #[test]
    fn decodes_chunked_body() {
        let body = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(decode_chunked(body).unwrap(), b"Wikipedia");
    }

    #[test]
    fn rejects_truncated_chunk() {
        assert!(decode_chunked(b"ff\r\nshort\r\n").is_none());
    }

    #[test]
    fn norm_strips_sha256_prefix() {
        assert_eq!(norm("sha256:abc123"), "abc123");
        assert_eq!(norm("abc123"), "abc123");
    }

    #[test]
    fn orphan_ids_excludes_referenced_images() {
        let images = vec![
            "sha256:aaa".to_string(),
            "sha256:bbb".to_string(),
            "sha256:ccc".to_string(),
        ];
        // bbb has no container (running or stopped) referencing it
        let referenced: HashSet<String> = ["aaa", "ccc"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            orphan_ids(&images, &referenced),
            vec!["sha256:bbb".to_string()]
        );
    }

    #[test]
    fn orphan_ids_empty_when_all_referenced() {
        let images = vec!["sha256:aaa".to_string(), "sha256:bbb".to_string()];
        let referenced: HashSet<String> = ["aaa", "bbb"].iter().map(|s| s.to_string()).collect();
        assert!(orphan_ids(&images, &referenced).is_empty());
    }
}
