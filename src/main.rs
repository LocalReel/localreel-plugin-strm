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

    for stream in listener.incoming().flatten() {
        let _ = handle(stream, &context, &token);
    }
}

fn handle(mut stream: TcpStream, context: &Context, token: &str) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut authorized = token.is_empty();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
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
    collect(&dir, recursive, &mut items);
    let body = json!({ "entries": items });
    respond(stream, 200, "application/json", body.to_string().as_bytes())
}

fn collect(dir: &Path, recursive: bool, out: &mut Vec<Value>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if recursive {
                collect(&path, true, out);
            }
            continue;
        }
        if !path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("strm"))
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let url = text.trim();
        if !is_http(url) {
            continue;
        }
        let metadata = entry.metadata().ok();
        out.push(json!({
            "uri": url,
            "name": strm_name(&path, url),
            "is_dir": false,
            "size": 0,
            "modified": metadata.as_ref().and_then(modified),
        }));
    }
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
