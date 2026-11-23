# Model providers

## Current abstraction

Provider types live in `src/agent/provider.rs`. `ModelProvider::complete(&mut self, ModelRequest) -> Result<ModelResponse, ProviderError>` is the boundary between the HECATE-owned runtime and any reasoning service.

- `ModelRequest`: typed conversation messages and registered `ToolDefinition`s.
- `ModelResponse`: optional assistant text, typed tool-call requests, and optional token usage.
- `ProviderError`: typed failure category and message.
- `MockProvider`: deterministic scripted responses that records requests for assertions. It is used in tests and examples; it does not contact an external model.
- `OpenAiCompatibleProvider`: synchronous HTTP adapter for services implementing the OpenAI-compatible Chat Completions protocol. It does not execute tools; responses return to the HECATE runtime for validation and dispatch.

The runtime, session, tool registry, permissions, and dispatch belong to HECATE. A provider returns a request for a named registered tool; it does not execute tools itself and must not create an alternate dispatch path.

## Current status

The provider is configured through environment variables and is used by `contribute --generate`; there is no general-purpose agent CLI command. Streaming and MCP are not implemented. The provider method is synchronous; runtime cancellation cannot interrupt a request already in progress, so the HTTP adapter applies a bounded request timeout. There is no separate provider data-disclosure consent prompt in the CLI.

In `contribute --generate`, contribution-plan context, registered tool definitions, tool-call arguments, and repository excerpts/results are sent to the configured model provider. Users must configure only a provider they trust with that repository data. The workspace-creation and patch-approval prompts are not a separate provider disclosure/consent step.

## Configuration and transport behavior

Required environment variables:

- `HECATE_API_KEY`: bearer credential.
- `HECATE_BASE_URL`: service base URL; `/chat/completions` is appended. HTTPS is required except for loopback HTTP endpoints.
- `HECATE_MODEL`: model identifier sent in the request body.

Optional limits:

- `HECATE_PROVIDER_TIMEOUT_SECONDS`: request time budget, default 30 seconds, maximum 120 seconds.
- `HECATE_MAX_PROVIDER_REQUEST_BYTES`: maximum request size, default 4 MiB, maximum 16 MiB.
- `HECATE_MAX_PROVIDER_RESPONSE_BYTES`: response body limit, default 1 MiB, maximum 16 MiB.

The adapter sends typed conversation messages and JSON tool definitions. Assistant tool calls preserve their IDs, and tool results are sent with the associated `tool_call_id`. Multiple tool calls are decoded as separate HECATE requests; only the runtime's registry, validators, policies, and budgets can dispatch them. Token usage is mapped when supplied by the service.

The request-size limit is separate from the HTTP timeout. HECATE first counts the cumulative serialized conversation and tool definitions without building the request body, then serializes the provider-specific body through a writer capped at the same limit. The timeout covers the HTTP request and response after this local request construction; it is not a deadline for local serialization work.

Authentication, rate-limit, server, timeout, network, configuration, and malformed-response failures use typed categories with generic messages. A 429 or 5xx response receives at most one retry within the configured total time budget; other failures are not retried. Redirects are disabled so credentials are sent only to the configured endpoint. Response bytes are bounded before JSON parsing. Response body details and credentials are not included in errors.

The provider key is held only by provider configuration/runtime memory and is not exposed through a getter, `Debug`, or serialization. The runtime uses the provider sanitization hook to remove exact key matches from session content, tool results, errors, and model responses before they are retained. The provider also sanitizes outgoing message content. This exact-match protection is a defense against accidental exposure, not a substitute for secret management or complete detection of every repository secret; heuristic repository redaction remains in place as another layer. See [SECURITY.md](SECURITY.md).

## Requirements for additional providers

Any provider adapter should:

1. Implement only `ModelProvider` and keep vendor request/response types inside its adapter module.
2. Map ordinary text, tool names/arguments, token usage, and provider failures into the existing typed contract; reject malformed or unsupported tool-call responses.
3. Use explicit configuration and never hardcode credentials. Avoid logging prompts, tokens, credentials, or sensitive repository contents.
4. Bound request/response sizes and latency, honor cancellation where its transport permits, and avoid unbounded automatic retries.
5. Preserve provider independence: do not put provider-specific branching in `AgentRuntime`, `ToolRegistry`, or repository analyzers.
6. Test mapping, malformed responses, errors, limits, and tool-call round trips with mocked HTTP/transport fixtures. Unit tests must not need live credentials or internet access.
7. Keep approval and permission decisions in HECATE. Provider output is untrusted input; the runtime must still enforce tool registration, permission, argument validation, budgets, and workspace/plan checks.

Adding a provider does not authorize write tools, shell execution, repository-code execution, automatic commits, pushes, or pull requests. Those require separate reviewed milestones.
