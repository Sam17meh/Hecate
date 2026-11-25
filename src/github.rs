use flate2::read::GzDecoder;
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, USER_AGENT};
use serde::Deserialize;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_SOURCE_ARCHIVE_BYTES: u64 = 200 * 1024 * 1024;
const MAX_EXTRACTED_SOURCE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_GITHUB_JSON_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_GITHUB_ERROR_RESPONSE_BYTES: u64 = 4 * 1024;
const MAX_GITHUB_ISSUES_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_GITHUB_ISSUES: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubRepository {
    pub owner: String,
    pub name: String,
    pub default_branch: String,
    pub commit: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubIssue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub url: String,
    pub comments: u64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug)]
pub struct GitHubClient {
    http: Client,
    api_base: String,
    cache_dir: PathBuf,
}

#[derive(Deserialize)]
struct RepositoryResponse {
    name: String,
    html_url: String,
    default_branch: String,
}

#[derive(Deserialize)]
struct CommitResponse {
    sha: String,
}

#[derive(Deserialize)]
struct IssueResponse {
    number: u64,
    title: String,
    #[serde(default)]
    body: Option<String>,
    html_url: String,
    #[serde(default)]
    comments: u64,
    created_at: String,
    updated_at: String,
    #[serde(default)]
    labels: Vec<LabelResponse>,
    pull_request: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct LabelResponse {
    name: String,
}

impl GitHubClient {
    pub fn new() -> Result<Self, String> {
        let cache_dir = std::env::var_os("HECATE_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("hecate-cache"));
        Self::with_config("https://api.github.com".to_string(), cache_dir)
    }

    fn with_config(api_base: String, cache_dir: PathBuf) -> Result<Self, String> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| format!("Could not initialize GitHub client: {error}"))?;
        Ok(Self {
            http,
            api_base,
            cache_dir,
        })
    }

    pub fn parse_repository(input: &str) -> Result<(String, String), String> {
        let value = input.trim();
        let path = if value.starts_with("https://") || value.starts_with("http://") {
            let url = reqwest::Url::parse(value).map_err(|_| "Invalid GitHub URL".to_string())?;
            if url.host_str() != Some("github.com") || url.scheme() != "https" {
                return Err("Repository URL must use https://github.com/owner/repo".to_string());
            }
            url.path().trim_matches('/').to_string()
        } else {
            value.to_string()
        };
        let path = path.strip_suffix(".git").unwrap_or(&path);
        let parts: Vec<_> = path.split('/').collect();
        if parts.len() != 2 || parts.iter().any(|part| !valid_segment(part)) {
            return Err(format!("Invalid repository target: {input}"));
        }
        Ok((parts[0].to_string(), parts[1].to_string()))
    }

    fn request(&self, url: &str) -> Result<Response, String> {
        let mut request = self
            .http
            .get(url)
            .header(USER_AGENT, "hecate-repository-intelligence")
            .header(ACCEPT, "application/vnd.github+json");
        let token = std::env::var("GITHUB_TOKEN")
            .or_else(|_| std::env::var("GH_TOKEN"))
            .ok()
            .filter(|value| !value.trim().is_empty());
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .map_err(|error| format!("GitHub network request failed: {error}"))?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        if status.as_u16() == 403 || status.as_u16() == 429 {
            return Err("GitHub API rate limit reached or access was denied; retry later or configure authentication".to_string());
        }
        if status.as_u16() == 404 {
            return Err(
                "GitHub repository or resource was not found, or is not public".to_string(),
            );
        }
        let detail = read_response_limited(response, MAX_GITHUB_ERROR_RESPONSE_BYTES)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_else(|_| "<error response omitted>".to_string());
        Err(format!(
            "GitHub API returned HTTP {status}: {}",
            detail.chars().take(300).collect::<String>()
        ))
    }

    fn json<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T, String> {
        self.json_with_limit(url, MAX_GITHUB_JSON_RESPONSE_BYTES)
            .map(|(value, _)| value)
    }

    fn json_with_limit<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
        limit: u64,
    ) -> Result<(T, u64), String> {
        let bytes = read_response_limited(self.request(url)?, limit)?;
        let length = bytes.len() as u64;
        let value = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Could not decode GitHub API response: {error}"))?;
        Ok((value, length))
    }

    pub fn repository(&self, input: &str) -> Result<GitHubRepository, String> {
        let (owner, name) = Self::parse_repository(input)?;
        let base = format!(
            "{}/repos/{owner}/{name}",
            self.api_base.trim_end_matches('/')
        );
        let metadata: RepositoryResponse = self.json(&base)?;
        let commit: CommitResponse =
            self.json(&format!("{base}/commits/{}", metadata.default_branch))?;
        Ok(GitHubRepository {
            owner: owner,
            name: metadata.name,
            default_branch: metadata.default_branch,
            commit: commit.sha,
            url: metadata.html_url,
        })
    }

    pub fn source_checkout(&self, repository: &GitHubRepository) -> Result<PathBuf, String> {
        let cache = self
            .cache_dir
            .join("repositories")
            .join(format!(
                "{}-{}",
                safe_cache_part(&repository.owner),
                safe_cache_part(&repository.name)
            ))
            .join(&repository.commit);
        let marker = cache.join(".hecate-checkout");
        if marker.is_file() {
            report_skipped_links(&cache);
            return repository_root(&cache);
        }
        fs::create_dir_all(&cache)
            .map_err(|error| format!("Could not create repository cache: {error}"))?;
        let archive_url = format!(
            "{}/repos/{}/{}/tarball/{}",
            self.api_base.trim_end_matches('/'),
            repository.owner,
            repository.name,
            repository.commit
        );
        let response = self.request(&archive_url)?;
        let skipped_links = unpack_github_archive(response, &cache)?;
        if skipped_links > 0 {
            fs::write(
                cache.join(".hecate-skipped-links"),
                skipped_links.to_string(),
            )
            .map_err(|error| format!("Could not record skipped archive links: {error}"))?;
        }
        fs::write(&marker, &repository.commit)
            .map_err(|error| format!("Could not mark repository cache complete: {error}"))?;
        report_skipped_links(&cache);
        repository_root(&cache)
    }

    pub fn open_issues(&self, repository: &GitHubRepository) -> Result<Vec<GitHubIssue>, String> {
        let mut issues = Vec::new();
        let mut response_bytes = 0u64;
        let base = self.api_base.trim_end_matches('/');
        for page in 1..=1000 {
            let url = format!(
                "{base}/repos/{}/{}/issues?state=open&per_page=100&page={page}",
                repository.owner, repository.name
            );
            let (records, page_bytes): (Vec<IssueResponse>, u64) =
                self.json_with_limit(&url, MAX_GITHUB_JSON_RESPONSE_BYTES)?;
            let count = records.len();
            let new_issue_count = records
                .iter()
                .filter(|issue| issue.pull_request.is_none())
                .count();
            response_bytes =
                checked_issue_budgets(response_bytes, page_bytes, issues.len(), new_issue_count)?;
            issues.extend(
                records
                    .into_iter()
                    .filter(|issue| issue.pull_request.is_none())
                    .map(|issue| GitHubIssue {
                        number: issue.number,
                        title: issue.title,
                        body: issue.body.unwrap_or_default(),
                        labels: issue.labels.into_iter().map(|label| label.name).collect(),
                        url: issue.html_url,
                        comments: issue.comments,
                        created_at: issue.created_at,
                        updated_at: issue.updated_at,
                    }),
            );
            if count < 100 {
                return Ok(issues);
            }
        }
        Err("GitHub issue pagination exceeded 1000 pages".to_string())
    }
}

fn read_response_limited(response: Response, limit: u64) -> Result<Vec<u8>, String> {
    let limit_plus_one = limit.saturating_add(1);
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        return Err(format!("GitHub response exceeds the {limit} byte limit"));
    }
    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or(0)
            .min(limit)
            .min(64 * 1024) as usize,
    );
    response
        .take(limit_plus_one)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read GitHub response".to_string())?;
    if bytes.len() as u64 > limit {
        return Err(format!("GitHub response exceeds the {limit} byte limit"));
    }
    Ok(bytes)
}

fn checked_issue_budgets(
    response_bytes: u64,
    page_bytes: u64,
    issue_count: usize,
    new_issue_count: usize,
) -> Result<u64, String> {
    let total_bytes = response_bytes.saturating_add(page_bytes);
    if total_bytes > MAX_GITHUB_ISSUES_RESPONSE_BYTES {
        return Err("GitHub issue data exceeds the 64 MiB aggregate response limit".into());
    }
    if issue_count.saturating_add(new_issue_count) > MAX_GITHUB_ISSUES {
        return Err("GitHub issue listing exceeds the 10,000 issue limit".into());
    }
    Ok(total_bytes)
}

fn valid_segment(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn safe_cache_part(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn unpack_github_archive(response: Response, destination: &Path) -> Result<usize, String> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_SOURCE_ARCHIVE_BYTES)
    {
        return Err("GitHub source archive exceeds the 200 MiB download limit".to_string());
    }
    let decoder = GzDecoder::new(response.take(MAX_SOURCE_ARCHIVE_BYTES + 1));
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| format!("Could not read GitHub source archive: {error}"))?;
    let mut extracted_bytes = 0u64;
    let mut skipped_links = 0usize;
    for entry in entries {
        let mut entry = entry.map_err(|error| format!("Invalid GitHub source archive: {error}"))?;
        let entry_type = entry.header().entry_type();
        // Repository links are optional for static analysis and may require elevated
        // privileges on Windows. Never create archive-controlled links on disk.
        if entry_type.is_symlink() || entry_type.is_hard_link() {
            skipped_links += 1;
            continue;
        }
        extracted_bytes = extracted_bytes.saturating_add(entry.size());
        if extracted_bytes > MAX_EXTRACTED_SOURCE_BYTES {
            return Err("GitHub source archive exceeds the 1 GiB extracted size limit".to_string());
        }
        let archive_path = entry
            .path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "<invalid archive path>".to_string());
        entry.unpack_in(destination).map_err(|error| {
            format!("Could not extract GitHub archive entry `{archive_path}`: {error:?}")
        })?;
    }
    let roots: Vec<_> = fs::read_dir(destination)
        .map_err(|error| format!("Could not inspect extracted source: {error}"))?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .collect();
    if roots.len() == 1 {
        return Ok(skipped_links);
    }
    Err("GitHub source archive did not contain one repository root".to_string())
}

fn report_skipped_links(cache: &Path) {
    if let Ok(value) = fs::read_to_string(cache.join(".hecate-skipped-links")) {
        if let Ok(count) = value.parse::<usize>() {
            if count > 0 {
                eprintln!(
                    "Warning: skipped {count} archive symlink(s)/hard link(s); linked files may be absent from static analysis."
                );
            }
        }
    }
}

fn repository_root(cache: &Path) -> Result<PathBuf, String> {
    let roots: Vec<_> = fs::read_dir(cache)
        .map_err(|error| format!("Could not inspect repository cache: {error}"))?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .collect();
    if roots.len() == 1 {
        Ok(roots[0].path())
    } else {
        Err("Cached source does not contain one repository root".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use serde_json::json;
    use std::io::Write;
    use std::net::TcpListener;
    use std::thread;

    fn mock_server(responses: Vec<(u16, String)>) -> String {
        mock_byte_server(
            responses
                .into_iter()
                .map(|(status, body)| (status, body.into_bytes()))
                .collect(),
        )
    }

    fn mock_byte_server(responses: Vec<(u16, Vec<u8>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 2048];
                let read = stream.read(&mut request).unwrap();
                let _request_text = String::from_utf8_lossy(&request[..read]);
                let reason = if status == 200 { "OK" } else { "Not Found" };
                write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        format!("http://{address}")
    }

    fn mock_close_delimited_server(status: u16, body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 2048];
            let _ = stream.read(&mut request).unwrap();
            let reason = if status == 200 {
                "OK"
            } else {
                "Internal Server Error"
            };
            write!(
                stream,
                "HTTP/1.1 {status} {reason}\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        });
        format!("http://{address}")
    }

    #[test]
    fn accepts_repository_identifiers_and_github_urls() {
        assert_eq!(
            GitHubClient::parse_repository("owner/repo").unwrap(),
            ("owner".into(), "repo".into())
        );
        assert_eq!(
            GitHubClient::parse_repository("https://github.com/owner/repo.git").unwrap(),
            ("owner".into(), "repo".into())
        );
        assert!(GitHubClient::parse_repository("https://example.com/owner/repo").is_err());
        assert!(GitHubClient::parse_repository("owner/repo/issues").is_err());
    }

    #[test]
    fn retrieves_repository_metadata_and_commit_from_mock_api() {
        let base = mock_server(vec![
            (200, json!({"name":"repo","html_url":"https://github.com/owner/repo","default_branch":"main"}).to_string()),
            (200, json!({"sha":"abc123"}).to_string()),
        ]);
        let client = GitHubClient::with_config(base, std::env::temp_dir()).unwrap();
        let repository = client.repository("owner/repo").unwrap();
        assert_eq!(repository.commit, "abc123");
        assert_eq!(repository.default_branch, "main");
        assert_eq!(repository.url, "https://github.com/owner/repo");
    }

    #[test]
    fn paginates_open_issues_and_excludes_pull_requests() {
        let page_one: Vec<_> = (1..=100).map(|number| {
            let mut record = json!({"number":number,"title":format!("Issue {number}"),"body":"","html_url":format!("https://github.com/o/r/issues/{number}"),"comments":0,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","labels":[{"name":"bug"}]});
            if number == 100 { record["pull_request"] = json!({"url":"https://api.github.com/pulls/100"}); }
            record
        }).collect();
        let page_two = vec![
            json!({"number":101,"title":"Last issue","body":"body text","html_url":"https://github.com/o/r/issues/101","comments":2,"created_at":"2026-01-02T00:00:00Z","updated_at":"2026-01-03T00:00:00Z","labels":[{"name":"good first issue"}]}),
        ];
        let base = mock_server(vec![
            (200, serde_json::to_string(&page_one).unwrap()),
            (200, serde_json::to_string(&page_two).unwrap()),
        ]);
        let client = GitHubClient::with_config(base, std::env::temp_dir()).unwrap();
        let repository = GitHubRepository {
            owner: "o".into(),
            name: "r".into(),
            default_branch: "main".into(),
            commit: "sha".into(),
            url: String::new(),
        };
        let issues = client.open_issues(&repository).unwrap();
        assert_eq!(issues.len(), 100);
        assert_eq!(issues.last().unwrap().number, 101);
        assert_eq!(issues[0].labels, vec!["bug"]);
        assert_eq!(issues.last().unwrap().body, "body text");
    }

    #[test]
    fn rejects_oversized_github_json_responses_before_deserialization() {
        let base = mock_close_delimited_server(
            200,
            vec![b' '; (MAX_GITHUB_JSON_RESPONSE_BYTES + 1) as usize],
        );
        let client = GitHubClient::with_config(base.clone(), std::env::temp_dir()).unwrap();
        assert!(
            client
                .json::<serde_json::Value>(&base)
                .unwrap_err()
                .contains("exceeds")
        );
    }

    #[test]
    fn issue_pagination_budgets_bound_aggregate_bytes_and_issue_count() {
        assert!(
            checked_issue_budgets(MAX_GITHUB_ISSUES_RESPONSE_BYTES - 10, 11, 0, 0)
                .unwrap_err()
                .contains("aggregate response limit")
        );
        assert!(
            checked_issue_budgets(0, 0, MAX_GITHUB_ISSUES, 1)
                .unwrap_err()
                .contains("10,000 issue limit")
        );
        assert_eq!(checked_issue_budgets(100, 200, 9_999, 1).unwrap(), 300);
    }

    #[test]
    fn caps_buffered_github_error_details() {
        let base = mock_close_delimited_server(
            500,
            vec![b'x'; (MAX_GITHUB_ERROR_RESPONSE_BYTES + 1) as usize],
        );
        let client = GitHubClient::with_config(base.clone(), std::env::temp_dir()).unwrap();
        let error = client.request(&base).unwrap_err();
        assert!(error.len() < 500);
        assert!(!error.contains("exceeds"));
    }

    #[test]
    fn reports_not_found_api_responses() {
        let base = mock_server(vec![(404, "{}".to_string())]);
        let client = GitHubClient::with_config(base, std::env::temp_dir()).unwrap();
        assert!(
            client
                .request(&format!("{}/missing", client.api_base))
                .unwrap_err()
                .contains("not found")
        );
    }

    #[test]
    fn reports_github_rate_limit_responses() {
        let base = mock_server(vec![(
            403,
            "{\"message\":\"API rate limit exceeded\"}".to_string(),
        )]);
        let client = GitHubClient::with_config(base.clone(), std::env::temp_dir()).unwrap();
        assert!(
            client
                .request(&format!("{base}/limited"))
                .unwrap_err()
                .contains("rate limit")
        );
    }

    #[test]
    fn caches_commit_pinned_source_archive() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut archive = tar::Builder::new(&mut encoder);
            let contents = b"pub fn main_entry() {}";
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive
                .append_data(&mut header, "repo-sha/src/main.rs", &contents[..])
                .unwrap();
            let mut link = tar::Header::new_gnu();
            link.set_entry_type(tar::EntryType::Symlink);
            link.set_link_name("../LICENSE-APACHE").unwrap();
            link.set_size(0);
            link.set_mode(0o777);
            link.set_cksum();
            archive
                .append_data(&mut link, "repo-sha/src/LICENSE", std::io::empty())
                .unwrap();
            archive.finish().unwrap();
        }
        let archive_bytes = encoder.finish().unwrap();
        let base = mock_byte_server(vec![(200, archive_bytes)]);
        let cache =
            std::env::temp_dir().join(format!("hecate_github_cache_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&cache);
        let client = GitHubClient::with_config(base, cache.clone()).unwrap();
        let repository = GitHubRepository {
            owner: "owner".into(),
            name: "repo".into(),
            default_branch: "main".into(),
            commit: "sha".into(),
            url: String::new(),
        };
        let first = client.source_checkout(&repository).unwrap();
        let second = client.source_checkout(&repository).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            fs::read_to_string(first.join("src/main.rs")).unwrap(),
            "pub fn main_entry() {}"
        );
        assert_eq!(
            fs::read_to_string(cache.join("repositories/owner-repo/sha/.hecate-skipped-links"))
                .unwrap(),
            "1"
        );
        fs::remove_dir_all(cache).unwrap();
    }
}
