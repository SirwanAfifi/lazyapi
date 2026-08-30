# LazyAPI

LazyAPI is a terminal tool for OpenAPI services.
It can capture, validate, mock, proxy, save, load, and replay HTTP traffic.
It masks sensitive data by default.

## Requirements

- Rust 1.88 or later
- An OpenAPI 3 document in JSON or YAML format

## Start LazyAPI

1. Start the sample service:

   ```bash
   cargo run -- --spec ./testdata/sample_openapi.yaml
   ```

2. Send a request to the service:

   ```bash
   curl -i http://127.0.0.1:3000/users
   ```

The server uses `127.0.0.1:3000` by default.

To send requests to another service, add its base URL:

```bash
cargo run -- \
  --spec ./openapi.yaml \
  --target http://127.0.0.1:8080
```

## Save a session

Use `--save FILE` to save traffic as JSON, JSONL, NDJSON, or HAR.
Use `--load FILE` to load a JSON, JSONL, or NDJSON session.

```bash
cargo run -- --spec ./openapi.yaml --save ./capture.json
cargo run -- --spec ./openapi.yaml --load ./capture.json
```

If you must record sensitive data, use `--no-redact`.

## Use the terminal interface

- Use `Up` and `Down`, or `k` and `j`, to select an item.
- Use `Tab` to select a pane.
- Use `/` to search.
- Use `Enter` or `Space` to show traffic for an operation.
- Use `R` to review and replay a request.
- Use `c` to show a cURL command.
- Use `?` or `h` to show help.
- Use `q` or `Ctrl+C` to stop LazyAPI.

Use `cargo run -- --help` to see all options.

## Test the code

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

## License

MIT © Sirwan Afifi
