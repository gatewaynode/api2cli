# Requirements: api2cli

A CLI app that exposes a generic REST endpoint and forwards incoming HTTP requests to a CLI application or shell pipe.

---

## CLI Options

| Flag | Short | Default | Description |
|---|---|---|---|
| `--port` | `-p` | `8080` | Port for the HTTP server to listen on |
| `--command` | `-c` | _(none)_ | Command to spawn and pipe requests into. Omit to use stdout (pipe mode). |
| `--persistent` | `-P` | `false` | Keep the server running after the first request |
| `--passthrough` | | `body` | What to forward: `body` (raw body only) or `full` (JSON envelope with method, path, headers, body) |

---

## Forwarding Modes

### Pipe mode (no `--command`)
`api2cli` writes each request to its own **stdout**. The user shell-pipes it to another application:
```
api2cli --port 8080 --persistent | myapp
```

### Explicit command mode (`--command <CMD>`)
`api2cli` spawns the given command as a subprocess and writes each request to its **stdin**:
```
api2cli --port 8080 --persistent --command myapp
```
In persistent mode the subprocess is spawned once at startup and kept alive. In one-shot mode a fresh subprocess is spawned per request.

---

## Passthrough Formats

### `--passthrough body` (default)
Raw request body bytes are written verbatim, followed by a newline.

### `--passthrough full`
A JSON object is written, followed by a newline:
```json
{"method":"POST","path":"/foo/bar","headers":{"content-type":"application/json"},"body":"..."}
```
`body` is the raw body as a UTF-8 string (empty string if no body).

---

## Persistence Behaviour

| `--command` | `--persistent` | Behaviour |
|---|---|---|
| absent | absent | Accept one request → write to stdout → exit |
| absent | set | Write every request to stdout → run forever |
| set | absent | Spawn subprocess → write one request to its stdin → exit |
| set | set | Spawn one subprocess at startup → write every request to its stdin → run forever |

---

## HTTP Response

All requests receive an immediate `200 OK` with an empty body. The app does not wait for the downstream CLI/pipe to process the request before responding.

---

## Error Handling

- If the subprocess exits unexpectedly in persistent mode, the server logs an error and continues accepting requests (attempting to respawn is out of scope).
- Invalid `--passthrough` values are rejected at startup with a clear error message.
