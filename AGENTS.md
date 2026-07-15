# AGENTS

Comprehensive handbook for coding agents working in this repository.

## Purpose

- Help agents make correct, safe, and verifiable changes in `meow`.
- Prioritize deterministic local validation before real-device or OS-level side effects.
- Keep edits small, focused, and easy to review.

## Project Summary

- `meow` is a Rust CLI for macOS-first keyboard/mouse sharing over `iroh`.
- It combines pure logic (protocol/state/parsing) with OS-level side effects (global input capture, pointer lock/warp/hide, input injection, permission prompts).
- Repository shape: single binary crate (`src/main.rs`) with internal modules and inline unit tests.

## Agent Operating Principles

- Build context first: inspect relevant modules and nearby tests before editing.
- Prefer small, reversible diffs over broad rewrites.
- Keep a tight change/evaluate loop: run the narrowest useful check early, then full checks for non-trivial work.
- Bias toward pure-logic changes and tests before touching OS side-effect paths.
- Treat runtime input capture/injection and pointer control as hazardous operations.
- Preserve user intent and existing behavior unless the task asks for behavior change.

## First Steps For Any Task

1. Identify the behavior and map it to owning modules.
2. Read current implementation and inline tests in those modules.
3. Choose the smallest implementation strategy.
4. Add or update tests for pure logic when possible.
5. Run validation commands appropriate to the scope.

## Quick Commands

- Full local gate: `make check`
- Format: `make fmt`
- Lint: `make lint`
- Test: `make test`
- Build: `make build`
- One-machine smoke test: `cargo run -- dev-smoke --duration-secs 5 --side right`

## Safe Development Workflow

1. Make code changes.
2. Run focused checks first when available (module-level or targeted tests).
3. Run `make check` for non-trivial changes.
4. If changes involve host/attach forwarding, edge switching, pointer mode, or probe behavior, run:
   - `cargo run -- dev-smoke --duration-secs 5 --side right`
5. Use remote/manual QA only when required for permissions or real-device behavior.

## Repository Map

- `src/main.rs`: CLI entrypoint and top-level command dispatch.
- `src/cli.rs`: command/flag definitions, including hidden diagnostics.
- `src/host.rs`: host daemon lifecycle, peer attach auth, forwarding loop, target switching.
- `src/attach.rs`: client attach loop, forwarded event receive path, optional injection and probe helpers.
- `src/input.rs`: global input grab, detach chord parsing, host-edge push detection, captured event shaping.
- `src/ipc.rs`: Unix socket control plane (`status`, `stop`, target switch, pointer mode).
- `src/protocol.rs`: wire protocol types, frame encoding/decoding, message size guards.
- `src/state.rs`: persistent identity/state files, path resolution, state repair, identity/secret rotation.
- `src/dev.rs`: one-machine dev smoke orchestration using isolated state.
- `src/model.rs`: shared enums/structs for side/target/pointer mode and host runtime state.
- `src/presentation.rs`: user-facing CLI output formatting.
- `src/probe.rs`: hidden pointer-lock diagnostic command implementation.
- `src/host_mouse.rs`: pointer dissociation, cursor warping, visibility helpers.
- `src/macos_mouse_delta.rs`: macOS relative mouse delta capture via CGEventTap.
- `src/macos_permissions.rs`: macOS Accessibility/Input Monitoring checks and prompts.

## Common Change Areas

- Add/change CLI command or flags: `src/cli.rs`, then dispatch in `src/main.rs`.
- Host behavior or forwarding semantics: `src/host.rs` and related model/protocol pieces.
- Attach-side receive/inject behavior: `src/attach.rs`.
- Wire payloads or framing rules: `src/protocol.rs` and all call sites.
- Target switching, pointer mode IPC, status output: `src/ipc.rs` plus `src/presentation.rs`.
- Persisted state schema/defaults/repair: `src/state.rs`.
- Detach key and edge detection logic: `src/input.rs`.
- User-visible text changes: `src/presentation.rs` (not scattered `println!` churn).

## Runtime Architecture Notes

- Host flow: startup permissions -> endpoint bind -> load state -> input capture threads -> forwarding loop -> IPC control socket.
- Attach flow: ephemeral endpoint -> host auth -> receive wire messages -> optional local injection -> optional probe reporting.
- Control flow: local commands (`status`, `stop`, directional switch, `pointer-mode`) go through Unix socket IPC.
- Pointer behavior:
  - `edge-to-edge`: remote edge push can return control toward host side logic.
  - `confine`: keep remote active until explicit local detach chord.

## Testing Strategy

- Unit tests are inline (`#[cfg(test)]`) in source modules; there is currently no standalone `tests/` directory.
- Prefer adding tests when editing pure logic such as:
  - detach chord parsing,
  - edge push detection,
  - protocol framing/limits,
  - state repair/default handling.
- For non-trivial changes, run `make check`.
- For forwarding/probe/edge-switch/pointer-mode changes, also run `dev-smoke`.

## State And Isolation

- Default runtime state directory: `~/.local/share/meow/`
- Files of interest:
  - `host.key`: persistent host private key (stable host endpoint identity).
  - `host_state.json`: persisted attach secret, detach key, pointer mode.
  - `meow.sock`: local control socket for daemon IPC.
- For isolated development runs:
  - `MEOW_STATE_DIR=/tmp/meow-dev meow host`
- `dev-smoke` already uses an isolated temporary state directory.

## macOS And Input Safety

- `meow host` may trigger Accessibility/Input Monitoring permission workflows.
- Avoid real input injection flows unless explicitly required.
- Prefer safe diagnostics:
  - `meow attach ... --no-inject`
  - `meow dev-smoke --duration-secs 5 --side right`
- `meow test-inject` performs real injection and should be treated as hazardous.
- `probe-pointer-lock` temporarily changes pointer lock behavior; use only for pointer diagnostics.
- Pointer hide/show and dissociation are stateful; keep transitions balanced on all exit paths.

## Diagnostics (Hidden CLI)

- `meow test-inject`
- `meow probe-pointer-lock --duration-secs 10`
- `meow attach <host-id> <secret> --side right --probe-received --probe-duration-secs 10`
- `meow attach <host-id> <secret> --side right --no-inject`
- `meow dev-smoke --duration-secs 5 --side right`

## Implementation Guidelines

- Keep protocol changes backward-aware within this repo; update all producer/consumer paths together.
- Do not mix unrelated refactors with behavior fixes.
- Prefer explicit error context (`anyhow::Context`) on fallible IO/runtime operations.
- Preserve existing style and naming unless a task requires cleanup.
- Minimize new dependencies; use existing crate patterns first.

## Review Checklist

- Does the change touch the right module(s) from the map above?
- Are risky runtime paths avoided unless required?
- Are pure-logic tests added/updated where appropriate?
- Did you run the right validation commands for scope?
- Are user-visible messages clear and consistent?

## Do Not Do

- Do not modify `target/` or generated artifacts manually.
- Do not run destructive identity commands (`reset-identity`, `rotate-secret`) unless requested.
- Do not run risky real-input flows when safer probes/smoke paths can validate the change.
- Do not commit or push unless explicitly requested.
