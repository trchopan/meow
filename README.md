# meow

macOS-first keyboard and mouse sharing over iroh.

## Install

### Homebrew

```sh
brew tap trchopan/tap
brew install meow
```

### GitHub Releases

Download the matching archive from the latest release:

- Apple Silicon: `meow-vX.Y.Z-aarch64-apple-darwin.tar.gz`
- Intel Mac: `meow-vX.Y.Z-x86_64-apple-darwin.tar.gz`

Then install:

```sh
tar -xzf meow-vX.Y.Z-<target>.tar.gz
chmod +x meow-vX.Y.Z-<target>/meow
mv meow-vX.Y.Z-<target>/meow ~/.local/bin/
```

Verify:

```sh
meow --version
```

Unsigned binaries may be quarantined by macOS. If needed:

```sh
xattr -dr com.apple.quarantine ~/.local/bin/meow
```

## Development Status

`meow` is currently in the **development** stage.

- APIs, CLI behavior, and wire protocol details may change between releases.
- Stability and ergonomics are still being improved.
- `meow` is macOS-first today; other platforms are unsupported for now.

## What It Does

`meow` runs a host daemon on the machine with your physical keyboard/mouse, then forwards input to attached machines over [iroh](https://www.iroh.computer).

You can switch active targets from the host with directional commands (`meow right`, `meow left`, etc.) or return to local control (`meow local`).

## Features

- Keyboard and mouse forwarding over iroh.
- Directional target switching (`left`, `right`, `up`, `down`).
- Two remote pointer modes: `edge-to-edge` and `confine`.
- Local detach chord for fast return to host input.
- Persistent host identity and attach secret.
- Local control socket for status and runtime control commands.
- Optional remote input overlay for displaying pressed keys and buttons.

## Requirements

- macOS host and client machines.
- Rust toolchain (`cargo`, `rustc`) to build from source.
- Accessibility permission for the app launching `meow` (Terminal, iTerm, etc.).
- Input Monitoring permission may also be required, depending on setup.

## Install From Source

Build a release binary:

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

## Quick Start

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
   ```

5. Optionally switch pointer mode while the host daemon is running:

   ```sh
   meow pointer-mode edge-to-edge
   meow pointer-mode confine
   ```

## Commands

Daemon and control commands:

```sh
meow host
# Optional: customize host edge activation.
meow host --edge-zone-px 12 --edge-dwell-ms 150
meow status
meow stop
meow local
meow right
meow left
meow up
meow down
meow pointer-mode <edge-to-edge|confine>
meow reset-identity
meow rotate-secret
```

Attach command:

```sh
meow attach <host-id> <secret> --side <left|right|up|down>
```

The optional input overlay can be enabled on the attached machine:

```sh
meow attach <host-id> <secret> --side right --input-overlay
```

Use `--input-overlay-position top-left|top-right|bottom-left|bottom-right` and
`--input-overlay-idle-ms <milliseconds>` to configure its position and idle timeout.

## Pointer Modes

`meow` supports two remote pointer behaviors:

- `edge-to-edge` (default): moving to a host edge switches control to the connected client on that side; reaching the client edge facing back toward the host returns control to local host input.
- `confine`: keeps control on the remote even at client edges; use the host detach chord to return to local.

Host edge switching uses a configurable activation zone and dwell time. The defaults
are a 12-pixel zone and 150 milliseconds. The pointer must enter and remain in the
zone; it switches once per edge entry and re-arms after leaving the zone.

Set mode while the host daemon is running:

```sh
meow pointer-mode edge-to-edge
meow pointer-mode confine
```

## Escape Chord

When input is forwarded to a remote machine, press `ctrl+alt+cmd+l` on the host to force control back to local.

You can customize this by editing `detach_key` in `host_state.json`.

## State Files

`meow` stores host state in:

```text
~/.local/share/meow/
```

Override this location for development/testing:

```sh
MEOW_STATE_DIR=/tmp/meow-dev meow host
```

- `host.key`: persistent host private key (keeps host endpoint id stable).
- `host_state.json`: persisted metadata, including attach secret and detach key chord.
- `meow.sock`: local Unix socket used for host daemon control.

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

- `host daemon is not running`: start host with `meow host`.
- Attach rejected: verify host id and secret are current.
- Input injection issues: re-check Accessibility and Input Monitoring permissions.
- Verbose logs:

  ```sh
  RUST_LOG=meow=debug meow host
  ```

## Development

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

Local one-machine smoke test:

```sh
cargo run -- dev-smoke --duration-secs 5 --side right
```

This runs a host + attach probe on the same machine with a temporary isolated state directory.

## Developer Diagnostics

These commands and flags are intentionally hidden from default CLI help, but remain available for debugging:

```sh
meow test-inject
meow probe-pointer-lock --duration-secs 10
meow attach <host-id> <secret> --side right --probe-received --probe-duration-secs 10
meow attach <host-id> <secret> --side right --no-inject
meow dev-smoke --duration-secs 5 --side right
```

## Security And Privacy

`meow` is an input-forwarding tool and interacts with privileged OS APIs.

- It captures local keyboard and mouse input on the host while forwarding is active.
- It can inject input events on attached machines.
- Run it only on machines and networks you trust.
- Keep your session secret private.

## License

This project is licensed under the MIT License. See `LICENSE`.
