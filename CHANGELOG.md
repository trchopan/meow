# Changelog

All notable changes to this project will be documented in this file.

## Unreleased

- Added clipboard paste between host and client machines with a configurable shortcut.
- Added a standalone macOS menu bar app for host and client lifecycle control.
- Hardened app/CLI runtime ownership, stale socket handling, and local IPC limits.
- Improved menu-bar permission diagnostics for the app-owned host/client runtime.
- Moved host and client runtime ownership into `Meow.app` so macOS permissions target the app process.

## v0.2.0 - 2026-07-21

- Added native macOS semantic keyboard and mouse forwarding.
- Added an optional remote input overlay.
- Hardened forwarding recovery and target-transition cleanup.

## v0.1.0 - 2026-07-16

- Initial public open source release.
- Added GitHub tag-based release workflow for macOS artifacts and checksums.
- Documented Homebrew and GitHub Release installation paths.
