# HTTP Gateway

The gateway exposes UIntell Agent as an authenticated JSON API. It binds to
`127.0.0.1:3000` by default and requires a working model provider and graph
memory before the listener starts.

## Start

```bash
export DEEPSEEK_API_KEY="sk-..."
export UINTELL_API_KEY="replace-with-a-long-random-secret"
uintell-agent serve --addr 127.0.0.1:3000
```

Use `uintell-agent --ollama serve --addr 127.0.0.1:3000` for the selected local
Ollama model. Keep the gateway on a private interface or put it behind a TLS
reverse proxy. `UINTELL_API_KEY` is only for gateway clients and is never used
as a provider credential.

Authenticated requests may use either header:

```text
X-API-Key: <UINTELL_API_KEY>
Authorization: Bearer <UINTELL_API_KEY>
```

## Endpoints

### `GET /health`

No authentication is required. A successful response is HTTP 200:

```json
{"status":"ok","version":"1.0.0","uptime_secs":42}
```

### `GET /ready`

Authentication is required. HTTP 200 means the process completed provider
preflight and graph initialization before it started listening:

```json
{"status":"ready","provider":"deepseek-v4-pro"}
```

This endpoint does not spend a model request. Provider failures after startup
are reported by `/chat`.

### `POST /chat`

Authentication and `Content-Type: application/json` are required:

```bash
curl --fail-with-body http://127.0.0.1:3000/chat \
  -H "X-API-Key: $UINTELL_API_KEY" \
  -H "Content-Type: application/json" \
  -H "X-Request-Id: example-1" \
  --data '{"message":"Summarize this repository","session_id":"demo"}'
```

`message` must be non-empty and no longer than 32,000 characters.
`session_id` is optional, limited to 128 ASCII characters, and may contain
letters, digits, `.`, `_`, and `-`. The optional `X-Request-Id` has the same
limit and also permits `:`. Invalid request IDs are replaced with a generated
UUID.

A successful response is HTTP 200:

```json
{
  "id": "example-1",
  "status": "ok",
  "response": "...",
  "provider": "deepseek-v4-pro",
  "usage": null,
  "error": null
}
```

Session history is process-local and is not durable. The gateway keeps at most
64 recent sessions and 20 messages per session, evicting the least recently
used session when full.

## Limits And Errors

- Request body: 128 KiB
- Authenticated requests: 60 per minute
- Agent request timeout, including queue time: 120 seconds
- Concurrent model execution: serialized per gateway process

Authentication and middleware errors use this shape:

```json
{"error":{"code":"UNAUTHORIZED","message":"..."}}
```

`/chat` validation and execution errors use the normal chat response shape
with `status` set to `error` or `timeout`.

| HTTP | Code | Meaning |
| --- | --- | --- |
| 400 | `EMPTY_MESSAGE` | `message` is empty |
| 400 | `MESSAGE_TOO_LARGE` | `message` exceeds 32,000 characters |
| 400 | `INVALID_SESSION_ID` | `session_id` is empty, too long, or malformed |
| 401 | `UNAUTHORIZED` | API key is missing or invalid |
| 413 | framework body-limit response | JSON body exceeds 128 KiB |
| 429 | `RATE_LIMITED` | API key exceeded 60 requests per minute |
| 502 | `AGENT_ERROR` | Provider or agent execution failed |
| 503 | `AUTH_NOT_CONFIGURED` | `UINTELL_API_KEY` is unset or empty |
| 504 | `TIMEOUT` | Request exceeded 120 seconds |

## CORS

By default, browser requests are allowed from `http://localhost:3000` and
`http://127.0.0.1:3000`. Set a comma-separated allow-list with:

```bash
export UINTELL_CORS_ORIGINS="https://app.example.com,https://admin.example.com"
```

`UINTELL_CORS_ALLOW_ANY=1` enables every origin and should not be used on a
public deployment.
