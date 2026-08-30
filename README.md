# LazyAPI

LazyAPI is a keyboard-first terminal API inspector written in Rust. It reads
the operations in an OpenAPI 3 JSON or YAML document, captures live HTTP
traffic, and displays matching request/response details in a three-pane TUI.

The capture server is built into the binary. Without a target it returns a
small local acknowledgement. With `--target`, it forwards requests to the real
service while preserving the upstream status, headers, and body.

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

Binding to loopback by default keeps captured development traffic local.
Request bodies are limited to 2 MiB; larger requests receive `413 Request
Entity Too Large`.

## Controls

- `↑`/`↓` or `k`/`j`: navigate endpoints, captured exchanges, or server output
- `Tab` / `Shift+Tab`: move between Endpoints, Logs, and Server panes
- `Enter` or `Space`: focus logs for the selected OpenAPI operation
- `/`: search by HTTP method or path
- `←`/`→` or `[`/`]`: switch Request, Response, and Headers tabs
- `Page Up` / `Page Down`: scroll the selected request or response body
- `p`: toggle pretty-printed and raw bodies
- `w`: toggle body line wrapping
- `e`: expand or close the request/response detail view
- Mouse click: focus panes, select endpoints/exchanges, and switch detail tabs
- Mouse wheel: navigate lists or scroll the body/server output under the pointer
- Horizontal wheel: switch Request, Response, and Headers tabs
- `r`: restart the capture server
- `?` or `h`: toggle help
- `q` or `Ctrl+C`: quit

OpenAPI templates such as `/users/{userId}` match concrete paths such as
`/users/42`. JSON bodies are pretty-printed and syntax-coloured; query
parameters and alphabetized headers have dedicated sections. Binary bodies are
shown as content-type and size metadata instead of terminal garbage.

## Build and test

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

## Project layout

```text
src/main.rs       CLI and application lifecycle
src/model.rs      endpoint matching and traffic models
src/server.rs     built-in capture/proxy server
src/spec.rs       deterministic OpenAPI operation loading
src/ui.rs         Ratatui event loop and rendering
testdata/         sample OpenAPI document
```

## License

MIT © Sirwan Afifi
