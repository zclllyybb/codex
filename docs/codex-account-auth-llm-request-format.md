# Codex Account Auth LLM Request Format

This note describes the request Codex sends for normal LLM turns when it is using an
authenticated OpenAI/Codex account. It is based on the local Rust code under
`codex-rs`.

## Scope

The normal LLM turn uses the Responses API payload defined by
`ResponsesApiRequest` in `codex-rs/codex-api/src/common.rs` and assembled by
`ModelClient::build_responses_request` in `codex-rs/core/src/client.rs`.

Default endpoint depends on auth mode:

- API key auth: `POST https://api.openai.com/v1/responses`
- ChatGPT/Codex account auth: `POST https://chatgpt.com/backend-api/codex/responses`

The switch is in `ModelProviderInfo::to_api_provider`: ChatGPT,
`chatgptAuthTokens`, and Agent Identity auth use the ChatGPT Codex backend;
other auth modes use `https://api.openai.com/v1` unless a provider base URL is
configured.

## HTTP Request Shape

```http
POST {provider_base_url}/responses
Accept: text/event-stream
Authorization: Bearer <token>
ChatGPT-Account-ID: <account_id>        # ChatGPT/Codex account auth, when present
X-OpenAI-Fedramp: true                  # FedRAMP account only
session-id: <session_uuid>
thread-id: <thread_uuid>
x-client-request-id: <thread_uuid>
x-codex-window-id: <window_id>
```

Other conditional headers are described below.

## Auth Headers

`CodexAuth::ApiKey`, `CodexAuth::Chatgpt`, and
`CodexAuth::ChatgptAuthTokens` are converted into a bearer auth provider:

- `Authorization: Bearer <token>`
  - API key auth: token is the stored API key.
  - ChatGPT auth: token is `tokens.access_token` from local auth storage.
  - External ChatGPT tokens: token is the externally supplied access token.
- `ChatGPT-Account-ID: <account_id>` is added when auth exposes an account id.
- `X-OpenAI-Fedramp: true` is added when auth says the account is FedRAMP.

`CodexAuth::AgentIdentity` does not expose a bearer token through
`get_token()`. It signs an Agent Identity authorization header and still adds
`ChatGPT-Account-ID` and `X-OpenAI-Fedramp` when applicable.

Provider-specific auth can override this path before first-party auth is used:

- `provider.env_key` reads an API key from an environment variable.
- `provider.experimental_bearer_token` uses a configured bearer token.
- `provider.auth.command` can produce a command-backed bearer token.

## Request Body

Canonical JSON body for HTTP streaming:

```json
{
  "model": "<model slug>",
  "instructions": "<base instructions>",
  "input": [ /* ResponseItem[] */ ],
  "tools": [ /* Responses API tool specs */ ],
  "tool_choice": "auto",
  "parallel_tool_calls": true,
  "reasoning": {
    "effort": "low|medium|high|...",
    "summary": "..."
  },
  "store": false,
  "stream": true,
  "include": ["reasoning.encrypted_content"],
  "service_tier": "<supported tier>",
  "prompt_cache_key": "<thread id>",
  "text": {
    "verbosity": "low|medium|high",
    "format": {
      "type": "json_schema",
      "strict": true,
      "schema": {},
      "name": "codex_output_schema"
    }
  },
  "client_metadata": {
    "x-codex-installation-id": "<installation id>"
  }
}
```

Fields skipped by serde are omitted when empty or `None` as noted below.

## Body Field Sources

| Field | Meaning | Source |
| --- | --- | --- |
| `model` | Model slug sent to the provider. | `model_info.slug`. |
| `instructions` | Base/system instructions for the turn. Omitted if empty. | `prompt.base_instructions.text`, resolved from config/history/model defaults. |
| `input` | Conversation input items visible to the model. | `prompt.get_formatted_input()`, currently a clone of `Prompt.input`; turn code builds it from current input or session history. |
| `tools` | Model-visible tool definitions. | `router.model_visible_specs()` -> `create_tools_json_for_responses_api()`. |
| `tool_choice` | Tool choice mode. | Constant `"auto"`. |
| `parallel_tool_calls` | Whether model may call tools in parallel. | `turn_context.model_info.supports_parallel_tool_calls`. |
| `reasoning` | Reasoning effort and summary controls. | Present only when `model_info.supports_reasoning_summaries`; effort is requested effort or model default, summary omitted when configured as `None`. |
| `store` | Whether the provider should store response items. | `provider.is_azure_responses_endpoint()`. If true for Azure, item IDs are attached before send. |
| `stream` | Enables streaming response. | Constant `true`. |
| `include` | Extra response fields requested. | `["reasoning.encrypted_content"]` only when `reasoning` is present; otherwise empty list. |
| `service_tier` | Optional service tier request. | User/config/role setting after `model_info.service_tier_for_request`; omitted if default marker or unsupported by model. |
| `prompt_cache_key` | Cache affinity key. | Current thread id. |
| `text` | Text controls: verbosity and optional JSON schema format. | Present only when verbosity or output schema exists. Verbosity requires model support; schema comes from `turn_context.final_output_json_schema`. |
| `client_metadata` | Metadata inside request body. | HTTP currently includes `x-codex-installation-id`; WebSocket metadata includes more fields. |

`input` items use the `ResponseItem` enum. Common wire variants include
`message`, `reasoning`, `function_call`, `function_call_output`,
`local_shell_call`, `custom_tool_call`, `web_search_call`, and related tool
outputs.

`tools` are serialized `ToolSpec` values. Supported tool spec families include
`function`, `namespace`, `tool_search`, `image_generation`, `web_search`, and
`custom`.

## Extra Headers and Sources

| Header | Source / condition |
| --- | --- |
| `originator` | Default Codex HTTP client header. |
| `User-Agent` | Default Codex HTTP client header. |
| `x-openai-internal-codex-residency` | Added when a residency requirement is configured. |
| `version` | Built-in OpenAI provider static header, value is Codex crate version. |
| `OpenAI-Organization` | Added by built-in OpenAI provider when `OPENAI_ORGANIZATION` is set. |
| `OpenAI-Project` | Added by built-in OpenAI provider when `OPENAI_PROJECT` is set. |
| `Accept: text/event-stream` | Added by the Responses HTTP stream client. |
| `x-client-request-id` | Current thread id. |
| `session-id` | Current session id. |
| `thread-id` | Current thread id. |
| `x-openai-subagent` | Present for subagent sessions, value is source such as `review`, `compact`, `memory_consolidation`, or `collab_spawn`. |
| `x-codex-beta-features` | Enabled beta feature keys for the session. |
| `x-codex-turn-state` | Sticky routing token from prior response header, replayed within the turn once known. |
| `x-codex-turn-metadata` | Optional parsed turn metadata. |
| `x-codex-parent-thread-id` | Present for session sources that carry a parent thread. |
| `x-codex-window-id` | Current model-client window id. |
| `x-oai-attestation` | Present only when the provider supports attestation and an attestation provider returns a header. |

## WebSocket Variant

If Responses-over-WebSocket is enabled, Codex connects to:

```text
wss://chatgpt.com/backend-api/codex/responses
```

or the equivalent URL formed by converting the provider HTTP URL to `ws`/`wss`.
The WebSocket upgrade uses the same auth/provider/default headers. It also adds:

- `OpenAI-Beta: responses_websockets=2026-02-06`
- `x-responsesapi-include-timing-metrics: true` when timing metrics are enabled

The WebSocket message is tagged as:

```json
{
  "type": "response.create",
  "...": "same fields as ResponseCreateWsRequest"
}
```

`ResponseCreateWsRequest` mirrors the HTTP body and adds:

- `previous_response_id`: normally omitted; used when reusing prior websocket state.
- `generate`: set to `false` for warmup requests; omitted for normal generation.

WebSocket `client_metadata` includes `x-codex-installation-id`,
`x-codex-window-id`, optional `x-openai-subagent`,
`x-codex-parent-thread-id`, and optional `x-codex-turn-metadata`. It can also
carry W3C trace context keys for websocket request tracing.

## Explicit Restrictions

- **Auth mode controls default backend.** ChatGPT, external ChatGPT tokens, and
  Agent Identity default to `chatgpt.com/backend-api/codex`; API key auth
  defaults to `api.openai.com/v1`.
- **Credential precedence is fixed.** When enabled, `CODEX_API_KEY` overrides
  other auth. Ephemeral external ChatGPT auth is checked before persisted auth.
  `CODEX_ACCESS_TOKEN` is interpreted as Agent Identity before persistent
  storage is used.
- **Forced login method can log out invalid auth.** If config requires API key
  but ChatGPT auth is active, or requires ChatGPT but API key is active, Codex
  logs out and returns an error.
- **Forced workspace id is enforced.** Browser login, persisted auth checks, and
  external token refresh all reject or clear credentials when
  `chatgpt_account_id` does not match the allowed workspace list.
- **ChatGPT workspace headers matter.** Account id and FedRAMP routing headers
  are derived from auth state and sent when present; they are not arbitrary UI
  labels.
- **Reasoning params are model gated.** `reasoning` is omitted unless the model
  advertises reasoning-summary support.
- **Verbosity is model gated.** A configured verbosity is ignored when the model
  does not support verbosity.
- **Service tier is model gated.** Unsupported service tiers and the explicit
  default marker are omitted.
- **HTTP request retry policy excludes 429.** Default request attempts are 4,
  5xx and transport errors are retried, 429 is not retried. User-configured
  request retry count is capped at 100.
- **Stream reconnect/idle limits exist.** Default stream max reconnects are 5
  and capped at 100. Default stream idle timeout is 300 seconds.
- **WebSocket use is gated.** It requires provider `supports_websockets` and no
  session-local websocket disable flag. HTTP 426 `UPGRADE_REQUIRED` on websocket
  path causes fallback to HTTP. Default websocket connect timeout is 15 seconds.
- **Compression is conditional.** Zstd request compression is used only when the
  feature is enabled, auth uses the Codex backend, and the provider is OpenAI.
- **Azure storage special case.** `store` is true for Azure Responses endpoints;
  when true, Codex attaches item IDs before sending.
- **401 recovery is limited.** On unauthorized response, managed ChatGPT auth
  reloads only if account id matches, then tries token refresh. External auth
  retries through its configured external refresh path. API key auth does not
  silently mint new credentials.
- **Provider auth configurations conflict.** Command-backed provider auth cannot
  be combined with `env_key`, `experimental_bearer_token`, or
  `requires_openai_auth`. AWS provider auth also conflicts with websocket
  support and OpenAI-auth requirements.

## Code Pointers

- Request struct: `codex-rs/codex-api/src/common.rs`
- Request assembly: `codex-rs/core/src/client.rs::build_responses_request`
- HTTP stream endpoint: `codex-rs/codex-api/src/endpoint/responses.rs`
- WebSocket endpoint: `codex-rs/codex-api/src/endpoint/responses_websocket.rs`
- Auth header mapping: `codex-rs/model-provider/src/auth.rs` and
  `codex-rs/model-provider/src/bearer_auth_provider.rs`
- Provider endpoint/retry defaults: `codex-rs/model-provider-info/src/lib.rs`
- Auth loading and restrictions: `codex-rs/login/src/auth/manager.rs`
