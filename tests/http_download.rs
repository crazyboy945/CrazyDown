use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const ETAG: &str = "\"crazy-it-v1\"";
const LAST_MODIFIED: &str = "Wed, 21 Oct 2026 07:28:00 GMT";
const CHUNK: usize = 1024;

struct TestServer {
    port: u16,
    ranges: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
}

impl TestServer {
    fn start(data: Vec<u8>, support_ranges: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let data = Arc::new(data);
        let ranges_thread = ranges.clone();
        let stop_thread = stop.clone();
        thread::spawn(move || {
            while !stop_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let data = data.clone();
                        let ranges = ranges_thread.clone();
                        thread::spawn(move || {
                            let _ = handle(stream, &data, support_ranges, &ranges);
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        Self { port, ranges, stop }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn observed_ranges(&self) -> Vec<String> {
        self.ranges.lock().unwrap().clone()
    }

    fn segment_requests(&self) -> Vec<String> {
        self.observed_ranges()
            .into_iter()
            .filter(|range| range != "bytes=0-0")
            .collect()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn handle(
    mut stream: TcpStream,
    data: &[u8],
    support_ranges: bool,
    ranges: &Mutex<Vec<String>>,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let _ = stream.set_nodelay(true);
    let mut request = Vec::new();
    let mut buffer = [0_u8; 2048];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..count]);
        if request.len() > 128 * 1024 {
            return Ok(());
        }
    }

    let head = String::from_utf8_lossy(&request);
    let range_header = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("range")
            .then(|| value.trim().to_string())
    });
    ranges
        .lock()
        .unwrap()
        .push(range_header.clone().unwrap_or_else(|| "<none>".to_string()));

    if !support_ranges {
        return write_full(&mut stream, data, false);
    }

    match range_header {
        Some(header) => match parse_range(&header, data.len()) {
            Some((start, end)) => write_partial(&mut stream, data, start, end),
            None => write_unsatisfiable(&mut stream, data.len()),
        },
        None => write_full(&mut stream, data, true),
    }
}

fn parse_range(header: &str, total: usize) -> Option<(usize, usize)> {
    let spec = header.trim().strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let start: usize = start.trim().parse().ok()?;
    if total == 0 || start >= total {
        return None;
    }
    let end: usize = match end.trim() {
        "" => total - 1,
        value => value.parse::<usize>().ok()?.min(total - 1),
    };
    (end >= start).then_some((start, end))
}

fn write_partial(
    stream: &mut TcpStream,
    data: &[u8],
    start: usize,
    end: usize,
) -> std::io::Result<()> {
    let body = &data[start..=end];
    let header = format!(
        "HTTP/1.1 206 Partial Content\r\n\
         Content-Length: {}\r\n\
         Content-Range: bytes {start}-{end}/{}\r\n\
         Accept-Ranges: bytes\r\n\
         ETag: {ETAG}\r\n\
         Last-Modified: {LAST_MODIFIED}\r\n\
         Connection: close\r\n\r\n",
        body.len(),
        data.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn write_full(stream: &mut TcpStream, data: &[u8], ranges: bool) -> std::io::Result<()> {
    let accept = if ranges {
        "Accept-Ranges: bytes\r\n"
    } else {
        ""
    };
    let header = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Length: {}\r\n\
         {accept}\
         ETag: {ETAG}\r\n\
         Last-Modified: {LAST_MODIFIED}\r\n\
         Connection: close\r\n\r\n",
        data.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(data)?;
    stream.flush()
}

fn write_unsatisfiable(stream: &mut TcpStream, total: usize) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 416 Range Not Satisfiable\r\n\
         Content-Range: bytes */{total}\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\r\n"
    );
    stream.write_all(header.as_bytes())?;
    stream.flush()
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("crazydown-it-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_crazydown"))
        .args(args)
        .output()
        .expect("run crazydown")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "exit={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn test_data(length: usize) -> Vec<u8> {
    (0..length).map(|index| (index % 251) as u8).collect()
}

fn source_key(source: &str) -> String {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn state_path(output: &Path) -> PathBuf {
    let mut name = output.as_os_str().to_os_string();
    name.push(".crazydown.json");
    PathBuf::from(name)
}

#[test]
fn segmented_download_writes_exact_bytes() {
    let data = test_data(300 * CHUNK);
    let server = TestServer::start(data.clone(), true);
    let dir = temp_dir("segmented");
    let output = dir.join("file.bin");
    let url = server.url("/file.bin");

    let result = run(&["-q", "-s", "4", "-o", output.to_str().unwrap(), &url]);
    assert_success(&result);
    assert_eq!(fs::read(&output).unwrap(), data);
    assert!(!state_path(&output).exists());

    let segments = server.segment_requests();
    assert!(
        segments.len() >= 4,
        "expected at least 4 segment requests, got {segments:?}"
    );
    assert!(segments.iter().all(|range| range.starts_with("bytes=")));

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn resumes_from_saved_state_and_requests_only_missing_ranges() {
    let total = 256 * CHUNK;
    let data = test_data(total);
    let half = total / 2;
    let server = TestServer::start(data.clone(), true);
    let dir = temp_dir("resume");
    let output = dir.join("partial.bin");
    let url = server.url("/partial.bin");

    let mut file = fs::File::create(&output).unwrap();
    file.write_all(&data[..half]).unwrap();
    file.set_len(total as u64).unwrap();
    drop(file);

    let state = format!(
        "{{\n  \"version\": 1,\n  \"source_key\": \"{}\",\n  \"total\": {total},\n  \"mode\": \"segmented\",\n  \"etag\": \"\\\"crazy-it-v1\\\"\",\n  \"last_modified\": \"{LAST_MODIFIED}\",\n  \"segments\": [\n    {{\"start\": 0, \"end\": {}, \"done\": {half}}},\n    {{\"start\": {half}, \"end\": {}, \"done\": 0}}\n  ],\n  \"downloaded\": {half}\n}}\n",
        source_key(&url),
        half - 1,
        total - 1,
    );
    fs::write(state_path(&output), state).unwrap();

    let result = run(&["-q", "-o", output.to_str().unwrap(), &url]);
    assert_success(&result);
    assert_eq!(fs::read(&output).unwrap(), data);
    assert!(!state_path(&output).exists());

    let expected = format!("bytes={half}-{}", total - 1);
    assert_eq!(server.segment_requests(), vec![expected]);

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn falls_back_to_single_stream_without_range_support() {
    let data = test_data(200 * CHUNK);
    let server = TestServer::start(data.clone(), false);
    let dir = temp_dir("no-range");
    let output = dir.join("stream.bin");
    let url = server.url("/stream.bin");

    let result = run(&["-q", "-o", output.to_str().unwrap(), &url]);
    assert_success(&result);
    assert_eq!(fs::read(&output).unwrap(), data);
    assert!(!state_path(&output).exists());
    assert!(
        server
            .observed_ranges()
            .iter()
            .any(|range| range == "<none>")
    );

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn empty_remote_file_creates_empty_output() {
    let server = TestServer::start(Vec::new(), true);
    let dir = temp_dir("empty");
    let output = dir.join("empty.bin");
    let url = server.url("/empty.bin");

    let result = run(&["-q", "-o", output.to_str().unwrap(), &url]);
    assert_success(&result);
    assert!(output.exists());
    assert_eq!(fs::metadata(&output).unwrap().len(), 0);
    assert!(!state_path(&output).exists());

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn batch_list_downloads_multiple_files() {
    let data = test_data(120 * CHUNK);
    let server = TestServer::start(data.clone(), true);
    let dir = temp_dir("batch");
    let list = dir.join("urls.txt");
    let url_a = server.url("/a.bin");
    let url_b = server.url("/b.bin");
    fs::write(
        &list,
        format!("# batch test\n{url_a}  alpha.bin\n{url_b}  beta.bin\n"),
    )
    .unwrap();

    let result = run(&[
        "-q",
        "-i",
        list.to_str().unwrap(),
        "-d",
        dir.to_str().unwrap(),
        "-j",
        "2",
    ]);
    assert_success(&result);
    assert_eq!(fs::read(dir.join("alpha.bin")).unwrap(), data);
    assert_eq!(fs::read(dir.join("beta.bin")).unwrap(), data);

    fs::remove_dir_all(dir).unwrap();
}
