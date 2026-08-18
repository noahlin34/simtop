# simtop — Product Context

## Overview

simtop is a macOS terminal application for managing Apple iOS Simulators. It provides:

- An interactive terminal UI with keyboard and mouse support
- A command-line interface for one-shot operations and automation
- Simulator lifecycle management
- App installation and execution
- Device logs and screenshots
- Xcode project discovery and build/run workflows
- Structured JSON output for scripts and CI

simtop is designed for developers and automation workflows that regularly interact with multiple simulators.

## Interactive TUI

Running `simtop` opens the interactive interface.

The TUI contains two persistent views.

### Simulators view

The Simulators view provides:

- Live simulator snapshots
- Device names, states, runtimes, OS versions, availability, and UDIDs
- Searchable device lists
- State filters for all, booted, or shutdown devices
- Selected-device details
- Live device logs
- Recent activity history
- Adjustable refresh intervals
- Display preferences
- Help and confirmation dialogs

It supports both keyboard and mouse interaction, including:

- Selecting devices
- Clicking tabs and action controls
- Scrolling with the mouse wheel
- Navigating with arrow keys or `j`/`k`
- Opening dialogs and prompts
- Triggering simulator and app operations

Available actions depend on the selected device’s state and availability.

### Projects view

The Projects view provides an Xcode project build and run workflow.

It can:

- Discover `.xcodeproj` and `.xcworkspace` containers
- Scan configured project roots
- Add project roots
- Filter discovered projects
- Load available schemes and configurations
- Select an Xcode container
- Select a scheme
- Select a build configuration
- Select an available simulator
- Build the selected project
- Stream build output
- Cancel an active build
- Run a project end to end

The Run workflow:

1. Selects an Xcode project or workspace
2. Selects a scheme
3. Selects a build configuration
4. Selects a simulator
5. Boots the simulator if needed
6. Builds the application
7. Resolves the resulting app product
8. Installs the app
9. Launches the app

Project roots and project-specific selections are persisted between sessions.

## Simulator operations

simtop supports the following simulator operations:

- List simulators
- Watch simulator snapshots continuously
- Boot a simulator
- Shut down a simulator
- Open Simulator.app focused on a selected device
- Create a simulator
- Delete a simulator
- Capture a PNG screenshot

Boot and shutdown operations are idempotent when the simulator is already in the requested state.

## App operations

For a selected simulator, simtop supports:

- Installing an `.app` bundle
- Launching an installed app by bundle identifier
- Terminating an app
- Uninstalling an app
- Opening HTTP, HTTPS, or custom-scheme URLs
- Printing recent device logs
- Following new log entries

The launch operation can report the launched process ID when it is available.

## Command-line interface

The CLI commands are:

```text
simtop
simtop list
simtop watch
simtop boot <selector>
simtop shutdown <selector>
simtop open <selector>
simtop create
simtop delete <selector>
simtop screenshot <selector>
simtop app <selector> install <path.app>
simtop app <selector> launch <bundle-id>
simtop app <selector> terminate <bundle-id>
simtop app <selector> uninstall <bundle-id>
simtop app <selector> logs
simtop app <selector> open-url <url>
```

Examples:

```bash
simtop
simtop list
simtop list --json
simtop watch --interval 2 --count 10

simtop boot "iPhone 16 Pro"
simtop shutdown "iPhone 16 Pro"
simtop open "iPhone 16 Pro"

simtop screenshot "iPhone 16 Pro" --output screen.png

simtop app "iPhone 16 Pro" install ./MyApp.app
simtop app "iPhone 16 Pro" launch com.example.MyApp
simtop app "iPhone 16 Pro" terminate com.example.MyApp
simtop app "iPhone 16 Pro" uninstall com.example.MyApp
simtop app "iPhone 16 Pro" logs --follow
simtop app "iPhone 16 Pro" open-url myapp://settings
```

## Device selectors

Commands that target a simulator accept either:

- An exact simulator UDID
- A unique simulator name, matched case-insensitively

Exact UDID matches take precedence over names.

If no simulator matches, simtop reports a device-not-found error. If multiple simulators have the same name, simtop reports an ambiguity error rather than choosing one automatically.

Convenience selectors such as `booted` are not supported.

## JSON output

All CLI commands support `--json`.

JSON mode provides:

- Schema-versioned output
- One JSON envelope per line
- Structured success results
- Structured error results
- Stable machine-readable error codes
- Deterministic process exit codes
- Streaming-compatible output for `watch` and followed logs
- No interactive prompts

Human-readable output is written in normal command mode. In JSON mode, command results are emitted as JSON and diagnostics are separated from the result stream.

## Safety behavior

simtop includes several safeguards:

- Delete prompts for confirmation in interactive human mode
- `--json` and `--no-input` do not prompt
- Ambiguous device names are rejected
- UDIDs, bundle identifiers, URLs, and Apple simulator identifiers are validated
- Arbitrary shell command passthrough is not supported
- Long-running builds and project runs can be cancelled
- Log and activity buffers are bounded
- Failures are surfaced with categorized error codes

## Typical use cases

- Monitoring a local set of iOS simulators
- Quickly booting and shutting down devices
- Installing and launching development builds
- Testing deep links and URL schemes
- Following simulator logs during development
- Capturing screenshots from a simulator
- Running repeatable simulator scripts
- Integrating simulator operations into CI
- Discovering and running Xcode projects from a terminal
- Managing multiple schemes, configurations, and simulator targets

## Intended users

- iOS developers
- macOS developers using Xcode
- QA engineers
- Build and release engineers
- Developers writing local automation
- CI and scripting workflows involving Apple simulators

## Requirements

- macOS 15 or later
- Xcode 16
- Apple CoreSimulator and Simulator.app
- Rust 1.74 or later when building from source

## Current project status

- Version: `0.1.0`
- Early development
- MIT licensed
- No published releases yet
- Current installation is from source
- The Projects build/run workflow is available in the interactive TUI
- There are no separate CLI project build/run commands currently

## Scope boundaries

simtop is not currently:

- A cross-platform simulator manager
- An Android emulator manager
- A physical-device management tool
- A cloud device farm
- A remote simulator service
- A complete automated testing framework
- A replacement for Xcode
- A replacement for the visual Simulator.app interface
- An app distribution or deployment platform
