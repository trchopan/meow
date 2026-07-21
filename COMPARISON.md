# Comparison

`meow` is a modern keyboard and mouse sharing tool built for developers.
This document compares it with existing solutions and clarifies where it fits.

---

## TL;DR

| Tool                | Works across networks | Encryption | CLI-first  | Setup complexity | UX model         |
| ------------------- | --------------------- | ---------- | ---------- | ---------------- | ---------------- |
| **meow**            | ✅ Yes (native)       | ✅ Yes     | ✅ Yes     | Low              | Input forwarding |
| Barrier / Synergy   | ❌ No (LAN only)      | ⚠️ Limited | ❌ No      | Medium           | Input forwarding |
| Tailscale + Barrier | ✅ Yes (via VPN)      | ✅ Yes     | ⚠️ Partial | High             | Input forwarding |
| Remote Desktop      | ✅ Yes                | ✅ Yes     | ❌ No      | Medium           | Screen streaming |

---

## What `meow` is

> `meow` = keyboard and mouse sharing that works across networks, securely, with minimal setup.

- No VPN required
- No GUI required
- No central server required
- Built on peer-to-peer connectivity (iroh)

---

## vs Barrier / Synergy

### Summary

Barrier and Synergy are the closest conceptual match.

### Strengths of Barrier / Synergy

- Mature ecosystem
- Familiar “move cursor across screen edge” UX
- Cross-platform support
- GUI configuration

### Limitations

- LAN-only by design
- Requires manual networking setup for remote usage
- No built-in NAT traversal
- Weak or optional encryption
- Not designed for automation or scripting

### `meow` differences

- Works across networks out of the box (NAT traversal via iroh)
- Encrypted by default
- CLI-first and scriptable
- Explicit attach model (no hidden discovery)
- Designed for developer workflows

---

## vs Tailscale + Barrier (common workaround)

### Summary

A common setup is:

- Use Tailscale for networking
- Run Barrier on top

### Strengths

- Works across networks
- Secure transport via Tailscale
- Reuses existing tools

### Limitations

- Two-layer setup (network + app)
- Requires account/login
- More moving parts to debug
- Not purpose-built for this use case

### `meow` differences

- Single binary
- No external dependencies
- No account required
- Integrated transport + input forwarding
- Simpler mental model

---

## vs Remote Desktop (Parsec, AnyDesk, etc.)

### Summary

Remote desktop tools solve a different problem.

### Strengths

- Full desktop access
- Works anywhere
- High compatibility

### Limitations

- Streams video (higher latency and resource usage)
- Does not feel like local input
- Overkill for simple multi-machine workflows
- Window/context switching overhead

### `meow` differences

- No video streaming
- Near-local input feel (latency-sensitive design)
- Keeps each machine’s display independent
- Optimized for “multiple machines, one keyboard”

---

## vs Hardware solutions (KVM switches, Logitech Flow)

### Summary

Hardware and vendor-specific solutions exist but are limited.

### Strengths

- Reliable (hardware KVM)
- Simple for supported setups
- No software configuration (KVM)

### Limitations

- Physical constraints (cables, ports)
- Limited flexibility
- Vendor lock-in (Logitech Flow)
- Often same-network only

### `meow` differences

- Software-only
- Works across networks
- No hardware required
- Vendor-independent

---

## Design philosophy differences

`meow` intentionally makes different trade-offs:

### Explicit over implicit

- You attach using a host ID and secret
- No automatic discovery magic

### CLI over GUI

- Designed for scripting, hotkeys, automation
- Easy integration with tools like `skhd`, `Hammerspoon`, `tmux`

### Peer-to-peer over centralized

- No cloud service
- No account
- No relay required in normal cases

### Minimal over feature-heavy

- Focus on input forwarding only
- Avoids becoming a remote desktop or workspace manager

---

## When to use `meow`

Use `meow` if you:

- Use multiple machines regularly (e.g. laptop + desktop, work + home)
- Want seamless keyboard/mouse control across them
- Need it to work across different networks
- Prefer CLI tools and minimal setup
- Care about security and local control

---

## When NOT to use `meow`

`meow` may not be the right choice if you:

- Need full remote desktop access (use Parsec / AnyDesk)
- Want a GUI-driven setup experience
- Need broad cross-platform support today
- Expect plug-and-play consumer UX

---

## Future direction

Potential areas of expansion (not current scope):

- Clipboard synchronization
- Multi-platform support
- Device trust / authorization management
- Optional discovery mechanisms

---

## Summary

`meow` does one thing:

> Forward your keyboard and mouse to another machine as if it were local.

It focuses on doing that:

- securely
- simply
- across networks
- without extra infrastructure

If that is your use case, it should feel like the most direct solution.
