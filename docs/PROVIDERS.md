# Model providers

## Current abstraction

Provider types live in `src/agent/provider.rs`. `ModelProvider::complete(&mut self, ModelRequest) -> Result<ModelResponse, ProviderError>` is the boundary between the HECATE-owned runtime and any reasoning service.

- `ModelRequest`: typed conversation messages and registered `ToolDefinition`s.
- `ModelResponse`: optional assistant text, typed tool-call requests, and optional token usage.
- `ProviderError`: typed failure category and message.
- `MockProvider`: deterministic scripted responses that records requests for assertions. It is used in tests and examples; it does not contact an external model.

The runtime, session, tool registry, permissions, and dispatch belong to HECATE. A provider returns a request for a named registered tool; it does not execute tools itself and must not create an alternate dispatch path.

## Current status

There is no real provider, API credential/configuration flow, model selection, retry policy, streaming support, or provider-specific CLI command. There is no MCP integration. The provider method is synchronous; the current runtime cannot interrupt a provider request while it is in progress.

## Requirements for a future provider

Any provider adapter should:

1. Implement only `ModelProvider` and keep vendor request/response types inside its adapter module.
2. Map ordinary text, tool names/arguments, token usage, and provider failures into the existing typed contract; reject malformed or unsupported tool-call responses.
3. Use explicit configuration and never hardcode credentials. Avoid logging prompts, tokens, credentials, or sensitive repository contents.
4. Bound request/response sizes and latency, honor cancellation where its transport permits, and avoid unbounded automatic retries.
5. Preserve provider independence: do not put provider-specific branching in `AgentRuntime`, `ToolRegistry`, or repository analyzers.
6. Test mapping, malformed responses, errors, limits, and tool-call round trips with mocked HTTP/transport fixtures. Unit tests must not need live credentials or internet access.
7. Keep approval and permission decisions in HECATE. Provider output is untrusted input; the runtime must still enforce tool registration, permission, argument validation, budgets, and workspace/plan checks.

Adding a provider does not authorize write tools, shell execution, repository-code execution, automatic commits, pushes, or pull requests. Those require separate reviewed milestones.
