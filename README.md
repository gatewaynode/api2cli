# api2cli

A lightweight server that accepts any HTTP request and forwards it to a CLI application or shell pipe. Useful for bridging HTTP-based tooling (webhooks, scripts, other services) into local command-line programs without writing glue code.

---

## Install

**Requirements:** Rust 1.75+ ([rustup.rs](https://rustup.rs))

```bash
git clone https://github.com/youruser/api2cli
cd api2cli
cargo install --path .
```

This places the `api2cli` binary in `~/.cargo/bin/`. Make sure that directory is on your `$PATH`.

To build without installing:

```bash
cargo build --release
# binary at: ./target/release/api2cli
```

---

## Usage

```
api2cli [OPTIONS]

Options:
  -p, --port <PORT>          Port to listen on [default: 1337]
  -c, --command <CMD>        Command to spawn and pipe requests into
  -P, --persistent           Keep the server running after the first request
      --passthrough <MODE>   What to forward: body (default) or full
      --log-level <LEVEL>    Log level: error, warn, info, debug, trace [default: info]
  -h, --help                 Print help
```

---

## How it works

Each incoming HTTP request is forwarded to either:

- **Pipe mode** (default, no `--command`): the request is written to `api2cli`'s **stdout**. Chain it with `|` to feed another program.
- **Subprocess mode** (`--command <CMD>`): `api2cli` spawns the command via `sh -c` and writes each request to its **stdin**.

All requests receive an immediate `200 OK`. The server does not wait for the downstream program to process before responding.

### Passthrough formats

| `--passthrough` | What is written |
|---|---|
| `body` (default) | Raw request body, followed by a newline |
| `full` | JSON envelope: `{"method","path","headers","body"}`, followed by a newline |

### Persistence

Without `--persistent`, the server exits after handling one request.
With `--persistent`, the server runs indefinitely (and in subprocess mode, a single process is kept alive across all requests).

---

## Examples

**One-shot pipe — capture a single webhook body:**
```bash
api2cli | jq .
# In another terminal:
curl -s -X POST localhost:1337/hook -d '{"event":"push"}'
```

**Persistent pipe — stream all requests into a processor:**
```bash
api2cli --persistent --passthrough full | while read line; do echo "$line" | jq .method; done
```

**Subprocess — persistent, send each request body to a script:**
```bash
api2cli --persistent --command 'python3 handler.py'
```

**Full request envelope to a command:**
```bash
api2cli --persistent --passthrough full --command 'node process.js'
```

**Custom port:**
```bash
api2cli --port 8080 --persistent | cat
```

---

## Configuration file

Default options can be set in `~/.config/api2cli/config.toml`. The file is created by the user; any missing field falls back to the hardcoded default. CLI flags always override config file values.

```toml
port       = 1337
persistent = false
passthrough = "body"
log_level  = "info"

# command = "myapp --flag"
```

---

## Logging

Logs are written to both **stderr** and a daily-rotating file in:

```
~/.local/state/api2cli/api2cli.log.YYYY-MM-DD
```

The directory is created automatically on first run. If `$XDG_STATE_HOME` is set it is used instead of `~/.local/state`. Log level is controlled with `--log-level` or the `log_level` config key.

---

## Development

```bash
cargo build          # debug build
cargo test           # run test suite
cargo clippy         # lint
cargo fmt            # format
```
