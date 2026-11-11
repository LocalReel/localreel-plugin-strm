//! `.strm` storage provider.
//!
//! A source directory holds small text files; each `.strm` file contains an
//! HTTP(S) URL to a media file. The plugin lists those files as remote entries
//! and proxies reads with HTTP Range, so LocalReel can probe, thumbnail and
//! stream them without downloading the whole file first.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

struct Context {
    root: PathBuf,
    agent: ureq::Agent,
}

fn main() {
    let token = std::env::var("LOCALREEL_PLUGIN_TOKEN").unwrap_or_default();
    let config: Value = std::env::var("LOCALREEL_PLUGIN_CONFIG")
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| json!({}));
    let root = config
        .get("root")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .expect("LOCALREEL_PLUGIN_CONFIG must contain root");
    let context = Context {
        root: root.canonicalize().expect("plugin root must exist"),
        agent: ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(60))
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(60))
            .build(),
    };

    // Exit when the host closes stdin, so no orphan keeps the port alive.
    std::thread::spawn(|| {
        let mut stdin = std::io::stdin();
        let mut buffer = [0u8; 64];
        while matches!(stdin.read(&mut buffer), Ok(read) if read > 0) {}
        std::process::exit(0);
    });

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    println!("{}", json!({ "ready": true, "port": port }));
    std::io::stdout().flush().ok();

    let context = std::sync::Arc::new(context);
    let token = std::sync::Arc::new(token);
    let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for stream in listener.incoming().flatten() {
        // Never queue unbounded threads/sockets. Dropping an overloaded socket
        // lets the host retry instead of blocking the accept loop.
        if active
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |n| (n < 16).then_some(n + 1),
            )
            .is_err()
        {
            continue;
        }
        let context = context.clone();
        let token = token.clone();
        let active = active.clone();
        std::thread::spawn(move || {
            struct Permit(std::sync::Arc<std::sync::atomic::AtomicUsize>);
            impl Drop for Permit {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, std::sync::atomic::Ordering::Release);
                }
            }
            let _permit = Permit(active);
            let _ = handle(stream, &context, &token);
        });
    }
}

fn handle(mut stream: TcpStream, context: &Context, token: &str) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    // Take bounds allocation even when a peer sends no newline. Socket timeout
    // bounds idle reads; the elapsed deadline also bounds slow header trickles.
    let mut reader = BufReader::new(stream.try_clone()?.take(16 * 1024));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut request_line = String::new();
    if header_line(&mut reader, &mut request_line, deadline)? == 0 {
        return Ok(());
    }
    let mut authorized = token.is_empty();
    let mut line = String::new();
    loop {
        line.clear();
        if header_line(&mut reader, &mut line, deadline)? == 0
            || std::time::Instant::now() >= deadline
        {
            return respond(
                &mut stream,
                400,
                "text/plain",
                b"incomplete or oversized headers",
            );
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("x-localreel-token") && value.trim() == token {
                authorized = true;
            }
        }
    }
    if !authorized {
        return respond(&mut stream, 403, "text/plain", b"forbidden");
    }
    let target = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    match path {
        "/v1/health" => respond(&mut stream, 200, "application/json", b"{\"ok\":true}"),
        "/v1/stat" => stat(&mut stream, context, query),
        "/v1/list" => entries(&mut stream, context, query, false),
        "/v1/walk" => entries(&mut stream, context, query, true),
        "/v1/read" => read(&mut stream, context, query),
        _ => respond(&mut stream, 404, "text/plain", b"unknown route"),
    }
}

fn header_line(
    reader: &mut impl BufRead,
    line: &mut String,
    deadline: std::time::Instant,
) -> std::io::Result<usize> {
    let mut count = 0;
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "header deadline",
            ));
        }
        let mut byte = [0u8];
        if reader.read(&mut byte)? == 0 {
            return Ok(count);
        }
        count += 1;
        line.push(char::from(byte[0]));
        if byte[0] == b'\n' {
            return Ok(count);
        }
    }
}

fn stat(stream: &mut TcpStream, context: &Context, query: &str) -> std::io::Result<()> {
    let Some(uri) = param(query, "uri") else {
        return respond(stream, 400, "text/plain", b"missing uri");
    };
    if is_http(&uri) {
        return match remote_head(context, &uri) {
            Some((size, modified)) => {
                let body = json!({ "size": size, "modified": modified, "is_dir": false });
                respond(stream, 200, "application/json", body.to_string().as_bytes())
            }
            None => respond(stream, 404, "text/plain", b"missing"),
        };
    }
    let Some(path) = local(context, &uri) else {
        return respond(stream, 404, "text/plain", b"missing");
    };
    let Ok(metadata) = std::fs::metadata(&path) else {
        return respond(stream, 404, "text/plain", b"missing");
    };
    let body = json!({
        "size": metadata.len(),
        "modified": modified(&metadata),
        "is_dir": metadata.is_dir(),
    });
    respond(stream, 200, "application/json", body.to_string().as_bytes())
}

fn entries(
    stream: &mut TcpStream,
    context: &Context,
    query: &str,
    recursive: bool,
) -> std::io::Result<()> {
    let Some(uri) = param(query, "uri") else {
        return respond(stream, 400, "text/plain", b"missing uri");
    };
    let Some(dir) = local(context, &uri) else {
        return respond(stream, 404, "text/plain", b"missing");
    };
    let mut items = Vec::new();
    if collect(&dir, recursive, &mut items).is_err() {
        return respond(
            stream,
            500,
            "text/plain",
            b"source traversal failed; scan is incomplete",
        );
    }
    let body = json!({ "entries": items });
    respond(stream, 200, "application/json", body.to_string().as_bytes())
}

fn collect(dir: &Path, recursive: bool, out: &mut Vec<Value>) -> std::io::Result<()> {
    let mut seen = out
        .iter()
        .filter_map(|item| item.get("uri").and_then(Value::as_str).map(str::to_owned))
        .collect();
    collect_unique(dir, recursive, out, &mut seen)
}

fn collect_unique(
    dir: &Path,
    recursive: bool,
    out: &mut Vec<Value>,
    seen: &mut std::collections::HashSet<String>,
) -> std::io::Result<()> {
    let mut entries = std::fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let kind = entry.file_type()?;
        // Do not follow links outside the granted tree or into directory cycles.
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            if recursive {
                collect_unique(&path, true, out, seen)?;
            }
            continue;
        }
        if !path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("strm"))
        {
            continue;
        }
        let text = std::fs::read_to_string(&path)?;
        let url = text.trim();
        if !is_http(url) {
            continue;
        }
        // The remote URL is the provider identity: aliases intentionally produce
        // one entry, with the first lexically traversed filename winning.
        if !seen.insert(url.to_owned()) {
            continue;
        }
        let metadata = entry.metadata()?;
        out.push(json!({
            "uri": url,
            "name": strm_name(&path, url),
            "is_dir": false,
            "size": 0,
            "modified": modified(&metadata),
        }));
    }
    Ok(())
}

fn read(stream: &mut TcpStream, context: &Context, query: &str) -> std::io::Result<()> {
    let Some(uri) = param(query, "uri") else {
        return respond(stream, 400, "text/plain", b"missing uri");
    };
    let start = param(query, "start")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let len = param(query, "len")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    if len > 8 * 1024 * 1024 {
        return respond(stream, 400, "text/plain", b"read length exceeds 8 MiB");
    }
    if is_http(&uri) {
        return match remote_read(context, &uri, start, len) {
            Some(bytes) => respond(stream, 200, "application/octet-stream", &bytes),
            None => respond(stream, 404, "text/plain", b"missing"),
        };
    }
    let Some(path) = local(context, &uri) else {
        return respond(stream, 404, "text/plain", b"missing");
    };
    let Ok(mut file) = std::fs::File::open(&path) else {
        return respond(stream, 404, "text/plain", b"missing");
    };
    use std::io::{Seek, SeekFrom};
    let size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    if start >= size {
        return respond(stream, 200, "application/octet-stream", b"");
    }
    file.seek(SeekFrom::Start(start))?;
    let length = len.min(size - start) as usize;
    let mut body = vec![0u8; length];
    let mut read = 0usize;
    while read < length {
        match file.read(&mut body[read..])? {
            0 => break,
            count => read += count,
        }
    }
    body.truncate(read);
    respond(stream, 200, "application/octet-stream", &body)
}

fn remote_head(context: &Context, url: &str) -> Option<(u64, Option<i64>)> {
    let response = match context.agent.head(url).call() {
        Ok(response) => response,
        Err(_) => context
            .agent
            .get(url)
            .set("Range", "bytes=0-0")
            .call()
            .ok()?,
    };
    // On a 206 fallback the real size is in Content-Range, not Content-Length.
    let size = response
        .header("Content-Range")
        .and_then(|value| value.rsplit('/').next())
        .and_then(|value| value.parse().ok())
        .or_else(|| {
            response
                .header("Content-Length")
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(0);
    let modified = response
        .header("Last-Modified")
        .and_then(|value| httpdate::parse_http_date(value).ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64);
    Some((size, modified))
}

fn remote_read(context: &Context, url: &str, start: u64, len: u64) -> Option<Vec<u8>> {
    if len == 0 {
        return Some(Vec::new());
    }
    let end = start.saturating_add(len).saturating_sub(1);
    let response = context
        .agent
        .get(url)
        .set("Range", &format!("bytes={start}-{end}"))
        .call()
        .ok()?;
    let status = response.status();
    let mut reader = response.into_reader();
    let mut bytes = Vec::new();
    if status == 206 {
        reader.take(len).read_to_end(&mut bytes).ok()?;
    } else {
        std::io::copy(&mut (&mut reader).take(start), &mut std::io::sink()).ok()?;
        reader.take(len).read_to_end(&mut bytes).ok()?;
    }
    Some(bytes)
}

fn local(context: &Context, uri: &str) -> Option<PathBuf> {
    let path = if let Some(rest) = uri.strip_prefix("file://") {
        file_uri_path(rest)
    } else {
        let path = PathBuf::from(uri);
        if path.is_absolute() {
            path
        } else {
            context.root.join(path)
        }
    };
    let canonical = path.canonicalize().ok()?;
    if canonical.starts_with(&context.root) {
        Some(canonical)
    } else {
        None
    }
}

fn file_uri_path(rest: &str) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(rest.trim_start_matches('/').replace('/', "\\"))
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(rest)
    }
}

fn is_http(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

fn strm_name(path: &Path, url: &str) -> String {
    let stem = path
        .file_stem()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default();
    let clean = url.split('?').next().unwrap_or(url);
    let extension = Path::new(clean)
        .extension()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default();
    if extension.is_empty() {
        stem
    } else {
        format!("{stem}.{extension}")
    }
}

fn param(query: &str, name: &str) -> Option<String> {
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if percent_decode(key) == name {
            return Some(percent_decode(value));
        }
    }
    None
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        if bytes[index] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[index]);
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn modified(metadata: &std::fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_secs() as i64)
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_failure_is_not_an_empty_success() {
        let missing = std::env::temp_dir().join(format!("strm-missing-{}", std::process::id()));
        assert!(collect(&missing, true, &mut Vec::new()).is_err());
    }

    #[test]
    fn duplicate_urls_have_one_deterministic_identity() {
        let root = std::env::temp_dir().join(format!("strm-alias-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("b.strm"), "https://example.com/movie.mp4").unwrap();
        std::fs::write(root.join("a.strm"), "https://example.com/movie.mp4").unwrap();
        let mut items = Vec::new();
        collect(&root, true, &mut items).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["name"], "a.mp4");
        // An unreadable/invalid UTF-8 STRM must fail the entire scan too.
        std::fs::write(root.join("bad.strm"), [255]).unwrap();
        assert!(collect(&root, true, &mut Vec::new()).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expired_header_deadline_is_rejected() {
        let mut reader = std::io::Cursor::new(b"GET / HTTP/1.1\r\n");
        assert!(header_line(&mut reader, &mut String::new(), std::time::Instant::now()).is_err());
    }

    #[test]
    fn names_take_the_extension_from_the_url() {
        assert_eq!(
            strm_name(
                Path::new("Show.S01E01.strm"),
                "http://nas.local/Show.S01E01.mp4?token=1"
            ),
            "Show.S01E01.mp4"
        );
        assert_eq!(
            strm_name(Path::new("README.strm"), "http://nas.local/no-extension"),
            "README"
        );
    }

    #[test]
    fn decodes_query_values() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("plain"), "plain");
    }

    #[test]
    fn detects_http_urls() {
        assert!(is_http("http://x"));
        assert!(is_http("https://x"));
        assert!(!is_http("ftp://x"));
        assert!(!is_http("/local/x"));
    }
}
