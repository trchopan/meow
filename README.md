# meow

macOS-first keyboard and mouse sharing over iroh.

## Requirements

- macOS host and client machines
- Rust toolchain (`cargo`, `rustc`) to build
- Accessibility permission for the app launching `meow` (Terminal, iTerm, etc.)
- Input Monitoring permission may also be required, depending on setup

## Build

```sh
cargo build --release
```

Binary path:

```text
target/release/meow
```

You can also run directly with Cargo during development:

```sh
cargo run -- host
```

## Development workflow

Fast local gate:

```sh
make check
```

Individual steps:

```sh
make fmt
make lint
make test
make build
```

Local one-machine smoke:

```sh
cargo run -- dev-smoke --duration-secs 5 --side right
```

This runs a host + attach probe on the same machine with a temporary isolated state directory.

## Quick start

1. On the machine with the physical keyboard/mouse (host), start the daemon:

   ```sh
   meow host
   ```

2. Copy the printed `Host endpoint id` and `Session secret`.

3. On a remote machine, attach to the host:

   ```sh
   meow attach <host-id> <secret> --side right
   ```

   Supported side values: `left`, `right`, `up`, `down`.

4. On the host, switch where input is sent:

   ```sh
   meow local
   meow right
    meow left
    meow up
    meow down
    meow pointer-mode edge-to-edge
    meow pointer-mode confine
    ```

## Commands

Daemon and control commands:

```sh
meow host
meow status
meow stop
meow pointer-mode <edge-to-edge|confine>
meow reset-identity
meow rotate-secret
```

Attach command:

```sh
meow attach <host-id> <secret> --side <left|right|up|down>
```

## Escape chord

When input is currently forwarded to a remote machine, press `ctrl+alt+cmd+l` on the host to force control back to local.

You can customize this by editing `detach_key` in `host_state.json`.

## Pointer mode

`meow` supports two remote pointer behaviors:

- `edge-to-edge` (default): moving to a host edge switches control to the connected client on that side; reaching the client edge facing back toward the host returns control to local host input.
- `confine`: keep control on the remote even at client edges; use the host detach chord to return to local.

Set mode while host daemon is running:

```sh
meow pointer-mode edge-to-edge
meow pointer-mode confine
```

## State files

`meow` stores host state in:

```text
~/.local/share/meow/
```

Override this location for development/testing:

```sh
MEOW_STATE_DIR=/tmp/meow-dev meow host
```

- `host.key`: persistent host private key (keeps host endpoint id stable)
- `host_state.json`: persisted metadata, including attach secret and detach key chord
- `meow.sock`: local Unix socket used for host daemon control

Example `host_state.json`:

```json
{
  "schema_version": 1,
  "endpoint_id": "...",
  "attach_secret": "...",
  "detach_key": "ctrl+alt+cmd+l",
  "remote_pointer_mode": "edge_to_edge"
}
```

`detach_key` format is `modifier+modifier+key` (case-insensitive). Supported modifiers: `ctrl`, `alt`, `cmd` (or `meta`, `super`, `win`), `shift`. Supported keys: `a-z`, `0-9`, `space`, `tab`, `enter`, `escape`.

Use `meow reset-identity` (while daemon is stopped) to remove identity files and force a new host id on the next `meow host` run.

Use `meow rotate-secret` (while daemon is stopped) to keep the same host id and generate a new attach secret.

## Troubleshooting

- `host daemon is not running`: start host with `meow host`
- attach rejected: verify host id and secret are current
- input injection issues: re-check Accessibility/Input Monitoring permissions
- verbose logs:

  ```sh
  RUST_LOG=meow=debug meow host
  ```

## Developer diagnostics

These commands/flags are intentionally hidden from the default CLI help but remain available for debugging:

```sh
meow test-inject
meow probe-pointer-lock --duration-secs 10
meow attach <host-id> <secret> --side right --probe-received --probe-duration-secs 10
meow attach <host-id> <secret> --side right --no-inject
meow dev-smoke --duration-secs 5 --side right
```
