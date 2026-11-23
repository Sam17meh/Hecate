//! Read-only repository tools rooted in a prepared HECATE workspace.
//!
//! Filesystem access is capability-scoped with `cap-std`; model arguments are parsed as relative
//! paths and can never name `.git` or HECATE's metadata. This is a read boundary, not a sandbox
//! for other processes or malicious repository behavior.

use crate::agent::provider::ToolDefinition;
use crate::agent::runtime::CancellationToken;
use crate::agent::session::{ContributionContext, PlanReviewStatus};
use crate::agent::tool::{
    Tool, ToolContext, ToolError, ToolErrorCode, ToolPermission, ToolRegistry, ToolResult,
};
use crate::analysis::symbols::analyze_rust_file;
use crate::workspace;
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_SEARCH_FILE_BYTES: u64 = 1024 * 1024;
const MAX_SYMBOL_FILE_BYTES: u64 = 256 * 1024;
const MAX_SCAN_BYTES: u64 = 24 * 1024 * 1024;
const MAX_SCAN_FILES: usize = 2_000;
const MAX_SCAN_ENTRIES: usize = 10_000;
const MAX_LIST_LIMIT: usize = 100;
const MAX_SEARCH_LIMIT: usize = 100;
const MAX_SYMBOL_RESULTS: usize = 100;
const MAX_DIFF_BYTES: usize = 256 * 1024;
const MAX_PATH_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepositoryOperation {
    ListFiles,
    ReadFile,
    SearchCode,
    InspectSymbol,
    FindReferences,
    ReadDiff,
}

impl RepositoryOperation {
    fn name(self) -> &'static str {
        match self {
            Self::ListFiles => "list_files",
            Self::ReadFile => "read_file",
            Self::SearchCode => "search_code",
            Self::InspectSymbol => "inspect_symbol",
            Self::FindReferences => "find_references",
            Self::ReadDiff => "read_diff",
        }
    }

    fn definition(self) -> ToolDefinition {
        let (description, properties, required) = match self {
            Self::ListFiles => (
                "List non-generated, non-sensitive workspace files with offset pagination.",
                json!({
                    "offset": {"type":"integer", "minimum":0, "maximum":10000},
                    "limit": {"type":"integer", "minimum":1, "maximum":100}
                }),
                vec![],
            ),
            Self::ReadFile => (
                "Read a bounded UTF-8 file by path relative to the approved workspace.",
                json!({"path":{"type":"string", "minLength":1, "maxLength":4096}}),
                vec!["path"],
            ),
            Self::SearchCode => (
                "Search bounded text files and return matching paths, line numbers, and snippets.",
                json!({
                    "query":{"type":"string", "minLength":1, "maxLength":256},
                    "case_sensitive":{"type":"boolean"},
                    "limit":{"type":"integer", "minimum":1, "maximum":100}
                }),
                vec!["query"],
            ),
            Self::InspectSymbol => (
                "Inspect Rust symbols using HECATE's tree-sitter symbol analyzer.",
                json!({
                    "symbol":{"type":"string", "minLength":1, "maxLength":256},
                    "file":{"type":"string", "maxLength":4096},
                    "limit":{"type":"integer", "minimum":1, "maximum":100}
                }),
                vec!["symbol"],
            ),
            Self::FindReferences => (
                "Find approximate identifier text matches and tree-sitter-verified Rust declarations.",
                json!({
                    "symbol":{"type":"string", "minLength":1, "maxLength":256},
                    "limit":{"type":"integer", "minimum":1, "maximum":100}
                }),
                vec!["symbol"],
            ),
            Self::ReadDiff => (
                "Read the bounded Git diff against HEAD using a fixed, non-extensible invocation.",
                json!({"max_bytes":{"type":"integer", "minimum":1, "maximum":262144}}),
                vec![],
            ),
        };
        ToolDefinition {
            name: self.name().into(),
            description: description.into(),
            input_schema: json!({
                "type":"object",
                "properties":properties,
                "required":required,
                "additionalProperties":false
            }),
        }
    }
}

/// A bundle of tools scoped to the checkout in a prepared HECATE workspace.
pub struct WorkspaceRepositoryTools;

impl WorkspaceRepositoryTools {
    /// Register all six read-only tools after validating the approved workspace and plan.
    /// `checkout_path` is the `repository/` directory returned by workspace preparation.
    pub fn register(
        checkout_path: &Path,
        context: &ContributionContext,
    ) -> Result<ToolRegistry, String> {
        if context.plan.review_status != PlanReviewStatus::Approved {
            return Err("Repository tools require a human-approved contribution plan".into());
        }
        if context.plan.schema_version != workspace::PLAN_SCHEMA_VERSION {
            return Err("Contribution plan reference has an unsupported schema version".into());
        }
        let checkout = workspace::validate_prepared_workspace(
            checkout_path,
            &context.repository_url,
            context.issue_number,
            &context.plan.artifact_id,
        )?;
        let mut registry = ToolRegistry::new();
        for operation in [
            RepositoryOperation::ListFiles,
            RepositoryOperation::ReadFile,
            RepositoryOperation::SearchCode,
            RepositoryOperation::InspectSymbol,
            RepositoryOperation::FindReferences,
            RepositoryOperation::ReadDiff,
        ] {
            registry
                .register(Box::new(WorkspaceRepositoryTool {
                    operation,
                    checkout: checkout.clone(),
                }))
                .map_err(|error| error.message)?;
        }
        Ok(registry)
    }
}

struct WorkspaceRepositoryTool {
    operation: RepositoryOperation,
    checkout: PathBuf,
}

impl WorkspaceRepositoryTool {
    fn root_dir(&self, context: &ContributionContext) -> Result<Dir, ToolError> {
        let validated = self.validate_context(context)?;
        if validated != self.checkout {
            return Err(tool_error(
                ToolErrorCode::PermissionDenied,
                "Session workspace does not match the registered checkout",
            ));
        }
        Dir::open_ambient_dir(&self.checkout, ambient_authority())
            .map_err(|error| tool_error(ToolErrorCode::ExecutionFailed, error.to_string()))
    }

    fn validate_context(&self, context: &ContributionContext) -> Result<PathBuf, ToolError> {
        if context.plan.review_status != PlanReviewStatus::Approved
            || context.plan.schema_version != workspace::PLAN_SCHEMA_VERSION
        {
            return Err(tool_error(
                ToolErrorCode::PermissionDenied,
                "Repository tools require an approved contribution plan with a supported schema",
            ));
        }
        workspace::validate_prepared_workspace(
            &self.checkout,
            &context.repository_url,
            context.issue_number,
            &context.plan.artifact_id,
        )
        .map_err(|message| tool_error(ToolErrorCode::PermissionDenied, message))
    }

    fn execute_operation(
        &self,
        arguments: &Value,
        context: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        if context.cancellation.is_cancelled() {
            return Err(tool_error(
                ToolErrorCode::ExecutionFailed,
                "Repository tool call was cancelled",
            ));
        }
        match self.operation {
            RepositoryOperation::ReadDiff => self.read_diff(arguments, context),
            operation => {
                let root = self.root_dir(&context.contribution)?;
                match operation {
                    RepositoryOperation::ListFiles => self.list_files(arguments, &root, context),
                    RepositoryOperation::ReadFile => self.read_file(arguments, &root),
                    RepositoryOperation::SearchCode => self.search_code(arguments, &root, context),
                    RepositoryOperation::InspectSymbol => {
                        self.inspect_symbol(arguments, &root, context)
                    }
                    RepositoryOperation::FindReferences => {
                        self.find_references(arguments, &root, context)
                    }
                    RepositoryOperation::ReadDiff => unreachable!(),
                }
            }
        }
    }

    fn list_files(
        &self,
        arguments: &Value,
        root: &Dir,
        context: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let offset = optional_usize(arguments, "offset", 0, 10_000)?;
        let limit = optional_usize(arguments, "limit", 50, MAX_LIST_LIMIT)?;
        let mut files = Vec::new();
        let mut stats = ScanStats::default();
        collect_files(
            root,
            Path::new(""),
            &mut files,
            &mut stats,
            Some(&context.cancellation),
            0,
        )?;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let total = files.len();
        let page: Vec<_> = files
            .iter()
            .skip(offset)
            .take(limit)
            .map(|file| json!({"path": shorten(&display_path(&file.path), 512), "size_bytes": file.size}))
            .collect();
        Ok(ToolResult::new(json!({
            "files": page,
            "offset": offset,
            "limit": limit,
            "total_files_scanned": total,
            "next_offset": if offset + page.len() < total { Some(offset + page.len()) } else { None::<usize> },
            "scan_truncated": stats.truncated
        })))
    }

    fn read_file(&self, arguments: &Value, root: &Dir) -> Result<ToolResult, ToolError> {
        let relative = required_string(arguments, "path")?;
        let path = safe_relative_path(relative)?;
        let metadata = root
            .symlink_metadata(&path)
            .map_err(|error| tool_error(ToolErrorCode::ExecutionFailed, error.to_string()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() || is_excluded(&path) {
            return Err(tool_error(
                ToolErrorCode::PermissionDenied,
                "Requested path is not an accessible regular workspace file",
            ));
        }
        if metadata.len() > MAX_FILE_BYTES {
            return Err(tool_error(
                ToolErrorCode::InvalidArguments,
                format!("File exceeds the {MAX_FILE_BYTES}-byte read limit"),
            ));
        }
        let file = root
            .open(&path)
            .map_err(|error| tool_error(ToolErrorCode::ExecutionFailed, error.to_string()))?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| tool_error(ToolErrorCode::ExecutionFailed, error.to_string()))?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(tool_error(
                ToolErrorCode::InvalidArguments,
                "File grew beyond the maximum read size",
            ));
        }
        let content = String::from_utf8(bytes)
            .map_err(|_| tool_error(ToolErrorCode::InvalidArguments, "File is not UTF-8 text"))?;
        let content = redact_sensitive_text(&content);
        Ok(ToolResult::new(json!({
            "path": display_path(&path),
            "size_bytes": metadata.len(),
            "line_count": content.lines().count(),
            "content": content,
            "truncated": false
        })))
    }

    fn search_code(
        &self,
        arguments: &Value,
        root: &Dir,
        context: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let query = required_string(arguments, "query")?;
        if query.chars().count() > 256 {
            return Err(invalid_arg("query cannot exceed 256 characters"));
        }
        let case_sensitive = arguments
            .get("case_sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let limit = optional_usize(arguments, "limit", 50, MAX_SEARCH_LIMIT)?;
        let (files, mut truncated) = scan_files(root, &context.cancellation)?;
        let query_normalized = if case_sensitive {
            query.to_string()
        } else {
            query.to_lowercase()
        };
        let mut matches = Vec::new();
        let mut bytes_scanned = 0_u64;
        'files: for file in files {
            if context.cancellation.is_cancelled() {
                return Err(cancelled_error());
            }
            if !is_text_candidate(&file.path) || file.size > MAX_SEARCH_FILE_BYTES {
                continue;
            }
            if bytes_scanned.saturating_add(file.size) > MAX_SCAN_BYTES {
                truncated = true;
                break;
            }
            let Some(content) = read_text_file(root, &file.path, MAX_SEARCH_FILE_BYTES)? else {
                continue;
            };
            bytes_scanned = bytes_scanned.saturating_add(content.len() as u64);
            for (index, line) in content.lines().enumerate() {
                let haystack = if case_sensitive {
                    line.to_string()
                } else {
                    line.to_lowercase()
                };
                if haystack.contains(&query_normalized) {
                    matches.push(json!({
                        "path": shorten(&display_path(&file.path), 512),
                        "line": index + 1,
                        "snippet": shorten(&redact_sensitive_text(line), 240)
                    }));
                    if matches.len() >= limit {
                        truncated = true;
                        break 'files;
                    }
                }
            }
        }
        Ok(ToolResult::new(json!({
            "query": query,
            "case_sensitive": case_sensitive,
            "matches": matches,
            "bytes_scanned": bytes_scanned,
            "truncated": truncated
        })))
    }

    fn inspect_symbol(
        &self,
        arguments: &Value,
        root: &Dir,
        context: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let name = required_string(arguments, "symbol")?;
        if name.chars().count() > 256 {
            return Err(invalid_arg("symbol cannot exceed 256 characters"));
        }
        let filter = arguments
            .get("file")
            .and_then(Value::as_str)
            .map(safe_relative_path)
            .transpose()?;
        let limit = optional_usize(arguments, "limit", 50, MAX_SYMBOL_RESULTS)?;
        let (files, mut truncated) = scan_files(root, &context.cancellation)?;
        let mut symbols = Vec::new();
        let mut bytes_scanned = 0_u64;
        let mut diagnostics = Vec::new();
        for file in files {
            if context.cancellation.is_cancelled() {
                return Err(cancelled_error());
            }
            if !file.path.to_string_lossy().ends_with(".rs")
                || filter.as_ref().is_some_and(|filter| filter != &file.path)
                || file.size > MAX_SYMBOL_FILE_BYTES
            {
                continue;
            }
            if bytes_scanned.saturating_add(file.size) > MAX_SCAN_BYTES {
                truncated = true;
                break;
            }
            let Some(source) = read_text_file(root, &file.path, MAX_SYMBOL_FILE_BYTES)? else {
                continue;
            };
            bytes_scanned = bytes_scanned.saturating_add(source.len() as u64);
            match analyze_rust_file(&source, &display_path(&file.path)) {
                Ok(found) => {
                    symbols.extend(found.into_iter().filter(|symbol| symbol.name == name));
                    if symbols.len() >= limit {
                        symbols.truncate(limit);
                        truncated = true;
                        break;
                    }
                }
                Err(message) => {
                    diagnostics.push(json!({"path":shorten(&display_path(&file.path), 512), "message": shorten(&message, 200)}))
                }
            }
        }
        Ok(ToolResult::new(json!({
            "symbol": name,
            "matches": symbols.iter().map(|symbol| json!({
                "file": symbol.file,
                "name": symbol.name,
                "kind": symbol.kind,
                "scope": symbol.scope.iter().take(32).map(|part| shorten(part, 128)).collect::<Vec<_>>(),
                "scope_truncated": symbol.scope.len() > 32,
                "start_line": symbol.start_line,
                "end_line": symbol.end_line,
                "visibility": symbol.visibility,
                "structurally_verified": true
            })).collect::<Vec<_>>(),
            "diagnostics": diagnostics,
            "bytes_scanned": bytes_scanned,
            "truncated": truncated
        })))
    }

    fn find_references(
        &self,
        arguments: &Value,
        root: &Dir,
        context: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let symbol = required_string(arguments, "symbol")?;
        if symbol.chars().count() > 256 || !is_identifier_like(symbol) {
            return Err(invalid_arg(
                "symbol must be an identifier of at most 256 characters",
            ));
        }
        let limit = optional_usize(arguments, "limit", 50, MAX_SEARCH_LIMIT)?;
        let (files, mut truncated) = scan_files(root, &context.cancellation)?;
        let mut declarations = Vec::new();
        let mut matches = Vec::new();
        let mut bytes_scanned = 0_u64;
        'files: for file in files {
            if context.cancellation.is_cancelled() {
                return Err(cancelled_error());
            }
            if !is_text_candidate(&file.path) || file.size > MAX_SEARCH_FILE_BYTES {
                continue;
            }
            if bytes_scanned.saturating_add(file.size) > MAX_SCAN_BYTES {
                truncated = true;
                break;
            }
            let Some(content) = read_text_file(root, &file.path, MAX_SEARCH_FILE_BYTES)? else {
                continue;
            };
            bytes_scanned = bytes_scanned.saturating_add(content.len() as u64);
            if file.path.to_string_lossy().ends_with(".rs") {
                if let Ok(found) = analyze_rust_file(&content, &display_path(&file.path)) {
                    declarations.extend(
                        found
                            .into_iter()
                            .filter(|entry| entry.name == symbol)
                            .map(|entry| {
                                json!({
                                "file":shorten(&entry.file, 512),
                                    "kind":entry.kind,
                                "scope":entry.scope.iter().take(32).map(|part| shorten(part, 128)).collect::<Vec<_>>(),
                                    "scope_truncated":entry.scope.len() > 32,
                                    "start_line":entry.start_line,
                                    "end_line":entry.end_line,
                                    "visibility":entry.visibility
                                })
                            }),
                    );
                    if declarations.len() > MAX_SYMBOL_RESULTS {
                        declarations.truncate(MAX_SYMBOL_RESULTS);
                        truncated = true;
                    }
                }
            }
            for (index, line) in content.lines().enumerate() {
                for (start, _) in line.match_indices(symbol) {
                    let before = line[..start].chars().next_back();
                    let after = line[start + symbol.len()..].chars().next();
                    if before.is_some_and(is_identifier_char)
                        || after.is_some_and(is_identifier_char)
                    {
                        continue;
                    }
                    matches.push(json!({
                        "path":shorten(&display_path(&file.path), 512),
                        "line":index + 1,
                        "snippet":shorten(&redact_sensitive_text(line), 240),
                        "match_type":"approximate_textual_identifier",
                        "structurally_verified":false
                    }));
                    if matches.len() >= limit {
                        truncated = true;
                        break 'files;
                    }
                }
            }
        }
        Ok(ToolResult::new(json!({
            "symbol":symbol,
            "structural_declarations":declarations,
            "textual_matches":matches,
            "limitations":"Declarations are tree-sitter parsed Rust symbols. Other occurrences are identifier-boundary text matches; they do not resolve Rust names, macros, generated code, or semantic references.",
            "bytes_scanned":bytes_scanned,
            "truncated":truncated
        })))
    }

    fn read_diff(&self, arguments: &Value, context: &ToolContext) -> Result<ToolResult, ToolError> {
        self.validate_context(&context.contribution)?;
        let git_metadata = std::fs::symlink_metadata(self.checkout.join(".git"))
            .map_err(|error| tool_error(ToolErrorCode::PermissionDenied, error.to_string()))?;
        if git_metadata.file_type().is_symlink() || !git_metadata.is_dir() {
            return Err(tool_error(
                ToolErrorCode::PermissionDenied,
                "Prepared workspace must contain its own regular .git directory",
            ));
        }
        let max_bytes = optional_usize(arguments, "max_bytes", 64 * 1024, MAX_DIFF_BYTES)?;
        let mut command = Command::new("git");
        command
            .env_clear()
            .envs([
                ("PATH", std::env::var_os("PATH").unwrap_or_default()),
                (
                    "SYSTEMROOT",
                    std::env::var_os("SYSTEMROOT").unwrap_or_default(),
                ),
                ("WINDIR", std::env::var_os("WINDIR").unwrap_or_default()),
                ("TEMP", std::env::var_os("TEMP").unwrap_or_default()),
                ("TMP", std::env::var_os("TMP").unwrap_or_default()),
            ])
            .arg("--no-pager")
            .args([
                "-c",
                "core.pager=cat",
                "-c",
                "diff.external=",
                "-c",
                "core.quotepath=false",
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--no-renames",
                "HEAD",
                "--",
            ])
            .current_dir(&self.checkout)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", git_null_device())
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command
            .spawn()
            .map_err(|error| tool_error(ToolErrorCode::ExecutionFailed, error.to_string()))?;
        let stdout = child.stdout.take().ok_or_else(|| {
            tool_error(
                ToolErrorCode::ExecutionFailed,
                "Git diff output unavailable",
            )
        })?;
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
            let result = stdout
                .take(max_bytes as u64 + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
                .map_err(|error| error.to_string());
            let _ = sender.send(result);
        });
        let started = Instant::now();
        let mut captured = None;
        let status = loop {
            if context.cancellation.is_cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(cancelled_error());
            }
            if started.elapsed() >= Duration::from_secs(15) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(tool_error(
                    ToolErrorCode::ExecutionFailed,
                    "Git diff exceeded the 15-second execution limit",
                ));
            }
            if captured.is_none() {
                match receiver.try_recv() {
                    Ok(Ok(bytes)) => captured = Some(bytes),
                    Ok(Err(message)) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(tool_error(ToolErrorCode::ExecutionFailed, message));
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(tool_error(
                            ToolErrorCode::ExecutionFailed,
                            "Git diff output reader stopped unexpectedly",
                        ));
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            if captured
                .as_ref()
                .is_some_and(|bytes: &Vec<u8>| bytes.len() > max_bytes)
            {
                let _ = child.kill();
            }
            let child_status = match child.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(tool_error(
                        ToolErrorCode::ExecutionFailed,
                        error.to_string(),
                    ));
                }
            };
            if let Some(status) = child_status.filter(|_| captured.is_some()) {
                break status;
            }
            thread::sleep(Duration::from_millis(10));
        };
        let mut bytes = captured.ok_or_else(|| {
            tool_error(
                ToolErrorCode::ExecutionFailed,
                "Git diff completed without captured output",
            )
        })?;
        let truncated = bytes.len() > max_bytes;
        if truncated {
            bytes.truncate(max_bytes);
        }
        if !truncated && !status.success() {
            return Err(tool_error(
                ToolErrorCode::ExecutionFailed,
                format!("Fixed Git diff command failed with {status}"),
            ));
        }
        let content = String::from_utf8_lossy(&bytes);
        Ok(ToolResult::new(json!({
            "base":"HEAD",
            "diff":redact_sensitive_text(&content),
            "bytes_returned":bytes.len(),
            "truncated":truncated,
            "untracked_files_included":false
        })))
    }
}

impl Tool for WorkspaceRepositoryTool {
    fn definition(&self) -> ToolDefinition {
        self.operation.definition()
    }

    fn permission(&self) -> ToolPermission {
        ToolPermission::ReadOnly
    }

    fn validate_arguments(&self, arguments: &Value) -> Result<(), ToolError> {
        let object = arguments
            .as_object()
            .ok_or_else(|| invalid_arg("arguments must be a JSON object"))?;
        let allowed: &[&str] = match self.operation {
            RepositoryOperation::ListFiles => &["offset", "limit"],
            RepositoryOperation::ReadFile => &["path"],
            RepositoryOperation::SearchCode => &["query", "case_sensitive", "limit"],
            RepositoryOperation::InspectSymbol => &["symbol", "file", "limit"],
            RepositoryOperation::FindReferences => &["symbol", "limit"],
            RepositoryOperation::ReadDiff => &["max_bytes"],
        };
        if object.keys().any(|key| !allowed.contains(&key.as_str())) {
            return Err(invalid_arg("arguments contain an unsupported field"));
        }
        match self.operation {
            RepositoryOperation::ListFiles => {
                optional_usize(arguments, "offset", 0, 10_000)?;
                optional_usize(arguments, "limit", 50, MAX_LIST_LIMIT)?;
            }
            RepositoryOperation::ReadFile => {
                safe_relative_path(required_string(arguments, "path")?)?;
            }
            RepositoryOperation::SearchCode => {
                let query = required_string(arguments, "query")?;
                if query.chars().count() > 256 {
                    return Err(invalid_arg("query cannot exceed 256 characters"));
                }
                if arguments
                    .get("case_sensitive")
                    .is_some_and(|value| !value.is_boolean())
                {
                    return Err(invalid_arg("case_sensitive must be a boolean"));
                }
                optional_usize(arguments, "limit", 50, MAX_SEARCH_LIMIT)?;
            }
            RepositoryOperation::InspectSymbol | RepositoryOperation::FindReferences => {
                let symbol = required_string(arguments, "symbol")?;
                if symbol.chars().count() > 256 {
                    return Err(invalid_arg("symbol cannot exceed 256 characters"));
                }
                if self.operation == RepositoryOperation::FindReferences
                    && !is_identifier_like(symbol)
                {
                    return Err(invalid_arg("symbol must be identifier-like"));
                }
                if let Some(file) = arguments.get("file") {
                    let file = file
                        .as_str()
                        .ok_or_else(|| invalid_arg("file must be a string"))?;
                    safe_relative_path(file)?;
                }
                optional_usize(arguments, "limit", 50, MAX_SYMBOL_RESULTS)?;
            }
            RepositoryOperation::ReadDiff => {
                optional_usize(arguments, "max_bytes", 64 * 1024, MAX_DIFF_BYTES)?;
            }
        }
        Ok(())
    }

    fn execute(
        &mut self,
        arguments: Value,
        context: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        self.validate_arguments(&arguments)?;
        self.execute_operation(&arguments, context)
    }
}

#[derive(Debug, Clone)]
struct ListedFile {
    path: PathBuf,
    size: u64,
}

#[derive(Default)]
struct ScanStats {
    files: usize,
    entries: usize,
    truncated: bool,
}

fn collect_files(
    root: &Dir,
    directory: &Path,
    output: &mut Vec<ListedFile>,
    stats: &mut ScanStats,
    cancellation: Option<&CancellationToken>,
    depth: usize,
) -> Result<(), ToolError> {
    if stats.truncated {
        return Ok(());
    }
    if depth > 64 {
        stats.truncated = true;
        return Ok(());
    }
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(cancelled_error());
    }
    let read_path = if directory.as_os_str().is_empty() {
        Path::new(".")
    } else {
        directory
    };
    let entries = root
        .read_dir(read_path)
        .map_err(|error| tool_error(ToolErrorCode::ExecutionFailed, error.to_string()))?;
    for entry in entries {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(cancelled_error());
        }
        let entry =
            entry.map_err(|error| tool_error(ToolErrorCode::ExecutionFailed, error.to_string()))?;
        stats.entries += 1;
        if stats.entries > MAX_SCAN_ENTRIES {
            stats.truncated = true;
            return Ok(());
        }
        let name = entry.file_name();
        let relative = directory.join(name);
        if is_excluded(&relative) {
            continue;
        }
        let kind = entry
            .file_type()
            .map_err(|error| tool_error(ToolErrorCode::ExecutionFailed, error.to_string()))?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            collect_files(root, &relative, output, stats, cancellation, depth + 1)?;
        } else if kind.is_file() {
            stats.files += 1;
            if stats.files > MAX_SCAN_FILES {
                stats.truncated = true;
                return Ok(());
            }
            let metadata = root
                .symlink_metadata(&relative)
                .map_err(|error| tool_error(ToolErrorCode::ExecutionFailed, error.to_string()))?;
            output.push(ListedFile {
                path: relative,
                size: metadata.len(),
            });
        }
    }
    Ok(())
}

fn scan_files(
    root: &Dir,
    cancellation: &CancellationToken,
) -> Result<(Vec<ListedFile>, bool), ToolError> {
    let mut files = Vec::new();
    let mut stats = ScanStats::default();
    collect_files(
        root,
        Path::new(""),
        &mut files,
        &mut stats,
        Some(cancellation),
        0,
    )?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((files, stats.truncated))
}

fn read_text_file(root: &Dir, path: &Path, max_bytes: u64) -> Result<Option<String>, ToolError> {
    let metadata = root
        .symlink_metadata(path)
        .map_err(|error| tool_error(ToolErrorCode::ExecutionFailed, error.to_string()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > max_bytes {
        return Ok(None);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    root.open(path)
        .and_then(|file| file.take(max_bytes + 1).read_to_end(&mut bytes))
        .map_err(|error| tool_error(ToolErrorCode::ExecutionFailed, error.to_string()))?;
    if bytes.len() as u64 > max_bytes {
        return Ok(None);
    }
    Ok(String::from_utf8(bytes).ok())
}

fn safe_relative_path(value: &str) -> Result<PathBuf, ToolError> {
    if value.is_empty() || value.len() > MAX_PATH_BYTES {
        return Err(invalid_arg("path must be nonempty and at most 4096 bytes"));
    }
    let mut normalized = PathBuf::new();
    let portable = value.replace('\\', "/");
    if portable.starts_with('/') || portable.contains(':') {
        return Err(tool_error(
            ToolErrorCode::PermissionDenied,
            "Absolute paths and platform-qualified paths are denied",
        ));
    }
    for component in portable.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(tool_error(
                ToolErrorCode::PermissionDenied,
                "Empty path components, `.` and parent traversal are denied",
            ));
        }
        normalized.push(component);
    }
    if normalized.as_os_str().is_empty() || is_excluded(&normalized) {
        return Err(tool_error(
            ToolErrorCode::PermissionDenied,
            "Requested path is outside the readable workspace file set",
        ));
    }
    Ok(normalized)
}

fn is_excluded(path: &Path) -> bool {
    path.components().any(|component| {
        let Component::Normal(component) = component else {
            return true;
        };
        let name = component.to_string_lossy().to_ascii_lowercase();
        name == ".git"
            || name == ".hecate"
            || matches!(
                name.as_str(),
                "target"
                    | "node_modules"
                    | "dist"
                    | "build"
                    | "vendor"
                    | ".cache"
                    | "coverage"
                    | "out"
                    | ".next"
                    | ".venv"
                    | "venv"
                    | "__pycache__"
                    | ".tox"
                    | ".gradle"
                    | "pods"
                    | ".turbo"
                    | ".idea"
            )
            || name.starts_with(".env")
            || name.starts_with(".git-credentials")
            || matches!(
                name.as_str(),
                "credentials"
                    | "secrets"
                    | "id_rsa"
                    | "id_ed25519"
                    | "known_hosts"
                    | ".npmrc"
                    | ".pypirc"
                    | ".netrc"
                    | ".aws"
                    | ".ssh"
                    | "service-account.json"
            )
            || name.starts_with("credentials.")
            || name.starts_with("secrets.")
            || name.ends_with(".pem")
            || name.ends_with(".key")
            || name.ends_with(".p12")
            || name.ends_with(".pfx")
            || name.ends_with(".keystore")
    })
}

fn is_text_candidate(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some(
            "rs" | "toml"
                | "md"
                | "txt"
                | "json"
                | "yaml"
                | "yml"
                | "js"
                | "jsx"
                | "ts"
                | "tsx"
                | "py"
                | "go"
                | "java"
                | "c"
                | "h"
                | "cpp"
                | "hpp"
                | "cs"
                | "sh"
                | "html"
                | "css"
                | "xml"
                | "sql"
                | "lock"
        )
    ) || path.file_name().is_some_and(|name| {
        matches!(
            name.to_string_lossy().as_ref(),
            "Makefile" | "Dockerfile" | ".gitignore" | ".editorconfig"
        )
    })
}

fn required_string<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, ToolError> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| invalid_arg(format!("{name} must be a nonempty string")))
}

fn optional_usize(
    arguments: &Value,
    name: &str,
    default: usize,
    maximum: usize,
) -> Result<usize, ToolError> {
    let Some(value) = arguments.get(name) else {
        return Ok(default);
    };
    let value = value
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| invalid_arg(format!("{name} must be a nonnegative integer")))?;
    if value == 0 && name != "offset" || value > maximum {
        return Err(invalid_arg(format!(
            "{name} must be at most {maximum} and nonzero when applicable"
        )));
    }
    Ok(value)
}

fn is_identifier_like(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_alphabetic())
        && chars.all(is_identifier_char)
}

fn is_identifier_char(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(windows)]
fn git_null_device() -> &'static str {
    "NUL"
}

#[cfg(not(windows))]
fn git_null_device() -> &'static str {
    "/dev/null"
}

fn shorten(value: &str, max_chars: usize) -> String {
    let value: String = value.chars().take(max_chars).collect();
    value
}

fn redact_sensitive_text(value: &str) -> String {
    let mut private_key = false;
    let mut output = String::with_capacity(value.len());
    for segment in value.split_inclusive('\n') {
        let (line, ending) = if let Some(line) = segment.strip_suffix('\n') {
            if let Some(line) = line.strip_suffix('\r') {
                (line, "\r\n")
            } else {
                (line, "\n")
            }
        } else {
            (segment, "")
        };
        let lower = line.to_ascii_lowercase();
        if lower.contains("-----begin ") && lower.contains("private key-----") {
            private_key = true;
        }
        let sensitive_assignment = [
            "token",
            "password",
            "secret",
            "api_key",
            "apikey",
            "private_key",
            "credential",
            "access_key",
            "authorization",
            "bearer ",
            "api-key",
            "client_secret",
            "client-secret",
            "access-token",
        ]
        .iter()
        .any(|needle| lower.contains(needle) && (line.contains('=') || line.contains(':')));
        let known_token = [
            "ghp_",
            "gho_",
            "ghu_",
            "ghs_",
            "ghr_",
            "github_pat_",
            "akia",
            "xoxb-",
            "xoxp-",
        ]
        .iter()
        .any(|needle| lower.contains(needle));
        if private_key || sensitive_assignment || known_token {
            output.push_str("[REDACTED SENSITIVE LINE]");
        } else {
            output.push_str(line);
        }
        output.push_str(ending);
        if lower.contains("-----end ") && lower.contains("private key-----") {
            private_key = false;
        }
    }
    output
}

fn tool_error(code: ToolErrorCode, message: impl Into<String>) -> ToolError {
    ToolError::new(code, message)
}

fn invalid_arg(message: impl Into<String>) -> ToolError {
    tool_error(ToolErrorCode::InvalidArguments, message)
}

fn cancelled_error() -> ToolError {
    tool_error(
        ToolErrorCode::ExecutionFailed,
        "Repository tool call was cancelled",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::provider::{MockProvider, ModelResponse, ToolCallRequest};
    use crate::agent::runtime::{AgentRuntime, ExecutionLimits};
    use crate::agent::session::{AgentSession, ContributionPlanReference, PlanReviewStatus};
    use crate::planning::ContributionPlan;
    use crate::workspace::{create_workspace_from_source_for_tests, preview_workspace_for_tests};
    use serde_json::json;
    use std::fs;
    use std::process::Output;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct DenyPolicy;

    impl crate::agent::tool::PermissionPolicy for DenyPolicy {
        fn authorize(
            &self,
            _permission: ToolPermission,
            _context: &ContributionContext,
        ) -> Result<(), ToolError> {
            Err(ToolError::new(
                ToolErrorCode::PermissionDenied,
                "denied by test policy",
            ))
        }
    }

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        root: PathBuf,
        checkout: PathBuf,
        context: ContributionContext,
    }

    impl Fixture {
        fn new() -> Self {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let base =
                std::env::temp_dir().join(format!("hecate-repo-tools-{}-{id}", std::process::id()));
            let source = base.join("source");
            fs::create_dir_all(source.join("src")).unwrap();
            let git = |args: &[&str]| -> Output {
                Command::new("git")
                    .args(args)
                    .current_dir(&source)
                    .output()
                    .unwrap()
            };
            assert!(
                Command::new("git")
                    .args(["init", "-b", "main"])
                    .current_dir(&source)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
            assert!(
                git(&["config", "user.name", "Hecate Test"])
                    .status
                    .success()
            );
            assert!(
                git(&["config", "user.email", "hecate@example.invalid"])
                    .status
                    .success()
            );
            fs::write(
                source.join("src/lib.rs"),
                "pub fn refresh_cache() {\n    let result = 1;\n}\n",
            )
            .unwrap();
            fs::write(source.join("README.md"), "Repository documentation.\n").unwrap();
            fs::write(source.join(".env"), "API_TOKEN=ghp_not-a-real-token\n").unwrap();
            fs::create_dir_all(source.join("target/debug")).unwrap();
            fs::write(source.join("target/debug/generated.rs"), "generated").unwrap();
            fs::create_dir_all(source.join(".git/secrets")).unwrap();
            fs::write(source.join(".git/secrets/config"), "must not read").unwrap();
            assert!(git(&["add", "."]).status.success());
            assert!(git(&["commit", "-m", "initial"]).status.success());
            let commit = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
                .unwrap()
                .trim()
                .to_owned();
            let repository = crate::github::GitHubRepository {
                owner: "example".into(),
                name: "repo".into(),
                default_branch: "main".into(),
                commit,
                url: "https://github.com/example/repo".into(),
            };
            let plan = ContributionPlan {
                repository_url: repository.url.clone(),
                issue_number: 7,
                issue_title: "Improve cache refresh".into(),
                issue_url: "https://github.com/example/repo/issues/7".into(),
                labels: vec!["bug".into()],
                created_at: "today".into(),
                updated_at: "today".into(),
                comments: 0,
                expected_outcome: "A reliable refresh".into(),
                interpretation: "Investigate cache behavior".into(),
                locations: vec![],
                documents: vec![],
                steps: vec!["Inspect".into()],
                test_recommendations: vec!["Add tests".into()],
                validation_commands: vec!["cargo test".into()],
                risks_and_assumptions: vec!["Heuristic".into()],
                unresolved_questions: vec![],
            };
            let workspace_root = base.join("workspaces");
            let preview = preview_workspace_for_tests(&repository, 7, &workspace_root).unwrap();
            let workspace = create_workspace_from_source_for_tests(
                &repository,
                &plan,
                &preview,
                source.to_str().unwrap(),
            )
            .unwrap();
            Self {
                root: base,
                checkout: workspace.checkout_path,
                context: ContributionContext {
                    repository_url: repository.url,
                    issue_number: 7,
                    issue_title: plan.issue_title,
                    plan: ContributionPlanReference {
                        artifact_id: ".hecate/plan-v1.json".into(),
                        repository_url: "https://github.com/example/repo".into(),
                        issue_number: 7,
                        schema_version: crate::workspace::PLAN_SCHEMA_VERSION,
                        review_status: PlanReviewStatus::Approved,
                    },
                },
            }
        }
        fn cleanup(self) {
            let _ = fs::remove_dir_all(self.root);
        }
    }

    fn call(name: &str, arguments: Value) -> ToolCallRequest {
        ToolCallRequest {
            id: "call-1".into(),
            name: name.into(),
            arguments,
        }
    }

    fn invoke(fixture: &Fixture, name: &str, arguments: Value) -> Result<ToolResult, ToolError> {
        let mut registry =
            WorkspaceRepositoryTools::register(&fixture.checkout, &fixture.context).unwrap();
        let context = ToolContext {
            contribution: fixture.context.clone(),
            cancellation: CancellationToken::default(),
        };
        registry.execute(
            &call(name, arguments),
            &crate::agent::tool::ReadOnlyPermissionPolicy,
            &context,
        )
    }

    fn run_agent(fixture: &Fixture, name: &str, arguments: Value) -> AgentSession {
        let registry =
            WorkspaceRepositoryTools::register(&fixture.checkout, &fixture.context).unwrap();
        let provider = MockProvider::from_responses([
            ModelResponse::with_tool_calls(vec![call(name, arguments)]),
            ModelResponse::text("Finished repository inspection."),
        ]);
        let mut runtime = AgentRuntime::new(provider, registry, ExecutionLimits::default());
        let mut session = AgentSession::new(
            "repo-tool-test",
            fixture.context.clone(),
            "Inspect the approved issue",
        )
        .unwrap();
        runtime
            .run(&mut session, &CancellationToken::default())
            .unwrap();
        session
    }

    #[test]
    fn registers_six_tools_and_lists_filtered_files_with_pagination() {
        let fixture = Fixture::new();
        let registry =
            WorkspaceRepositoryTools::register(&fixture.checkout, &fixture.context).unwrap();
        assert_eq!(registry.definitions().len(), 6);
        let result = invoke(&fixture, "list_files", json!({"offset":0,"limit":2})).unwrap();
        let files = result.output["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0]["path"], "README.md");
        let listed = serde_json::to_string(&result.output).unwrap();
        assert!(!listed.contains("target"));
        assert!(!listed.contains(".git"));
        assert!(!listed.contains(".env"));
        assert_eq!(
            invoke(&fixture, "read_file", json!({"path":".env"}))
                .unwrap_err()
                .code,
            ToolErrorCode::PermissionDenied
        );
        fixture.cleanup();
    }

    #[test]
    fn reads_bounded_file_and_redacts_secret_assignments() {
        let fixture = Fixture::new();
        fs::write(
            fixture.checkout.join("secret.txt"),
            "password=hidden\nordinary text\n",
        )
        .unwrap();
        let result = invoke(&fixture, "read_file", json!({"path":"secret.txt"})).unwrap();
        assert!(
            result.output["content"]
                .as_str()
                .unwrap()
                .contains("[REDACTED SENSITIVE LINE]")
        );
        assert!(
            !result.output["content"]
                .as_str()
                .unwrap()
                .contains("hidden")
        );
        assert!(
            result.output["content"]
                .as_str()
                .unwrap()
                .ends_with("ordinary text\n")
        );
        fs::write(
            fixture.checkout.join("large.txt"),
            vec![b'x'; (MAX_FILE_BYTES + 1) as usize],
        )
        .unwrap();
        assert_eq!(
            invoke(&fixture, "read_file", json!({"path":"large.txt"}))
                .unwrap_err()
                .code,
            ToolErrorCode::InvalidArguments
        );
        fixture.cleanup();
    }

    #[test]
    fn rejects_traversal_absolute_and_metadata_paths() {
        let fixture = Fixture::new();
        for path in [
            "../source/src/lib.rs",
            "src/../../README.md",
            "C:\\Users\\milan\\secret.txt",
            "..\\source\\README.md",
            ".git/config",
            ".hecate/plan-v1.json",
        ] {
            assert_eq!(
                invoke(&fixture, "read_file", json!({"path":path}))
                    .unwrap_err()
                    .code,
                ToolErrorCode::PermissionDenied
            );
        }
        assert_eq!(
            invoke(&fixture, "read_file", json!({"path":"missing.txt"}))
                .unwrap_err()
                .code,
            ToolErrorCode::ExecutionFailed
        );
        fixture.cleanup();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape_from_workspace() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let outside = fixture.root.join("outside.txt");
        fs::write(&outside, "outside secret").unwrap();
        symlink(&outside, fixture.checkout.join("escape.txt")).unwrap();
        assert_eq!(
            invoke(&fixture, "read_file", json!({"path":"escape.txt"}))
                .unwrap_err()
                .code,
            ToolErrorCode::PermissionDenied
        );
        let result = invoke(&fixture, "list_files", json!({})).unwrap();
        assert!(!result.output.to_string().contains("escape.txt"));
        let outside_dir = fixture.root.join("outside-dir");
        fs::create_dir_all(&outside_dir).unwrap();
        fs::write(outside_dir.join("private.txt"), "outside directory secret").unwrap();
        symlink(&outside_dir, fixture.checkout.join("linked-dir")).unwrap();
        assert!(
            invoke(
                &fixture,
                "read_file",
                json!({"path":"linked-dir/private.txt"})
            )
            .is_err()
        );
        fixture.cleanup();
    }

    #[cfg(windows)]
    #[test]
    fn rejects_symlink_escape_from_workspace() {
        let fixture = Fixture::new();
        let outside = fixture.root.join("outside.txt");
        fs::write(&outside, "outside secret").unwrap();
        if std::os::windows::fs::symlink_file(&outside, fixture.checkout.join("escape.txt")).is_ok()
        {
            assert_eq!(
                invoke(&fixture, "read_file", json!({"path":"escape.txt"}))
                    .unwrap_err()
                    .code,
                ToolErrorCode::PermissionDenied
            );
        }
        fixture.cleanup();
    }

    #[test]
    fn searches_with_line_numbers_and_respects_case_mode() {
        let fixture = Fixture::new();
        let result = invoke(
            &fixture,
            "search_code",
            json!({"query":"REFRESH_CACHE","case_sensitive":false}),
        )
        .unwrap();
        assert_eq!(result.output["matches"][0]["line"], 1);
        let strict = invoke(
            &fixture,
            "search_code",
            json!({"query":"REFRESH_CACHE","case_sensitive":true}),
        )
        .unwrap();
        assert_eq!(strict.output["matches"].as_array().unwrap().len(), 0);
        fixture.cleanup();
    }

    #[test]
    fn inspects_symbols_and_reports_approximate_reference_matches_separately() {
        let fixture = Fixture::new();
        let symbols = invoke(
            &fixture,
            "inspect_symbol",
            json!({"symbol":"refresh_cache"}),
        )
        .unwrap();
        let symbol = &symbols.output["matches"][0];
        assert_eq!(symbol["kind"], "function");
        assert_eq!(symbol["file"], "src/lib.rs");
        assert_eq!(symbol["start_line"], 1);
        assert_eq!(symbol["visibility"], "pub");
        assert_eq!(symbol["structurally_verified"], true);
        let references = invoke(
            &fixture,
            "find_references",
            json!({"symbol":"refresh_cache"}),
        )
        .unwrap();
        assert!(
            !references.output["structural_declarations"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(references.output["textual_matches"][0]["structurally_verified"] == false);
        assert!(
            references.output["limitations"]
                .as_str()
                .unwrap()
                .contains("do not resolve Rust names")
        );
        fixture.cleanup();
    }

    #[test]
    fn reads_fixed_bounded_git_diff_and_integrates_with_agent_runtime() {
        let fixture = Fixture::new();
        fs::write(
            fixture.checkout.join("src/lib.rs"),
            "pub fn refresh_cache() { let changed = true; }\n",
        )
        .unwrap();
        fs::write(
            fixture.checkout.join("untracked.txt"),
            "untracked secret=hidden\n",
        )
        .unwrap();
        let diff = invoke(&fixture, "read_diff", json!({"max_bytes":5000})).unwrap();
        assert!(diff.output["diff"].as_str().unwrap().contains("changed"));
        assert!(
            !diff.output["diff"]
                .as_str()
                .unwrap()
                .contains("untracked.txt")
        );
        assert_eq!(diff.output["untracked_files_included"], false);
        let session = run_agent(&fixture, "read_file", json!({"path":"README.md"}));
        assert_eq!(
            session.status,
            crate::agent::session::AgentStatus::Completed
        );
        assert_eq!(session.tool_calls.len(), 1);
        fixture.cleanup();
    }

    #[test]
    fn rejects_invalid_arguments_permission_and_workspace_context() {
        let fixture = Fixture::new();
        let mut pending = fixture.context.clone();
        pending.plan.review_status = PlanReviewStatus::PendingHumanReview;
        assert!(WorkspaceRepositoryTools::register(&fixture.checkout, &pending).is_err());
        assert_eq!(
            invoke(&fixture, "list_files", json!({"limit":101}))
                .unwrap_err()
                .code,
            ToolErrorCode::InvalidArguments
        );
        let mut registry =
            WorkspaceRepositoryTools::register(&fixture.checkout, &fixture.context).unwrap();
        let mut pending_context = fixture.context.clone();
        pending_context.plan.review_status = PlanReviewStatus::PendingHumanReview;
        assert_eq!(
            registry
                .execute(
                    &call("read_file", json!({"path":"README.md"})),
                    &crate::agent::tool::ReadOnlyPermissionPolicy,
                    &ToolContext {
                        contribution: pending_context,
                        cancellation: CancellationToken::default(),
                    },
                )
                .unwrap_err()
                .code,
            ToolErrorCode::PermissionDenied
        );
        let denied = registry.execute(
            &call("read_file", json!({"path":"README.md"})),
            &DenyPolicy,
            &ToolContext {
                contribution: fixture.context.clone(),
                cancellation: CancellationToken::default(),
            },
        );
        assert_eq!(denied.unwrap_err().code, ToolErrorCode::PermissionDenied);
        let mut context = fixture.context.clone();
        context.issue_number = 8;
        let error = registry
            .execute(
                &call("read_file", json!({"path":"README.md"})),
                &crate::agent::tool::ReadOnlyPermissionPolicy,
                &ToolContext {
                    contribution: context,
                    cancellation: CancellationToken::default(),
                },
            )
            .unwrap_err();
        assert_eq!(error.code, ToolErrorCode::PermissionDenied);
        fixture.cleanup();
    }

    #[test]
    fn cancellation_and_runtime_tool_budget_remain_enforced() {
        let fixture = Fixture::new();
        let registry =
            WorkspaceRepositoryTools::register(&fixture.checkout, &fixture.context).unwrap();
        let provider = MockProvider::from_responses([ModelResponse::with_tool_calls(vec![call(
            "list_files",
            json!({}),
        )])]);
        let mut runtime = AgentRuntime::new(
            provider,
            registry,
            ExecutionLimits {
                max_iterations: 1,
                max_tool_calls: 0,
            },
        );
        let mut session = AgentSession::new("limited", fixture.context.clone(), "list").unwrap();
        runtime
            .run(&mut session, &CancellationToken::default())
            .unwrap();
        assert_eq!(
            session.status,
            crate::agent::session::AgentStatus::LimitReached
        );
        assert_eq!(session.usage.tool_calls, 0);

        let registry =
            WorkspaceRepositoryTools::register(&fixture.checkout, &fixture.context).unwrap();
        let provider = MockProvider::from_responses([ModelResponse::with_tool_calls(vec![call(
            "list_files",
            json!({}),
        )])]);
        let mut runtime = AgentRuntime::new(provider, registry, ExecutionLimits::default());
        let mut session = AgentSession::new("cancelled", fixture.context.clone(), "list").unwrap();
        let token = CancellationToken::default();
        token.cancel();
        runtime.run(&mut session, &token).unwrap();
        assert_eq!(
            session.status,
            crate::agent::session::AgentStatus::Cancelled
        );
        fixture.cleanup();
    }
}
