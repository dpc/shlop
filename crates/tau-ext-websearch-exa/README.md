# tau-ext-websearch-exa

A tau extension that registers a `websearch_exa` tool, proxying agent
search requests to Exa's keyless free-tier hosted MCP at
<https://mcp.exa.ai/mcp>.

The free tier currently allows ~1000 requests/month per IP with no
API key. If you need more, set `?exaApiKey=…` on the endpoint via the
`config.endpoint` field (forthcoming) or run your own `exa-mcp-server`
and point the extension at it.

## What it does

On each `tool.invoke` for `websearch_exa`, the extension:

1. Re-encodes the agent's CBOR arguments (`query`, optional
   `num_results`) into a JSON-RPC `tools/call` envelope for Exa's
   `web_search_exa` tool.
2. Sends `POST` to the configured Exa endpoint with
   `Accept: application/json, text/event-stream` and the
   `MCP-Protocol-Version` header. Exa replies with a
   Streamable-HTTP `text/event-stream` body carrying one `message`
   SSE frame.
3. Decodes that frame, pulls the `result.content[*].text` chunks, and
   returns the concatenated text blob to the agent as a `tool.result`.
4. Surfaces transport / JSON-RPC / schema failures as `tool.error`s
   with the original arguments echoed in `details`.

## Tool schema

```json
{
  "name": "websearch_exa",
  "parameters": {
    "type": "object",
    "properties": {
      "query":       { "type": "string" },
      "num_results": { "type": "integer", "minimum": 1, "maximum": 100 }
    },
    "required": ["query"]
  }
}
```

`num_results` defaults to 5 when omitted. The query should describe
the *ideal page* in natural language — e.g.
`blog post comparing React and Vue performance` — rather than reading
like a keyword search; Exa's semantic search performs noticeably
better that way. Prefix with `category:people` or `category:company`
to scope to LinkedIn-style profiles or company pages.

## Configuration

Enabled by default. Disable in `harness.json5` if you'd rather not
make outbound HTTP calls:

```json5
{
  extensions: {
    "core-websearch-exa": { enable: false },
  },
}
```

There are no extension-level config fields today; richer settings
(custom endpoint, API key, default `num_results`, additional Exa tools
such as `web_fetch_exa`) can be added per-need.

## Tracing

```sh
TAU_EXT_LOG=websearch-exa=debug tau …
```

`debug` shows one line per request with query, requested count, and
response size. `warn` shows network / decode failures.
