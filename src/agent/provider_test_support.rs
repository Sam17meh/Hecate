use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub(crate) struct MockHttpResponse {
    pub status: u16,
    pub body: String,
    pub delay: Duration,
    pub declared_length: Option<usize>,
    pub omit_content_length: bool,
    pub location: Option<String>,
}

impl MockHttpResponse {
    pub(crate) fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            delay: Duration::ZERO,
            declared_length: None,
            omit_content_length: false,
            location: None,
        }
    }

    pub(crate) fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    pub(crate) fn with_declared_length(mut self, length: usize) -> Self {
        self.declared_length = Some(length);
        self
    }

    pub(crate) fn without_content_length(mut self) -> Self {
        self.omit_content_length = true;
        self
    }

    pub(crate) fn redirect_to(mut self, url: impl Into<String>) -> Self {
        self.location = Some(url.into());
        self
    }
}

pub(crate) struct MockHttpServer {
    pub base_url: String,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    worker: Option<JoinHandle<()>>,
}

impl MockHttpServer {
    pub(crate) fn start(responses: Vec<MockHttpResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let address = listener.local_addr().expect("read local test address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let thread_requests = requests.clone();
        let worker = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept provider request");
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .expect("set mock server read timeout");
                let request = read_request(&mut stream);
                thread_requests.lock().unwrap().push(request);
                if !response.delay.is_zero() {
                    thread::sleep(response.delay);
                }
                let reason = match response.status {
                    200 => "OK",
                    401 => "Unauthorized",
                    403 => "Forbidden",
                    429 => "Too Many Requests",
                    500 => "Internal Server Error",
                    502 => "Bad Gateway",
                    503 => "Service Unavailable",
                    _ => "Response",
                };
                let mut headers = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\n",
                    response.status, reason
                );
                if !response.omit_content_length {
                    let content_length = response.declared_length.unwrap_or(response.body.len());
                    headers.push_str(&format!("Content-Length: {content_length}\r\n"));
                }
                if let Some(location) = response.location {
                    headers.push_str(&format!("Location: {location}\r\n"));
                }
                headers.push_str("Connection: close\r\n\r\n");
                let _ = stream.write_all(headers.as_bytes());
                let _ = stream.write_all(response.body.as_bytes());
                let _ = stream.flush();
            }
        });
        Self {
            base_url: format!("http://{address}/v1/"),
            requests,
            worker: Some(worker),
        }
    }

    pub(crate) fn requests(&self) -> Vec<Vec<u8>> {
        self.requests.lock().unwrap().clone()
    }

    pub(crate) fn finish(mut self) {
        if let Some(worker) = self.worker.take() {
            worker.join().expect("mock server worker did not panic");
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let count = stream.read(&mut buffer).expect("read request headers");
        assert!(count > 0, "connection closed before request headers");
        request.extend_from_slice(&buffer[..count]);
        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        assert!(request.len() < 64 * 1024, "request headers were too large");
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while request.len() < header_end + content_length {
        let count = stream.read(&mut buffer).expect("read request body");
        assert!(count > 0, "connection closed before request body completed");
        request.extend_from_slice(&buffer[..count]);
    }
    request
}
