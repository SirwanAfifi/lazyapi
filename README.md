# LazyAPI

LazyAPI is a keyboard-first terminal API workbench written in Rust. It reads an
OpenAPI 3 JSON or YAML document, captures live HTTP traffic, validates exchanges
against the contract, and displays request/response details in an adaptive TUI.
Wide terminals show endpoints and requests together, compact terminals rebalance
those panes, and narrow terminals show one focused pane at a time. Server output
opens on demand instead of permanently taking inspector space.

The capture server is built into the binary. Without a target it serves mock
responses from documented examples and schemas. With `--target`, it forwards
requests to the real service while preserving the upstream status, headers,
and body.

Captured exchanges are marked valid, invalid with field-level findings, or
amber/inconclusive when the available evidence cannot be checked safely.
Validation covers required parameters, JSON syntax and common schema assertions,
content types, and documented status codes. Local JSON Pointer `$ref` values are
resolved within depth and expansion budgets; external and anchor references are
rejected, while exercised recursive branches are reported as inconclusive.

## Run

Requirements: a current stable Rust toolchain and an OpenAPI 3 document.

```bash
cargo run -- --spec ./testdata/sample_openapi.yaml
```

LazyAPI listens on `127.0.0.1:3000` by default:

```bash
curl -i http://127.0.0.1:3000/users
```

To inspect a real service, provide its base URL:

```bash
cargo run -- \
  --spec ./openapi.yaml \
  --listen 127.0.0.1:3000 \
  --target http://127.0.0.1:8080
```

Captured secrets are masked by default. This includes authorization, cookie,
and API-key headers; token-bearing URL headers; and common token, secret, and
password fields in query strings, JSON, and form bodies. Encoded bodies that
cannot be inspected safely are masked as a whole. Use `--no-redact` only when
the unmasked values are deliberately required.

To stream the complete session to a private, atomically published file, choose
LazyAPI JSON, JSONL/NDJSON, or HAR:

```bash
cargo run -- --spec ./openapi.yaml --target http://127.0.0.1:8080 \
  --save ./capture.json

cargo run -- --spec ./openapi.yaml --load ./capture.json
```

`--load` accepts LazyAPI JSON arrays and JSONL/NDJSON. Loading and saving can be
combined without buffering the complete session in memory. HAR is export-only.
On Unix, saved files are created with mode `0600`. Without `--save`, the TUI
retains the newest 500 exchanges within a 64 MiB display budget and performs no
session spooling.

Binding to loopback by default keeps captured development traffic local.
Request bodies are limited to 2 MiB; larger requests receive `413 Request
Entity Too Large`. Larger upstream responses are forwarded in full but their
captured body is truncated after 2 MiB and contract validation is marked partial.

## Controls

- `↑`/`↓` or `k`/`j`: navigate endpoints, captured exchanges, or server output
- `Tab` / `Shift+Tab`: move between the visible panes
- `Enter` or `Space`: focus logs for the selected OpenAPI operation
- `/`: edit the focused search; `Enter` applies and `Esc` restores the prior query
- `1`–`5`: select Selected, All, Errors, Slow, or Unmatched traffic directly
- `v`: cycle the traffic filters
- `f`: pause or resume following the newest visible exchange
- `R`: review and confirm replaying the selected exchange through the capture server
- `c`: expand a copyable cURL command for the selected exchange
- `←`/`→` or `[`/`]`: switch Request, Response, Headers, Contract, and cURL tabs
- `Page Up` / `Page Down`: scroll the selected request or response body
- `Home` / `End`: jump to the start or end of inspector/server content
- `p`: toggle pretty-printed and raw bodies
- `w`: toggle body line wrapping
- `e`: expand or close the request/response detail view
- `o`: open or close the server-output drawer
- Mouse click: focus panes, choose traffic filters, search, select exchanges, or switch tabs
- Mouse wheel: navigate lists or scroll the body/server output under the pointer
- Horizontal wheel: switch detail tabs
- `r`: restart the capture server
- `?` or `h`: toggle help
- `q` or `Ctrl+C`: quit

Endpoint search includes method, path, summary, tag, and operation ID. Traffic
search includes method, path, status, latency, query, repeated headers, bodies,
and contract findings. OpenAPI
templates such as `/users/{userId}` match concrete paths such as `/users/42`.
JSON bodies are pretty-printed and syntax-coloured; query parameters and
headers have dedicated sections. Repeated values such as `Set-Cookie` remain
separate through proxying, capture, replay, cURL, and HAR export. Binary bodies are shown as
content-type and size metadata instead of terminal garbage.

Replay and cURL generation fail closed when a request capture is redacted,
truncated, lossy, or otherwise cannot reproduce the original bytes faithfully.
Replay always shows the method and request target for confirmation first, labels
the captured Host separately from the active replay route, and adds a warning
for methods that may change upstream data.

## Build and test

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo build --release
```

## Project layout

```text
src/main.rs       CLI and application lifecycle
src/model.rs      endpoint, contract-validation, and traffic models
src/server.rs     concurrent capture, mock, proxy, redaction, and replay server
src/session.rs    LazyAPI session and HAR persistence
src/spec.rs       OpenAPI operation loading and local reference resolution
src/ui.rs         Ratatui event loop and rendering
testdata/         sample OpenAPI document
```

## License

MIT © Sirwan Afifi
