# Ironjaw Requirements

**Document Version:** 1.0
**Date:** 2026-03-11
**Status:** Draft
**Source:** Requirements questionnaire (2026-03-11) + Gap analysis consolidated plan (steady-honking-kazoo.md)
**Scope:** Ironjaw backend, Web Dashboard, Dr. CLAW Android App, CLI client, TUI client

---

## Table of Contents

1. [Vision and Platform Strategy](#1-vision-and-platform-strategy)
2. [Web Dashboard](#2-web-dashboard)
3. [Dr. CLAW Android App](#3-dr-claw-android-app)
4. [Client Interfaces — CLI and TUI](#4-client-interfaces--cli-and-tui)
5. [Backend — Core Improvements](#5-backend--core-improvements)
6. [Backend — Security Hardening](#6-backend--security-hardening)
7. [Backend — Memory and Brain](#7-backend--memory-and-brain)
8. [Backend — Tool Expansion](#8-backend--tool-expansion)
9. [Backend — Smart Routing and Cost](#9-backend--smart-routing-and-cost)
10. [Backend — Interoperability](#10-backend--interoperability)
11. [Architecture and Infrastructure](#11-architecture-and-infrastructure)
12. [Known Issues and Stabilization](#12-known-issues-and-stabilization)
13. [Glossary](#glossary)
14. [Change Log](#change-log)

---

## 1. Vision and Platform Strategy

### BR-001: Feature Parity Across All Clients

**Priority:** P0
**Status:** Draft

**Description:**
All four client interfaces — Web Dashboard, Dr. CLAW Android, CLI, and TUI — must expose the same full set of capabilities. No feature may be exclusive to one client.

**Rationale:**
The user explicitly rejected a split-purpose model (admin console vs. mobile chat). Both products serve the same purpose; platform differences are in interaction model only, not in capability.

**Acceptance Criteria:**
- [ ] Every dashboard page has an equivalent Dr. CLAW screen or equivalent access path
- [ ] Every dashboard page has an equivalent CLI command or subcommand
- [ ] Every dashboard page has an equivalent TUI view
- [ ] A feature shipped in one client is backlogged for all other clients before being considered complete

**Source:** Questionnaire Section 1.1
**Dependencies:** None

---

### BR-002: Cross-Device Real-Time Synchronization

**Priority:** P0
**Status:** Draft

**Description:**
When a user sends a message, receives a response, executes a tool, or makes any state change on one connected client, all other simultaneously connected clients must reflect that change in real time without requiring a manual refresh.

**Rationale:**
The user wants seamless transitions between clients (e.g., start a conversation on Dr. CLAW, continue reviewing on the dashboard). Stale state across devices breaks this workflow.

**Acceptance Criteria:**
- [ ] A message sent from Dr. CLAW appears in the dashboard chat view within 500ms
- [ ] A message sent from the dashboard appears in Dr. CLAW within 500ms
- [ ] A tool execution initiated on one client shows live status on all other connected clients
- [ ] A new CC session detected by the backend appears on all connected clients without refresh
- [ ] Cross-device sync uses the existing WebSocket broadcast mechanism (no polling)

**Source:** Questionnaire Section 2.4, Section 6.1
**Dependencies:** BR-010 (single persistent WebSocket)

---

### BR-003: Single-User Now, Multi-User Architecture Ready

**Priority:** P1
**Status:** Draft

**Description:**
The system must support a single authenticated user in the immediate term. The auth model must be designed to support future extension to small-team or open-source multi-user scenarios without a full rewrite.

**Rationale:**
The user selected "single user now, possibly small team and/or open-source in the future." Building a fully single-user-only system now would require painful migration later.

**Acceptance Criteria:**
- [ ] Current implementation requires only one auth token and supports one user
- [ ] User-facing data (sessions, memories, preferences) is associated with a user identifier in the data model, even if only one user ID exists today
- [ ] Adding a second user account does not require schema migration for session or brain tables
- [ ] Auth layer is an isolated module that can be replaced without touching session, brain, or tool code

**Source:** Questionnaire Section 1.2
**Dependencies:** None

---

### BR-004: Passkey / WebAuthn Authentication

**Priority:** P1
**Status:** Draft

**Description:**
Within 12 months of initial stable release, the current static token auth must be replaceable with Passkey/WebAuthn authentication (hardware key or phone biometrics).

**Rationale:**
The user explicitly selected Passkey/WebAuthn as the 12-month auth target. A compromised static token today has no rotation path; biometric auth eliminates the token-sharing risk entirely.

**Acceptance Criteria:**
- [ ] Auth module interface is defined such that token-based and WebAuthn implementations are swappable
- [ ] A WebAuthn implementation plan exists before the 12-month mark
- [ ] Static token auth remains functional until WebAuthn is fully implemented and tested
- [ ] Token rotation from the dashboard (without server restart) is supported as an interim improvement

**Source:** Questionnaire Section 5.3
**Dependencies:** BR-003

---

### BR-005: No Paid APIs, Self-Hosted Only

**Priority:** P0
**Status:** Draft

**Description:**
Every technology component in the Ironjaw platform must be free and unlimited. No paid external APIs, no cloud services for core functionality, no quota-limited services.

**Rationale:**
Hard constraint confirmed explicitly in the questionnaire. The entire workstation/docker-host infrastructure is built around this principle.

**Acceptance Criteria:**
- [ ] All new dependencies have MIT, Apache-2.0, or equivalent permissive licenses
- [ ] No new integration requires an API key from a paid service
- [ ] All LLM inference runs via llama-server on workstation
- [ ] All search runs via SearXNG/Perplexica on docker-host
- [ ] CI/CD pipeline uses Gitea Actions on docker-host (self-hosted, no GitHub Actions minutes quota)

**Source:** Questionnaire Section 6.3
**Dependencies:** None

---

## 2. Web Dashboard

### BR-010: Single Persistent WebSocket Architecture

**Priority:** P0
**Status:** Draft

**Description:**
The dashboard must use a single persistent WebSocket connection for all real-time data. REST polling is not an acceptable substitute for any dashboard feature.

**Rationale:**
The user explicitly selected the single-WebSocket model and confirmed that all features listed in Section 2.3 require WebSocket push, not polling.

**Acceptance Criteria:**
- [ ] Dashboard opens exactly one WebSocket connection on load and reuses it for all operations
- [ ] Chat streaming, tool status, infra metrics, CC session detection, HITL dialogs, device status, and schedule status all arrive via this single WebSocket
- [ ] Reconnection on disconnect is automatic with user-visible retry status
- [ ] No dashboard feature falls back to HTTP polling

**Source:** Questionnaire Section 2.3
**Dependencies:** None

---

### BR-011: All Eleven Dashboard Pages — Must Have

**Priority:** P0
**Status:** Draft

**Description:**
The dashboard must implement all eleven pages: Chat, CC Sessions, Models, Tools, Brain, Schedules, Devices, Metrics, Chronicle, Settings, and Login. No pages may be deferred to a v2.

**Rationale:**
The user explicitly classified all eleven pages as "Must Have" and stated "nothing to cut — all 11 pages in v1."

**Acceptance Criteria:**
- [ ] Chat page is functional and accepts/displays messages
- [ ] CC Sessions page lists, searches, and displays session transcripts
- [ ] Models page displays available LLM models and their status
- [ ] Tools page lists, executes, and displays tool history
- [ ] Brain page supports full memory CRUD
- [ ] Schedules page lists and manages cron schedules and workflows
- [ ] Devices page shows connected device status
- [ ] Metrics page displays system and LLM metrics with charts
- [ ] Chronicle page surfaces CC Chronicle session data
- [ ] Settings page allows management of Ironjaw configuration
- [ ] Login page enforces authentication before any other page is accessible

**Source:** Questionnaire Section 2.2, Section 6.2
**Dependencies:** BR-010

---

### BR-012: OpenClaw Dashboard Visual Style with Light/Dark Toggle

**Priority:** P0
**Status:** Draft

**Description:**
The dashboard must follow the existing OpenClaw gateway dashboard visual style. A light/dark theme toggle must be available, with the preference persisted in browser localStorage.

**Rationale:**
The user wants familiarity ("keep existing OpenClaw gateway dashboard style") combined with the flexibility of a theme toggle.

**Acceptance Criteria:**
- [ ] Visual layout and component style matches OpenClaw gateway dashboard
- [ ] A toggle control (button or switch) is accessible from all pages
- [ ] Switching themes applies immediately without a page reload
- [ ] Selected theme is stored in `localStorage` under a stable key
- [ ] On fresh load, the theme persisted in localStorage is applied before any content renders (no flash of wrong theme)

**Source:** Questionnaire Section 2.1
**Dependencies:** BR-011

---

### BR-013: Full Mobile Responsiveness — S22 Ultra

**Priority:** P0
**Status:** Draft

**Description:**
All eleven dashboard pages must render correctly and be fully usable on a Samsung Galaxy S22 Ultra screen (6.8-inch, 1440x3088 at 500ppi, typically viewed in portrait mode on a mobile browser).

**Rationale:**
The user selected "Regularly — must work well on S22 Ultra screen. Full mobile-responsive layout required."

**Acceptance Criteria:**
- [ ] No page requires horizontal scrolling on a viewport of 390px width (S22 Ultra portrait, scaled)
- [ ] All interactive elements (buttons, inputs, nav) have tap targets of at least 44x44 CSS pixels
- [ ] Navigation between pages works via touch-friendly controls
- [ ] Chat input and message list are usable without a physical keyboard (soft keyboard aware)
- [ ] Tables and data-dense views collapse or reflow to single-column on narrow viewports
- [ ] Dashboard is accessible via DuckDNS/Caddy from the phone browser

**Source:** Questionnaire Section 2.5
**Dependencies:** BR-011

---

### BR-014: Dashboard Performance Targets

**Priority:** P0
**Status:** Draft

**Description:**
The dashboard must meet the following performance thresholds: initial page load under 250ms over LAN, client-side route navigation instant, and the Metrics and CC Sessions pages display data immediately on navigation.

**Rationale:**
The user specified tighter thresholds than the suggested defaults. "Instant" means no visible loading state on data that has already been received via WebSocket push.

**Acceptance Criteria:**
- [ ] Time from browser navigation to first meaningful paint on LAN is under 250ms (measured via browser dev tools)
- [ ] Navigating between already-loaded pages takes under 100ms (client-side routing)
- [ ] Metrics page data is visible within 100ms of page activation (pre-loaded via WebSocket)
- [ ] CC Sessions list with 100+ sessions is displayed within 100ms of page activation
- [ ] First streaming token appears in chat within 200ms of the LLM beginning its response (LLM-bound, not transport-bound)

**Source:** Questionnaire Section 2.6
**Dependencies:** BR-010, BR-011

---

### BR-015: Chat Page — Full Feature Set

**Priority:** P0
**Status:** Draft

**Description:**
The Chat page must support Markdown rendering with syntax highlighting and LaTeX math, per-message model switching, three user-configurable streaming modes, abort via button and Escape key, and HITL dialogs in both modal and inline forms.

**Rationale:**
Every item in Section 2.7 was checked as required. Chat is the highest-priority page (ranked 1 in the capability priority table).

**Acceptance Criteria:**
- [ ] Messages render full Markdown: bold, italic, code blocks, lists, tables, blockquotes
- [ ] Code blocks have syntax highlighting for all major languages
- [ ] LaTeX expressions (inline `$...$` and block `$$...$$`) render as typeset math
- [ ] A session list is accessible from the chat sidebar
- [ ] New session button is visible in the chat interface
- [ ] Sessions can be renamed inline from the chat header
- [ ] Sessions can be deleted from the chat interface
- [ ] A model picker on the chat input allows switching model for each individual message (not locked to session)
- [ ] Streaming mode is user-configurable per session: token-by-token, buffered chunks, or show-when-complete
- [ ] An abort button is visible during streaming and cancels the active LLM response
- [ ] Pressing Escape during streaming triggers the same abort behavior as the button
- [ ] Blocking HITL approval requests appear as modal overlays
- [ ] Non-blocking HITL notifications appear as inline messages in the chat stream

**Source:** Questionnaire Section 2.7
**Dependencies:** BR-010, BR-011

---

### BR-016: Tools Page — Full Feature Set

**Priority:** P0
**Status:** Draft

**Description:**
The Tools page must list all registered tools with metadata, support manual execution with parameter input, show execution history, stream output inline, and indicate tools currently executing.

**Rationale:**
All items in Section 2.8 were checked as required.

**Acceptance Criteria:**
- [ ] All registered tools are listed with name, description, and permission tier
- [ ] Clicking a tool opens a form with typed parameter inputs matching the tool's JSON schema
- [ ] Submitting the form executes the tool and displays output inline on the page
- [ ] Tool output streams in real-time for long-running tools
- [ ] A history section shows recent tool executions with timestamps and results
- [ ] Tools currently executing are visually distinguished (spinner, badge, or similar indicator)
- [ ] HITL approval dialogs appear when a Confirm-tier tool is manually executed

**Source:** Questionnaire Section 2.8
**Dependencies:** BR-010, BR-011

---

### BR-017: Metrics Page — Full Feature Set

**Priority:** P0
**Status:** Draft

**Description:**
The Metrics page must display all current Ironjaw metrics plus workstation system metrics, llama-server token throughput, per-model usage, circuit breaker states, WebSocket connection history, and tool latency histograms. All data must be displayed in both chart and numeric table formats.

**Rationale:**
All items in Section 2.9 were checked as required. The user wants both charts and numbers.

**Acceptance Criteria:**
- [ ] Ironjaw request counts per method are displayed
- [ ] Error counts per method are displayed
- [ ] Active WebSocket connection count is displayed
- [ ] LLM token usage totals are displayed
- [ ] workstation CPU, RAM, and disk utilization are displayed
- [ ] llama-server token throughput (tokens/second) is displayed
- [ ] Per-model usage breakdown shows token counts and request counts per model
- [ ] Circuit breaker states (open/closed/half-open) for each monitored service are displayed
- [ ] WebSocket connection history (connects, disconnects over time) is displayed
- [ ] Tool execution latency histogram is displayed
- [ ] All metrics that change over time are shown as time-series charts
- [ ] All metrics are also available in numeric table format on the same page

**Source:** Questionnaire Section 2.9
**Dependencies:** BR-010, BR-011

---

### BR-018: CC Sessions Page — Full Feature Set

**Priority:** P0
**Status:** Draft

**Description:**
The CC Sessions page must provide paginated listing, full-text and semantic search, inline transcript viewing, filtering by machine, date range filtering, context percentage display, auto-generated session titles, and links to Chronicle entries.

**Rationale:**
All items in Section 2.10 were checked as must-have.

**Acceptance Criteria:**
- [ ] Sessions are listed in paginated form (server-side pagination for 100+ sessions)
- [ ] A search input searches session content and titles
- [ ] Selecting a session displays the full transcript inline without a page navigation
- [ ] A machine filter allows showing only workstation sessions, only docker-host sessions, or both
- [ ] A date range picker filters sessions by creation date
- [ ] Each session entry displays context percentage (0-100) and compaction count
- [ ] Each session entry displays the auto-generated title from cc-bridge
- [ ] Sessions with Chronicle entries display a link to the corresponding Chronicle record

**Source:** Questionnaire Section 2.10
**Dependencies:** BR-010, BR-011

---

### BR-019: Brain Page — Full Feature Set

**Priority:** P0
**Status:** Draft

**Description:**
The Brain page must support semantic memory search, manual add, individual delete, layer visualization, bulk import, full export, and memory statistics display.

**Rationale:**
All items in Section 2.11 were checked as must-have. Brain is ranked P1 (tied with Chat) in the capability priority table.

**Acceptance Criteria:**
- [ ] A search input triggers semantic memory search via `brain.recall` and displays ranked results
- [ ] A form allows manually adding a new memory via `brain.remember`
- [ ] Each memory entry has a delete button that calls `brain.forget`
- [ ] Memory layers (identity vs. long-term) are visually distinguished
- [ ] A bulk import control accepts a file (JSON or JSONL) and calls `brain.import`
- [ ] An export button downloads all memories as a JSON file via `export.brain`
- [ ] A statistics panel shows total memory count, storage size on disk, oldest memory date, and newest memory date

**Source:** Questionnaire Section 2.11
**Dependencies:** BR-010, BR-011

---

### BR-020: Remote Access via DuckDNS/Caddy

**Priority:** P0
**Status:** Draft

**Description:**
The dashboard must be accessible from any device (phone browser, remote machine) via the existing DuckDNS/Caddy TLS termination path, using the same auth token as Dr. CLAW.

**Rationale:**
The user confirmed "Yes — same as Dr. CLAW, accessible via phone browser or remote machine" and noted both products are full-featured and should support seamless transitions.

**Acceptance Criteria:**
- [ ] Dashboard is reachable at the existing DuckDNS domain over HTTPS (port 443 via Caddy)
- [ ] Caddy proxies dashboard WebSocket connections to Ironjaw :18794 correctly
- [ ] Auth token stored in localStorage authenticates the same way as the existing WS auth
- [ ] No CORS or CSP errors when accessing the dashboard from a non-localhost origin

**Source:** Questionnaire Section 2.4
**Dependencies:** BR-010, BR-011

---

## 3. Dr. CLAW Android App

### BR-030: Dr. CLAW Screen Inventory — All Screens Required

**Priority:** P0
**Status:** Draft

**Description:**
Dr. CLAW must implement all current screens plus five additional screens for feature parity: Brain/Memory, Schedules/Workflows, Chronicle, Devices, and Metrics.

**Rationale:**
All existing screens were classified Must Have in Section 3.1. Five new screens were explicitly requested for dashboard parity.

**Acceptance Criteria:**
- [ ] Chat screen is functional with streaming, HITL, and session management
- [ ] Voice screen supports STT/TTS (see BR-034)
- [ ] CC Sessions screen lists, searches, and displays session transcripts
- [ ] Attached Session screen displays the currently attached CC session
- [ ] Projects screen lists available projects
- [ ] Model Manager screen shows available models and allows selection
- [ ] Infra/Status screen shows workstation/docker-host infrastructure health
- [ ] Settings screen exposes all configurable app preferences
- [ ] Camera/Vision screen supports all vision features (see BR-035)
- [ ] Search screen allows querying SearXNG/Perplexica
- [ ] Brain/Memory screen has feature parity with the dashboard Brain page (BR-019)
- [ ] Schedules/Workflows screen has feature parity with the dashboard Schedules page
- [ ] Chronicle screen has feature parity with the dashboard Chronicle page
- [ ] Devices screen has feature parity with the dashboard Devices page
- [ ] Metrics screen has feature parity with the dashboard Metrics page (BR-017)

**Source:** Questionnaire Section 3.1
**Dependencies:** BR-001

---

### BR-031: Push Notifications — All Event Types with Per-Type Toggle

**Priority:** P0
**Status:** Draft

**Description:**
Dr. CLAW must receive push notifications via ntfy for all nine defined event categories. Each event category must have an individual enable/disable toggle in Settings. HITL notifications must be actionable directly from the notification shade.

**Rationale:**
All nine event types were confirmed as requiring notifications. Actionable HITL notifications were explicitly required in Section 3.2.

**Acceptance Criteria:**
- [ ] HITL permission requests trigger urgent-priority ntfy notifications
- [ ] Proactive engine task completions trigger normal-priority notifications
- [ ] Proactive engine task failures trigger urgent-priority notifications
- [ ] Workflow step approval requests trigger urgent-priority notifications
- [ ] LLM response completions (while app is backgrounded) trigger normal-priority notifications
- [ ] Ironjaw service crash/restart events trigger urgent-priority notifications
- [ ] New CC session detections trigger silent notifications
- [ ] Circuit breaker open events trigger normal-priority notifications
- [ ] workstation/docker-host metrics threshold breaches trigger normal-priority notifications
- [ ] Settings screen provides individual enable/disable toggles for each of the nine event categories
- [ ] Tapping a HITL notification opens the approval dialog directly, not the app home screen
- [ ] HITL approval and denial actions are available as notification action buttons in the notification shade without opening the app

**Source:** Questionnaire Section 3.2
**Dependencies:** BR-001

---

### BR-032: Offline Mode — Read-Only Cache and Draft Queuing

**Priority:** P0
**Status:** Draft

**Description:**
When Ironjaw is unreachable, Dr. CLAW must display last-known data from local Room DB in read-only mode and allow composing messages that are queued and sent automatically on reconnect. Reconnect must be automatic with user-visible retry status.

**Rationale:**
The user selected both "read-only local cache" and "full offline chat draft" in Section 3.3, and "automatic with user-visible retry status" for reconnect behavior.

**Acceptance Criteria:**
- [ ] When WebSocket connection is lost, app enters offline mode without crashing
- [ ] Offline mode displays a persistent, visible connection status indicator
- [ ] Previously loaded session history remains readable in offline mode
- [ ] Previously loaded metrics, CC sessions, and brain memories are viewable in offline mode (last-known state)
- [ ] The chat input remains available in offline mode
- [ ] Messages composed offline are stored locally in Room DB with a "pending" status indicator
- [ ] Pending messages are transmitted automatically when the connection is restored
- [ ] Reconnect attempts use automatic exponential backoff
- [ ] The retry status (attempt count, next retry time) is visible to the user during reconnect

**Source:** Questionnaire Section 3.3
**Dependencies:** BR-001, BR-010

---

### BR-033: Android Target — API 35 Minimum

**Priority:** P0
**Status:** Draft

**Description:**
Dr. CLAW must target Android API level 35 (Android 15) as both the minimum SDK and target SDK.

**Rationale:**
The user explicitly selected API 35 in Section 3.8 to enable cleaner code and access to the latest Android APIs.

**Acceptance Criteria:**
- [ ] `minSdk = 35` in the app-level `build.gradle.kts`
- [ ] `compileSdk = 35` and `targetSdk = 35` in the app-level `build.gradle.kts`
- [ ] No deprecated API shims are included for Android versions below 35
- [ ] App builds and runs correctly on Android 15 (API 35)

**Source:** Questionnaire Section 3.8
**Dependencies:** None

---

### BR-034: Voice — Core Feature with PTT, VAD, Optional TTS, and Driving Mode

**Priority:** P0
**Status:** Draft

**Description:**
Voice interaction is a daily-use core feature. Dr. CLAW must support both push-to-talk and voice activity detection (switchable in settings), per-message optional TTS playback, and a dedicated hands-free driving mode with continuous listening and automatic TTS.

**Rationale:**
The user selected "core feature — daily use, must be excellent" and confirmed all interaction models in Section 3.4.

**Acceptance Criteria:**
- [ ] Push-to-talk mode: holding the microphone button records audio; releasing sends for STT transcription
- [ ] Voice activity detection (VAD) mode: app listens continuously, detects speech start and end automatically
- [ ] A toggle in Settings switches between PTT and VAD modes
- [ ] Each assistant message has a per-message TTS playback button
- [ ] TTS does not play automatically unless driving mode is active
- [ ] Driving mode is accessible from a dedicated screen or persistent quick action
- [ ] In driving mode, the app continuously listens (VAD) and reads every assistant response aloud (auto-TTS)
- [ ] Driving mode requires no screen interaction to use after activation
- [ ] Voice STT is powered by the Whisper model via llama-server on workstation (no external STT service)
- [ ] Voice TTS is powered by the Orpheus model via llama-server on workstation (no external TTS service)

**Source:** Questionnaire Section 3.4
**Dependencies:** BR-001

---

### BR-035: Vision — All Features

**Priority:** P0
**Status:** Draft

**Description:**
Dr. CLAW must support photo analysis, screenshot/clipboard image analysis, barcode and QR code scanning, real-time camera feed to LLM, and file attachment (PDF and gallery images) for analysis.

**Rationale:**
All vision features were checked as required in Section 3.5. Vision is powered by qwen3-vl:30b-a3b on workstation.

**Acceptance Criteria:**
- [ ] User can take a photo in-app and send it to the LLM with a question
- [ ] User can paste an image from clipboard and send it for analysis
- [ ] User can select an image from the gallery and attach it to a message
- [ ] User can attach a PDF file and send it for analysis
- [ ] Barcode and QR code scanning is available as a dedicated mode in the camera screen
- [ ] Real-time camera feed can be streamed to the vision LLM with a prompt
- [ ] Vision inference uses qwen3-vl:30b-a3b via llama-server on workstation
- [ ] Images are validated against the configured maximum size (10MB default) before upload

**Source:** Questionnaire Section 3.5
**Dependencies:** BR-001

---

### BR-036: Android Widgets and Quick Actions

**Priority:** P0
**Status:** Draft

**Description:**
Dr. CLAW must provide two home screen widgets (status/preview and quick-compose), two quick tile actions (PTT toggle and new chat session), and a polished floating chat bubble.

**Rationale:**
All widget and quick-action types were selected in Section 3.6.

**Acceptance Criteria:**
- [ ] A glanceable status widget shows connection status and the last message preview on the home screen
- [ ] A quick-compose widget provides a text input on the home screen that sends directly to Ironjaw
- [ ] A notification shade quick tile toggles push-to-talk recording
- [ ] A notification shade quick tile opens a new chat session
- [ ] The floating bubble (BubbleService) is polished and functional over other apps
- [ ] The floating bubble allows reading and sending chat messages without switching apps
- [ ] All widgets update their displayed data via the existing WebSocket connection (no separate polling)

**Source:** Questionnaire Section 3.6
**Dependencies:** BR-032

---

### BR-037: Battery — Configurable Background Behavior

**Priority:** P1
**Status:** Draft

**Description:**
The app must expose configurable settings for WebSocket keep-alive timeout and background disconnect policy, giving the user control over the battery vs. connectivity tradeoff.

**Rationale:**
The user selected "moderate concern" and specified that the behavior should be user-configurable rather than fixed.

**Acceptance Criteria:**
- [ ] Settings screen exposes a configurable WebSocket inactivity timeout (e.g., disconnect after N minutes in background)
- [ ] Settings screen exposes a background disconnect policy toggle (always connected vs. disconnect on screen off)
- [ ] The selected policy is persisted in app preferences and survives app restart
- [ ] Changing these settings takes effect on the next connection without requiring an app restart

**Source:** Questionnaire Section 3.7
**Dependencies:** BR-032

---

### BR-038: Tablet — Layouts Must Not Break

**Priority:** P2
**Status:** Draft

**Description:**
Dr. CLAW layouts must not break on tablet-sized screens. A two-pane master-detail layout is not required.

**Rationale:**
The user selected "maybe — design should not break on large screens but no two-pane layout needed" in Section 3.9.

**Acceptance Criteria:**
- [ ] All screens render without visible layout corruption on a 10-inch (1280x800) tablet viewport
- [ ] No text overflows its container, no buttons are off-screen, no scrolling is broken on tablet
- [ ] The existing single-pane layout scales acceptably on larger screens (no mandatory two-pane refactor)

**Source:** Questionnaire Section 3.9
**Dependencies:** None

---

### BR-039: No Wear OS Support

**Priority:** P0
**Status:** Draft

**Description:**
Dr. CLAW must not include any Wear OS companion module or Galaxy Watch integration.

**Rationale:**
The user explicitly selected "No — skip entirely" in Section 3.10.

**Acceptance Criteria:**
- [ ] No `wearApp` module exists in the project
- [ ] No Wear OS dependencies are present in any `build.gradle.kts`
- [ ] App does not prompt the user to install a watch companion

**Source:** Questionnaire Section 3.10
**Dependencies:** None

---

### BR-040: No Third-Party Messaging Channels

**Priority:** P0
**Status:** Draft

**Description:**
Ironjaw must not implement Telegram, Signal, Slack, Discord, SMS relay, or email (IMAP/SMTP) integrations.

**Rationale:**
All multi-channel messaging options were rated "None" in Section 4.4. Dr. CLAW + Dashboard + CLI/TUI is the complete channel set.

**Acceptance Criteria:**
- [ ] No Telegram bot, Signal bot, Slack app, or Discord bot code exists in any Ironjaw crate
- [ ] No IMAP/SMTP email integration exists
- [ ] No SMS relay integration exists

**Source:** Questionnaire Section 4.4
**Dependencies:** None

---

## 4. Client Interfaces — CLI and TUI

### BR-050: CLI Client for Scripting and Claude Code Integration

**Priority:** P0
**Status:** Draft

**Description:**
A command-line client must be available that exposes all Ironjaw capabilities as shell-executable commands, suitable for use in scripts and for integration with Claude Code workflows.

**Rationale:**
The user explicitly requested a CLI client for "scriptability and integration with other CLIs like Claude Code." This was stated alongside TUI and web as a full-featured interface.

**Acceptance Criteria:**
- [ ] A CLI binary (e.g., `ironjaw`) is available with subcommands for all major operations
- [ ] `ironjaw chat "<message>"` sends a message and outputs the response to stdout
- [ ] `ironjaw chat --stream "<message>"` streams tokens to stdout
- [ ] `ironjaw session list` lists sessions
- [ ] `ironjaw tools list` lists registered tools
- [ ] `ironjaw tools exec <name> [args]` executes a tool
- [ ] `ironjaw brain recall "<query>"` performs a semantic memory search
- [ ] `ironjaw mcp-server` runs Ironjaw as an MCP server on stdio (see BR-072)
- [ ] Exit codes follow Unix conventions (0 = success, non-zero = error)
- [ ] Output format is configurable: human-readable (default) and JSON (`--json` flag)

**Source:** Questionnaire Section 2.12
**Dependencies:** BR-001

---

### BR-051: TUI Client as Daily-Driver Terminal Interface

**Priority:** P0
**Status:** Draft

**Description:**
A terminal user interface (TUI) client must be available that provides an interactive, full-featured experience inside a terminal emulator. The TUI is intended as a regular daily-driver interface, not a minimal prototype.

**Rationale:**
The user selected "Yes — I would use a TUI regularly. Build it." and confirmed dashboard scope is not reduced because the TUI exists.

**Acceptance Criteria:**
- [ ] A TUI binary or `ironjaw tui` subcommand launches an interactive terminal interface
- [ ] The TUI provides a navigable view of: Chat, CC Sessions, Brain, Tools, Metrics, and Settings
- [ ] Chat in the TUI supports message streaming with visible token-by-token output
- [ ] The TUI handles HITL approval dialogs interactively (keyboard-driven confirm/deny)
- [ ] Navigation uses standard keyboard conventions (arrow keys, tab, vim-style hjkl where applicable)
- [ ] The TUI works over SSH to workstation without requiring a GUI
- [ ] The TUI connects to Ironjaw via the same WebSocket protocol as all other clients

**Source:** Questionnaire Section 2.12, Section 4.6
**Dependencies:** BR-001, BR-010

---

## 5. Backend — Core Improvements

### BR-060: Block-Aware Session Compaction

**Priority:** P0
**Status:** Draft

**Description:**
When a session's context token count exceeds the configured limit, Ironjaw must compact the session by summarizing older messages while preserving tool-use/tool-result pairs as atomic units that cannot be split.

**Rationale:**
Current Ironjaw has no compaction. Long sessions will exceed context limits and fail. Tool-use/tool-result pair integrity is required by the Anthropic API.

**Acceptance Criteria:**
- [ ] Compaction is triggered automatically when context tokens exceed `context_token_limit`
- [ ] The most recent N messages (configurable, default 10) are preserved verbatim
- [ ] A middle tier of messages is summarized via an LLM call with a "summarize this conversation" prompt
- [ ] The oldest messages are discarded after summarization
- [ ] Tool-use messages and their paired tool-result messages are never separated across compaction tiers
- [ ] The session's `compaction_count` is incremented after each compaction
- [ ] A `CompactionResult` log entry records removed, summarized, and tokens-saved counts

**Source:** Gap plan Sprint 2, item #6
**Dependencies:** BR-061

---

### BR-061: Session Message History Repair

**Priority:** P0
**Status:** Draft

**Description:**
Before every LLM call, the message history must be validated and repaired through a seven-phase pipeline that removes empty messages, fixes orphaned tool results, merges consecutive same-role messages, ensures proper user/assistant alternation, validates tool-use/result pair adjacency, trims to context limit, and ensures the first non-system message is from the user role.

**Rationale:**
Malformed message history is the most common cause of LLM API errors. Session repair prevents cascading failures from accumulated message state corruption. This is a prerequisite for compaction (BR-060).

**Acceptance Criteria:**
- [ ] `repair_history()` is called in the `chat.send` handler after loading messages and before calling `llm_router.chat()`
- [ ] Empty-content messages are removed
- [ ] Orphaned tool-result messages (no preceding tool-use) are removed
- [ ] Consecutive messages with the same role are merged with a double newline separator
- [ ] Non-alternating sequences are repaired by inserting synthetic bridging messages
- [ ] Tool-use and tool-result pairs are validated as adjacent
- [ ] History is trimmed to fit within the configured context limit
- [ ] The first non-system message in the history is always from the user role
- [ ] A `RepairReport` is logged for every invocation (even if no repairs were needed)
- [ ] Unit tests cover each phase independently and all phases combined

**Source:** Gap plan Sprint 1, item #5
**Dependencies:** None

---

### BR-062: LLM Response Cache

**Priority:** P1
**Status:** Draft

**Description:**
Identical LLM requests must be served from an in-memory LRU cache with a configurable TTL rather than forwarded to the LLM provider.

**Rationale:**
Repeated identical requests (e.g., idempotent tool descriptions, common queries) waste llama-server inference time and add unnecessary latency.

**Acceptance Criteria:**
- [ ] Cache key is a SHA-256 hash of the concatenated message roles and contents
- [ ] Cache capacity and TTL are configurable in `ironjaw.toml`
- [ ] Cache hits bypass the LLM provider and return the stored response immediately
- [ ] After a stream completes, the full response is stored in the cache
- [ ] Cache entries expire after the configured TTL
- [ ] Entries are evicted LRU-style when capacity is exceeded
- [ ] Cache hit/miss counts are exposed via the `infra.metrics` method

**Source:** Gap plan Sprint 2, item #10
**Dependencies:** None

---

### BR-063: Config Hot-Reload

**Priority:** P1
**Status:** Draft

**Description:**
Changes to `ironjaw.toml` must be detected and applied without restarting the Ironjaw process, for the sections that support hot-reload.

**Rationale:**
Restarting Ironjaw to change a security threshold or brain parameter disconnects all clients. Hot-reload eliminates this disruption.

**Acceptance Criteria:**
- [ ] Ironjaw watches `ironjaw.toml` for file system changes using the `notify` crate
- [ ] Changes are debounced for 1 second to ignore rapid saves
- [ ] The following sections are hot-reloadable: `[security]`, `[brain]`, `[tools]`, `[logging]`
- [ ] The following sections are NOT hot-reloadable and require a restart: `[server]` (port), `[auth]` (token), `[llm]` (providers)
- [ ] On a valid config change, running connections are not terminated
- [ ] On an invalid config change (parse error), the existing config remains active and an error is logged

**Source:** Gap plan Sprint 2, item #14
**Dependencies:** None

---

### BR-064: Tool Nudging — Outcome Gate

**Priority:** P1
**Status:** Draft

**Description:**
When the LLM produces a text-only response (no tool call) in a context where a tool call was clearly expected, Ironjaw must append a nudge message and re-invoke the LLM, up to two times per turn.

**Rationale:**
LLMs occasionally describe what they will do rather than doing it. The nudge mechanism catches this and forces tool execution without user intervention.

**Acceptance Criteria:**
- [ ] After receiving an LLM response with no tool call and no structured outcome, a nudge message is appended
- [ ] The nudge reads approximately: "You described what to do but didn't take action. Please use the appropriate tool to complete the task."
- [ ] A maximum of 2 nudges are applied per user turn
- [ ] Nudge count resets when a tool call is successfully made or when the user sends a new message
- [ ] Nudge behavior is configurable (enable/disable) in `[security]` config section

**Source:** Gap plan Sprint 2, item #7
**Dependencies:** None

---

### BR-065: Parallel Tool Execution

**Priority:** P1
**Status:** Draft

**Description:**
When an LLM response contains multiple tool calls, Auto-tier tools must be executed concurrently rather than sequentially.

**Rationale:**
Sequential tool execution for independent tools wastes wall-clock time proportional to the number of tools called. Concurrency reduces multi-tool response time significantly.

**Acceptance Criteria:**
- [ ] When an LLM response contains 2+ tool calls that are all Auto-tier, they are executed concurrently via `futures::future::join_all`
- [ ] Results from concurrent executions are collected and returned to the LLM in the correct order
- [ ] Confirm-tier tools are always executed sequentially (HITL approval is per-tool and cannot be parallelized)
- [ ] A mix of Auto and Confirm tier tools in one response executes Auto tools concurrently first, then handles Confirm tools sequentially

**Source:** Gap plan Sprint 2, item #18
**Dependencies:** None

---

### BR-066: Silent Memory Flush Before Compaction

**Priority:** P1
**Status:** Draft

**Description:**
Immediately before compacting a session, Ironjaw must invoke the LLM to extract important facts, decisions, and preferences from the messages about to be compacted and store them in the brain.

**Rationale:**
Compaction discards messages. Without a memory flush, important context is permanently lost. This bridges compaction and memory.

**Acceptance Criteria:**
- [ ] Before executing compaction (BR-060), a "memory flush" LLM call is made with the messages scheduled for summarization
- [ ] The flush prompt asks the LLM to extract the 5 most important facts, decisions, and preferences
- [ ] Each extracted item is stored in the brain via `brain.remember`
- [ ] The memory flush occurs before message deletion, ensuring no data loss if the flush fails
- [ ] Flush results are logged alongside the CompactionResult

**Source:** Gap plan Sprint 2, item #19
**Dependencies:** BR-060

---

### BR-067: Merkle Hash-Chain Audit Log

**Priority:** P1
**Status:** Draft

**Description:**
The audit log must be tamper-evident through a hash chain where each entry includes the SHA-256 hash of the previous entry's fields.

**Rationale:**
A plain append-only audit log can be silently modified. The hash chain makes any tampering detectable.

**Acceptance Criteria:**
- [ ] Each `AuditEntry` stores a `prev_hash` field (SHA-256 of the previous entry's hash + action_type + method + timestamp)
- [ ] The audit database schema is migrated to add the `prev_hash TEXT` column
- [ ] A `verify_chain()` function walks all audit entries, recomputes hashes, and returns a validity flag and row count
- [ ] The Settings page exposes a "Verify audit chain" action and displays the result
- [ ] Chain verification detects any tampered entry

**Source:** Gap plan Sprint 2, item #8
**Dependencies:** None

---

### BR-068: Cron CRUD Agent Tools

**Priority:** P1
**Status:** Draft

**Description:**
Cron schedule management must be available as agent-callable tools, allowing the LLM to create, list, toggle, and manage schedules without requiring user navigation to the Schedules page.

**Rationale:**
The agent should be able to manage its own scheduled tasks. Without tool access, every schedule change requires manual user action.

**Acceptance Criteria:**
- [ ] `schedule_list` tool (Auto tier) returns all schedules with cron expression, enabled state, and last-run time
- [ ] `schedule_create` tool (Confirm tier) creates a new schedule given a cron expression and action
- [ ] `schedule_toggle` tool (Notify tier) enables or disables a schedule by ID
- [ ] All three tools are registered in `ToolRegistry` when `[proactive]` is enabled
- [ ] Each tool has unit tests with an in-memory `ScheduleStore`

**Source:** Gap plan Sprint 2, item #9
**Dependencies:** None

---

### BR-069: Event Publish Tool

**Priority:** P1
**Status:** Draft

**Description:**
Agents must be able to publish named events that trigger proactive engine workflows, allowing agent-initiated automation chains.

**Rationale:**
Without an event publish mechanism, agents cannot initiate multi-step workflows. This enables emergent automation where one agent's output triggers another process.

**Acceptance Criteria:**
- [ ] `event_publish` tool (Notify tier) accepts an `event_name` string and a `payload` object
- [ ] Published events are broadcast to the `ProactiveEngine` via its event channel
- [ ] The `ProactiveEngine` can be configured to trigger specific workflows on specific event names
- [ ] The tool is registered in `ToolRegistry` when `[proactive]` is enabled

**Source:** Gap plan Sprint 2, item #11
**Dependencies:** None

---

### BR-070: JSON Repair for Malformed Tool Call Arguments

**Priority:** P0
**Status:** Draft

**Description:**
Before passing LLM-generated tool call arguments to a tool's execute function, Ironjaw must attempt to repair common JSON malformation patterns.

**Rationale:**
LLMs frequently produce tool call arguments with trailing commas, JavaScript-style comments, unbalanced braces, or Markdown code fences. Without repair, these cause tool call failures that interrupt the agent loop unnecessarily.

**Acceptance Criteria:**
- [ ] Before calling `tool.execute()`, the `params` JSON is passed through a repair function
- [ ] The repair function handles: `// and /* */` comments, trailing commas before `}` and `]`, unbalanced braces/brackets, and Markdown code fences (` ```json ... ``` `)
- [ ] If repair produces valid JSON, execution proceeds with the repaired params
- [ ] If repair still fails, the original parse error is returned to the caller
- [ ] Unit tests cover each repair case individually

**Source:** Gap plan Sprint 2, item #17
**Dependencies:** None

---

### BR-071: Task Board — Shared Agent Task Queue

**Priority:** P1
**Status:** Draft

**Description:**
A shared task queue must exist that allows agents to post tasks, claim them, complete them, and list pending/claimed tasks. This enables multi-agent coordination.

**Rationale:**
Without a task board, agents cannot hand off work or coordinate on multi-step parallel tasks. This is foundational for the planned sub-agent delegation capability.

**Acceptance Criteria:**
- [ ] `task_post` tool (Notify tier) creates a new task with title and description
- [ ] `task_claim` tool (Notify tier) atomically claims an unclaimed task for an agent identifier
- [ ] `task_complete` tool (Notify tier) marks a claimed task as done
- [ ] `task_list` tool (Auto tier) returns tasks filtered by optional status (pending/claimed/done)
- [ ] Task state is persisted in SQLite
- [ ] Claim operation is atomic — concurrent claims for the same task only succeed for one agent

**Source:** Gap plan Sprint 2, item #15
**Dependencies:** None

---

## 6. Backend — Security Hardening

### BR-080: Tool Loop Guard

**Priority:** P0
**Status:** Draft

**Description:**
Ironjaw must detect when an agent is stuck in a tool-call loop and terminate the loop before it exhausts resources or causes runaway LLM costs.

**Rationale:**
Agents can enter infinite or near-infinite tool loops when a tool always returns the same result or when the LLM misinterprets repeated failure. Without a guard, this burns llama-server compute indefinitely.

**Acceptance Criteria:**
- [ ] A `LoopGuard` struct is maintained per session in `ConnectionContext`
- [ ] Each tool call is fingerprinted via SHA-256 of tool name and sorted argument keys, truncated to u64
- [ ] If the same fingerprint appears 3+ times within a sliding window of 10 calls, a `LoopDetected` verdict is returned
- [ ] If the last 4 fingerprints form an ABAB alternating pattern, a `PingPongDetected` verdict is returned
- [ ] On either verdict, the agent loop is aborted and an error is returned to the client
- [ ] The `LoopGuard` is reset when the user sends a new message
- [ ] Unit tests cover: exact repeat detection, ping-pong detection, and no false positive on varied arguments

**Source:** Gap plan Sprint 1, item #1
**Dependencies:** None

---

### BR-081: SSRF Protection

**Priority:** P0
**Status:** Draft

**Description:**
Tools that make outbound HTTP requests must block requests targeting private IP ranges, loopback addresses, link-local addresses, and cloud metadata endpoints.

**Rationale:**
Without SSRF protection, a compromised prompt can instruct a search tool to probe internal infrastructure (workstation, docker-host, llama-server API) or AWS metadata endpoints.

**Acceptance Criteria:**
- [ ] A `check_url()` function validates URLs before any HTTP request is made by a tool
- [ ] The following IP ranges are blocked: RFC 1918 (10.x, 172.16-31.x, 192.168.x), loopback (127.x), link-local (169.254.x), metadata (169.254.169.254), IPv6 equivalents (::1, fe80::, fc00::)
- [ ] The following URL schemes are blocked: `file://`, `data://`, `gopher://`
- [ ] Explicitly configured internal service URLs (searxng_url, perplexica_url, cc_bridge_url, etc.) are allowlisted in config and bypass the block
- [ ] SSRF blocking is applied to: `SearxngTool`, `PerplexicaTool`, `WebSearchTool`, and any future web-fetch tool
- [ ] Unit tests cover all RFC 1918 ranges, loopback, link-local, metadata endpoint, and allowlisted exceptions

**Source:** Gap plan Sprint 1, item #2
**Dependencies:** None

---

### BR-082: Prompt Injection Detection

**Priority:** P0
**Status:** Draft

**Description:**
User input must be scanned for prompt injection patterns before being sent to the LLM. Detection behavior (warn, block, or log-only) must be configurable.

**Rationale:**
The existing sanitizer strips known injection markers but does not score or detect novel injection attempts. An `InjectionDetector` with regex-based category scoring catches broader injection patterns.

**Acceptance Criteria:**
- [ ] An `InjectionDetector` scans input across six categories: SystemPromptOverride, RoleConfusion, ToolCallInjection, SecretExtraction, CommandInjection, Jailbreak
- [ ] Detection uses approximately 30 regex patterns across the six categories
- [ ] Each detected pattern contributes to a composite score (0.0–1.0)
- [ ] When the score exceeds the configured threshold (default 0.5), the configured action is taken
- [ ] Configurable actions: `"log"` (record to audit, proceed), `"warn"` (return warning to client, proceed), `"block"` (return error to client, do not proceed)
- [ ] Detection results are always logged to the audit trail regardless of action
- [ ] Unit tests verify each category with known patterns and verify no false positives on normal chat input

**Source:** Gap plan Sprint 1, item #3
**Dependencies:** None

---

### BR-083: Outbound Leak Detection

**Priority:** P0
**Status:** Draft

**Description:**
Messages and tool results must be scanned for secret patterns before being sent to the LLM. Detected secrets must be redacted or blocked per the configured policy.

**Rationale:**
Files, environment dumps, or command output may contain API keys or credentials. Sending these to the LLM risks leaking secrets to LLM provider logs or through model responses.

**Acceptance Criteria:**
- [ ] A `LeakDetector` scans text for 22+ secret patterns including: AWS keys (AKIA prefix), GitHub tokens (ghp_/gho_/ghs_), JWTs (eyJ prefix), PEM keys, database URLs, Slack tokens (xoxb-/xoxp-), SSH private keys, and base64-encoded strings over 40 characters
- [ ] On detection, the configured action is applied: Block (return error), Redact (replace with `[REDACTED:key_name]`), or Warn
- [ ] Scanning is applied: (1) to the full message before the LLM call in `chat.send`, and (2) to tool output before appending to conversation history
- [ ] When Redact action is used, the message structure is preserved; only the secret value is replaced
- [ ] Detection events are logged to the audit trail
- [ ] Unit tests cover each pattern category and verify redaction preserves message structure

**Source:** Gap plan Sprint 1, item #4
**Dependencies:** None

---

### BR-084: Library Injection Blocking

**Priority:** P0
**Status:** Draft

**Description:**
The shell tool must strip environment variables that allow library injection or interpreter hijacking from any subprocess it spawns.

**Rationale:**
If a malicious script sets LD_PRELOAD in the parent environment, all spawned subprocesses can be compromised. Stripping these variables at spawn time breaks this attack vector.

**Acceptance Criteria:**
- [ ] Before spawning any subprocess, the shell tool removes the following env vars: `LD_PRELOAD`, `LD_LIBRARY_PATH`, `DYLD_INSERT_LIBRARIES`, `NODE_OPTIONS`, `PYTHONPATH`, `PYTHONSTARTUP`, `RUBYOPT`, `PERL5OPT`, `JAVA_TOOL_OPTIONS`
- [ ] The subprocess inherits all other environment variables normally
- [ ] A unit test spawns a shell with `LD_PRELOAD` set in the test environment and verifies it is absent in the child process environment

**Source:** Gap plan Sprint 1, item #12
**Dependencies:** None

---

### BR-085: Tool Chain Alerting

**Priority:** P0
**Status:** Draft

**Description:**
Ironjaw must detect and alert on dangerous sequences of tool calls that, in combination, could indicate a privilege escalation or data exfiltration attempt.

**Rationale:**
Individual tool calls may be safe in isolation but dangerous in sequence. A "write file then execute" sequence could deploy a backdoor; "secrets_get then shell" could exfiltrate credentials.

**Acceptance Criteria:**
- [ ] A `ChainMonitor` tracks the recent tool call history per session
- [ ] The following sequences are detected and acted upon:
  - `write_file` → `shell`: Warn
  - `shell` → `web_search`: Warn
  - `secrets_get` → `shell`: Block
  - `brain.recall` → `shell`: Warn
- [ ] Warn: the tool call proceeds but a `permission.notify` event is sent to all connected clients
- [ ] Block: the tool call is rejected and an error is returned
- [ ] Chain detection events are logged to the audit trail
- [ ] Unit tests verify each dangerous sequence and verify non-dangerous sequences pass without alert

**Source:** Gap plan Sprint 1, item #16
**Dependencies:** None

---

### BR-086: OS-Level Process Sandbox

**Priority:** P1
**Status:** Draft

**Description:**
The shell tool must apply OS-level process isolation to limit what spawned processes can do.

**Rationale:**
The HITL gate controls whether a shell command runs, but does not limit what the command can do once running. OS-level sandboxing adds a second containment layer.

**Acceptance Criteria:**
- [ ] On Windows (workstation), spawned shell processes are placed inside a Windows Job Object with: restricted process creation, a memory limit, and no unrestricted network access
- [ ] On Linux (if ever deployed to docker-host), spawned shell processes use bubblewrap with a read-only root and workspace-only write access
- [ ] Sandboxing does not prevent legitimate shell tool usage within the working directory
- [ ] A test verifies that a subprocess cannot write outside the configured working directory

**Source:** Gap plan Sprint 1 (P1 security), item #20
**Dependencies:** BR-084

---

### BR-087: Secret Store Categories

**Priority:** P1
**Status:** Draft

**Description:**
The secrets vault must categorize secrets as either system secrets (never exposed to subprocesses) or tool secrets (injected as environment variables for tool use).

**Rationale:**
Currently all secrets use the same exposure model. System secrets (Ironjaw API tokens, database credentials) should never appear in subprocess environments, while tool secrets (user-configured API keys for tool use) should be injectable.

**Acceptance Criteria:**
- [ ] The `secrets` table gains a `category TEXT` column with values `system` or `tool`
- [ ] Before spawning shell subprocesses, the environment is cleared, then only `tool`-category secrets are injected as env vars
- [ ] `system`-category secrets are never injected into subprocess environments
- [ ] The `SecretsSetTool` accepts and stores the category field
- [ ] Existing secrets default to `system` category on migration

**Source:** Gap plan Sprint 1 (P1 security), item #21
**Dependencies:** BR-084

---

### BR-088: Secret Output Scrubbing

**Priority:** P1
**Status:** Draft

**Description:**
All tool output must be scanned for secret values and redacted before being returned to the client or appended to conversation history.

**Rationale:**
A `shell` or `read_file` tool might output a file that contains secrets. Without scrubbing, these flow back to the LLM and to the client UI in plaintext.

**Acceptance Criteria:**
- [ ] A rolling buffer scrubber loads all configured secret values from the vault at tool execution time
- [ ] Tool output is scanned for secrets in plaintext, URL-encoded, base64, and hex forms
- [ ] Matched secrets are replaced with `[REDACTED:key_name]`
- [ ] Scrubbing handles secrets that may be split across streaming output chunks
- [ ] A unit test verifies each encoding variant is detected and redacted

**Source:** Gap plan Sprint 1 (P1 security), item #22
**Dependencies:** BR-087

---

## 7. Backend — Memory and Brain

### BR-090: sqlite-vec KNN Vector Search

**Priority:** P1
**Status:** Draft

**Description:**
The brain's vector similarity search must use sqlite-vec's approximate nearest neighbor (ANN) index instead of the current O(n) cosine scan over all embeddings.

**Rationale:**
At 10,000 memories the current O(n) scan takes measurable time. At 100,000+ memories (plausible if CC Chronicle transcripts are imported) it becomes a bottleneck on every `chat.send`.

**Acceptance Criteria:**
- [ ] A `memory_vectors` virtual table is created using the sqlite-vec extension (`CREATE VIRTUAL TABLE memory_vectors USING vec0(embedding float[768])`)
- [ ] On `add_memory()`, the embedding is inserted into both the existing memories table and the virtual table
- [ ] `brain.recall` uses a vector table KNN query (`SELECT rowid, distance FROM memory_vectors WHERE embedding MATCH ? ORDER BY distance LIMIT ?`) instead of loading all embeddings into Rust
- [ ] A fallback to O(n) cosine scan is used if sqlite-vec fails to load
- [ ] Recall latency for a 10,000-memory store is under 50ms

**Source:** Gap plan Sprint 4, item #28
**Dependencies:** None

---

### BR-091: Typed Memories with Importance Scoring

**Priority:** P1
**Status:** Draft

**Description:**
Memories must be categorized by type (Fact, Preference, Decision, Identity, Event, Observation, Goal, Todo) with an importance score that determines retention during pruning. Identity-type memories are exempt from decay.

**Rationale:**
Without typed memories, all memories are treated equally. Important decisions and identity information should survive longer than ephemeral observations. Type-based importance scoring enables intelligent retention.

**Acceptance Criteria:**
- [ ] The `memories` table gains `memory_type TEXT` (default `fact`) and `importance REAL` (default 0.5) columns via migration
- [ ] Importance defaults by type: Identity/Decision → 0.9, Preference/Goal → 0.7, Fact/Event → 0.5, Observation/Todo → 0.3
- [ ] Importance decays by `0.95^(days_since_last_access)` per maintenance cycle (BR-093)
- [ ] Identity-type memories are exempt from decay and pruning
- [ ] `brain.remember` accepts an optional `memory_type` parameter
- [ ] `brain.recall` results include `memory_type` and `importance` in the response

**Source:** Gap plan Sprint 4, item #29
**Dependencies:** None

---

### BR-092: Hybrid Search — Vector + FTS + RRF

**Priority:** P1
**Status:** Draft

**Description:**
Memory recall must combine vector similarity search and full-text search (FTS5), merging results via Reciprocal Rank Fusion (RRF) to improve recall quality.

**Rationale:**
Vector search misses exact-match queries (error codes, function names, proper nouns). FTS5 catches these. RRF merging combines both result sets without requiring score normalization.

**Acceptance Criteria:**
- [ ] An FTS5 virtual table (`memory_fts`) is created with porter stemming tokenizer
- [ ] On `add_memory()`, the memory content is inserted into both the vector and FTS tables
- [ ] `brain.recall` runs both a vector KNN query and an FTS5 BM25 query
- [ ] Results are merged via RRF with k=60: `score = Σ(1 / (k + rank_in_list))` for each document
- [ ] The top-K results from the merged list are returned
- [ ] No new dependencies are required (FTS5 is built into SQLite)

**Source:** Gap plan Sprint 4, item #30
**Dependencies:** BR-090

---

### BR-093: Memory Maintenance — Decay, Pruning, Deduplication

**Priority:** P1
**Status:** Draft

**Description:**
A background maintenance process must run daily to apply importance decay, prune low-importance memories, and deduplicate near-duplicate memories.

**Rationale:**
Without maintenance, the memory store grows unboundedly with stale, low-value, or duplicate content. Decay and pruning keep the memory store relevant and within capacity.

**Acceptance Criteria:**
- [ ] A background tokio task runs memory maintenance every 24 hours (and on startup)
- [ ] Decay: for each memory, `importance *= 0.95^(days_since_last_access)`
- [ ] Pruning: memories with `importance < 0.1` are deleted, except Identity-type memories
- [ ] Deduplication: memories with cosine similarity > 0.95 are merged (newer content replaces older, oldest creation date is preserved)
- [ ] FTS5 and sqlite-vec indexes are reindexed after deduplication
- [ ] Maintenance results (decayed, pruned, deduplicated counts) are logged

**Source:** Gap plan Sprint 4, item #31
**Dependencies:** BR-090, BR-091

---

### BR-094: Knowledge Graph

**Priority:** P1
**Status:** Draft

**Description:**
A structured knowledge graph must be available alongside vector memory, storing named entities and typed relationships between them with confidence scores.

**Rationale:**
Vector memory stores facts as prose. A knowledge graph stores structured relationships (Person --manages--> Project, Tool --requires--> API) that can be queried precisely without semantic search.

**Acceptance Criteria:**
- [ ] `entities` table stores: id, name, entity_type, confidence, created_at
- [ ] `relations` table stores: id, from_id, to_id, relation_type, confidence, created_at
- [ ] `kg_add_entity` tool (Notify tier) creates or updates an entity
- [ ] `kg_add_relation` tool (Notify tier) creates a typed relationship between two entities
- [ ] `kg_query` tool (Auto tier) returns an entity and all its 1-hop relations
- [ ] All three tools are registered in `ToolRegistry` when `[brain]` is enabled

**Source:** Gap plan Sprint 4, item #27
**Dependencies:** None

---

### BR-095: Database Migration Path Planning for Brain and Sessions

**Priority:** P1
**Status:** Draft

**Description:**
A documented migration path must exist for moving brain and session data from SQLite to PostgreSQL + pgvector if scale requires it. No immediate migration is required, but the schema and access layer must not make migration unnecessarily difficult.

**Rationale:**
The user indicated both brain and sessions may outgrow SQLite within 2 years (especially if CC Chronicle transcripts are imported). Gap plan item #28 proposes sqlite-vec as an intermediate step. The access layer must abstract the storage backend.

**Acceptance Criteria:**
- [ ] `MemoryStore` and `SessionStore` traits are defined such that a PostgreSQL implementation can be substituted without changing call sites
- [ ] A `docs/adr/sqlite-to-postgres-migration.md` document describes the migration decision criteria, estimated migration effort, and recommended trigger conditions
- [ ] sqlite-vec (BR-090) is implemented as the first scalability improvement before committing to PostgreSQL

**Source:** Questionnaire Section 4.5, Section 5.2
**Dependencies:** BR-090

---

## 8. Backend — Tool Expansion

### BR-100: Long-Running Process Management

**Priority:** P1
**Status:** Draft

**Description:**
Agents must be able to spawn long-running processes, poll them for output, write to their stdin, list active processes, and kill them — all without blocking the agent loop.

**Rationale:**
The current shell tool is synchronous and blocks until the command exits. Development servers, build watchers, and monitoring scripts run indefinitely. Without async process management, these cannot be used by agents.

**Acceptance Criteria:**
- [ ] `process_start` tool (Confirm tier): spawns a process asynchronously, returns a PID
- [ ] `process_poll` tool (Auto tier): returns new stdout/stderr content since the last poll for a given PID
- [ ] `process_write` tool (Notify tier): sends input to a process's stdin
- [ ] `process_kill` tool (Confirm tier): terminates a process by PID
- [ ] `process_list` tool (Auto tier): returns all managed processes with PID, command, and uptime
- [ ] Each process maintains a 64KB ring buffer for stdout and stderr separately
- [ ] Process state persists for the duration of the Ironjaw process lifetime

**Source:** Gap plan Sprint 5, item #32
**Dependencies:** None

---

### BR-101: Interactive Shell Output Streaming

**Priority:** P1
**Status:** Draft

**Description:**
Shell and process output must be streamed to connected dashboard and Dr. CLAW clients in real-time via WebSocket events.

**Rationale:**
Long-running commands currently require repeated polling. Streaming output events allow the dashboard and Dr. CLAW to display live output in a terminal-style view without any client-side polling.

**Acceptance Criteria:**
- [ ] As a managed process produces stdout or stderr output, a `process.output` WebSocket event is emitted to all connected clients
- [ ] Each `process.output` event includes: PID, stream type (stdout/stderr), and the new line(s) of output
- [ ] The dashboard renders streaming process output in a scrolling terminal-style view
- [ ] Dr. CLAW renders streaming process output in an equivalent scrollable view

**Source:** Gap plan Sprint 5, item #33
**Dependencies:** BR-100, BR-010

---

### BR-102: apply_patch Tool

**Priority:** P1
**Status:** Draft

**Description:**
An `apply_patch` tool must be available for agents to apply unified diff patches to files in the working directory.

**Rationale:**
Agents currently write whole files. For large files with small changes, applying a diff is more efficient and easier to review in HITL.

**Acceptance Criteria:**
- [ ] `apply_patch` tool (Confirm tier): accepts a `patch` string in unified diff format and an optional `working_dir`
- [ ] The tool parses `--- a/file`, `+++ b/file`, and `@@ hunk` headers from the patch
- [ ] Each file identified in the patch is read, the hunks applied, and the file written back
- [ ] Returns a result of `{ files_modified: N, hunks_applied: N }`
- [ ] Files outside the configured working directory are rejected

**Source:** Gap plan Sprint 5, item #34
**Dependencies:** None

---

### BR-103: docker_exec Tool

**Priority:** P1
**Status:** Draft

**Description:**
An `docker_exec` tool must allow agents to run commands inside Docker containers on docker-host via SSH.

**Rationale:**
docker-host hosts 21 Docker containers across 7 stacks. Agents currently cannot interact with container internals. `docker_exec` enables debugging, log reading, and configuration from an agent session.

**Acceptance Criteria:**
- [ ] `docker_exec` tool (Confirm tier): accepts `container` name, `command` string, and optional `timeout_secs`
- [ ] The tool SSHs to docker-host and runs `docker exec {container} {command}` with the specified timeout
- [ ] Container name is validated against an alphanumeric + `[-_.]` allowlist
- [ ] No shell metacharacters are accepted in the command string without explicit quoting/escaping
- [ ] Output is returned as stdout/stderr pair

**Source:** Gap plan Sprint 5, item #35
**Dependencies:** BR-081 (SSRF protection applies to SSH targets)

---

### BR-104: Sub-Agent Delegation

**Priority:** P1
**Status:** Draft

**Description:**
An agent must be able to spawn a sub-agent session with a custom system prompt to handle a delegated subtask, with a maximum delegation depth to prevent unbounded recursion.

**Rationale:**
Complex tasks benefit from specialized sub-agents (a coding sub-agent, a research sub-agent). Without delegation support, every capability must be embedded in one monolithic system prompt.

**Acceptance Criteria:**
- [ ] `delegate_task` tool (Notify tier): accepts `system_prompt`, `task`, and optional `max_depth` (default 3)
- [ ] A new session is created for the sub-agent, with the parent session ID recorded
- [ ] The delegation depth is tracked; at depth 3, further delegation is rejected
- [ ] The tool blocks until the sub-agent completes and returns the sub-agent's final response
- [ ] Sub-agent sessions are visible in the CC Sessions page with their parent session linked

**Source:** Gap plan Sprint 5, item #36
**Dependencies:** BR-061, BR-071

---

### BR-105: ADB Tool

**Priority:** P1
**Status:** Draft

**Description:**
An `adb` tool must allow agents to run Android Debug Bridge commands locally on workstation.

**Rationale:**
The user has active Android development (Dr. CLAW, re-lab) and stated this has "high personal value." ADB tool access allows agents to assist with device testing, log capture, and APK deployment.

**Acceptance Criteria:**
- [ ] `adb` tool (Confirm tier): accepts an `action` from the allowlist (`shell`, `push`, `pull`, `install`, `logcat`, `devices`) and relevant args
- [ ] `logcat` action defaults to returning the last 50 lines
- [ ] The action field is validated against the allowlist before execution
- [ ] Shell metacharacters in `args` are rejected
- [ ] ADB is executed locally on workstation with a 30-second timeout

**Source:** Gap plan Sprint 5, item #37
**Dependencies:** None

---

## 9. Backend — Smart Routing and Cost

### BR-110: Complexity-Based Model Routing

**Priority:** P1
**Status:** Draft

**Description:**
The LLM router must score each request's complexity and route it to the appropriate model tier, rather than always using the same provider.

**Rationale:**
Simple queries (3 messages, no tools) do not need a 32B model. Routing by complexity keeps the 32B models available for hard problems and reduces average inference time for easy queries.

**Acceptance Criteria:**
- [ ] A `ComplexityScorer` classifies each request as Simple, Medium, or Complex based on: message count, system prompt token count, presence of recent tool calls
- [ ] The `[llm.routing]` config section defines `simple_model`, `medium_model`, and `complex_model`
- [ ] The router selects the model for each request based on the complexity classification
- [ ] Model-prefix routing is also fixed: if `request.model` starts with a provider name, that provider is used regardless of complexity score
- [ ] The selected complexity tier and model name are included in the audit log entry for each `chat.send`

**Source:** Gap plan Sprint 3, item #24
**Dependencies:** None

---

### BR-111: LLM Token Usage Tracking and Budget Enforcement

**Priority:** P1
**Status:** Draft

**Description:**
All LLM token usage must be tracked per model, and per-model token budgets (hourly, daily, monthly) must be configurable and enforced.

**Rationale:**
Without usage tracking, there is no visibility into how much llama-server compute each model consumes. Budget enforcement prevents runaway usage from automated workflows.

**Acceptance Criteria:**
- [ ] Every LLM call records: model name, tokens_in, tokens_out, session_id, and timestamp to a `usage_events` table
- [ ] A `budgets` table stores per-model hourly, daily, and monthly token limits
- [ ] Before each LLM call, the current period's usage is checked against the configured budget
- [ ] If a budget is exceeded, the call is rejected with a clear error message
- [ ] Usage summary data is exposed via `infra.metrics` and displayed on the Metrics dashboard page (BR-017)
- [ ] Budget configuration is editable from the Settings page

**Source:** Gap plan Sprint 3, item #25
**Dependencies:** BR-062

---

### BR-112: Provider Decorator Stack

**Priority:** P1
**Status:** Draft

**Description:**
LLM providers must be wrapped with composable decorators for retry with exponential backoff, response caching, and provider failover with error-class-based routing.

**Rationale:**
The current provider implementation handles errors inconsistently. A decorator stack standardizes retry, caching, and failover behavior across all providers without duplicating logic.

**Acceptance Criteria:**
- [ ] `LlmProvider` is a trait, not an enum. All providers (`LlamaServerProvider`, `OpenAiProvider`, `MockProvider`) implement it
- [ ] `RetryProvider<P>` decorator wraps any provider and retries on transient errors with configurable backoff
- [ ] `CacheProvider<P>` decorator wraps any provider with the response cache from BR-062
- [ ] `FailoverProvider` holds multiple providers and tries each in order on failure
- [ ] Error classification maps HTTP status codes to: RateLimit (429), Auth (401/403), Server (5xx), Network/Timeout
- [ ] Rate limit errors use exponential backoff before retry; auth errors skip the provider; server errors retry once then failover; network/timeout errors failover immediately

**Source:** Gap plan Sprint 3, item #26
**Dependencies:** BR-062

---

## 10. Backend — Interoperability

### BR-120: MCP Client — Consume External MCP Servers

**Priority:** P0
**Status:** Draft

**Description:**
Ironjaw must function as an MCP client, connecting to configured external MCP servers, discovering their tools, and making those tools available in the `ToolRegistry`.

**Rationale:**
The user explicitly selected "Both" for MCP support. Claude Code already uses jcodemunch, repomix, and context7 MCP servers. Ironjaw as MCP client can route tool calls to these same servers, giving the Ironjaw agent all Claude Code's MCP tool capabilities.

**Acceptance Criteria:**
- [ ] A `[[mcp.servers]]` array in `ironjaw.toml` configures MCP server connections
- [ ] Each server entry specifies: `name`, `transport` (stdio or http), and transport-specific config (command/args for stdio, url for http)
- [ ] On startup, Ironjaw connects to all configured MCP servers and calls `tools/list` to discover available tools
- [ ] Discovered tools are registered in `ToolRegistry` with the naming convention `mcp::{server_name}::{tool_name}`
- [ ] `tools.list` returns both built-in tools and discovered MCP tools
- [ ] `tools.execute` can invoke any discovered MCP tool by its namespaced name
- [ ] MCP server disconnections are handled gracefully (tools removed from registry, reconnect attempted)

**Source:** Questionnaire Section 4.3, Gap plan Sprint 6, item #38
**Dependencies:** BR-005

---

### BR-121: MCP Server Mode — Expose Ironjaw Tools to Claude Code

**Priority:** P0
**Status:** Draft

**Description:**
Ironjaw must expose all its registered tools as MCP tools via an stdio MCP server mode, allowing Claude Code and other MCP clients to call Ironjaw tools natively.

**Rationale:**
The user explicitly selected "Both" for MCP support. If Ironjaw is an MCP server, the 17+ Ironjaw tools (shell, write_file, searxng, brain.recall, etc.) are available directly from Claude Code sessions on workstation.

**Acceptance Criteria:**
- [ ] `ironjaw mcp-server` CLI subcommand launches Ironjaw in stdio MCP server mode
- [ ] The server implements the MCP `tools/list` endpoint, returning all tools from the `ToolRegistry` in MCP tool definition format
- [ ] The server implements the MCP `tools/call` endpoint, dispatching to `ToolRegistry::execute` and returning results in MCP format
- [ ] Auth is enforced: the MCP server requires the configured token
- [ ] Claude Code can be configured to use Ironjaw as an MCP server and successfully call at least `shell`, `read_file`, and `brain.recall`

**Source:** Questionnaire Section 4.3, Gap plan Sprint 6, item #39
**Dependencies:** BR-120

---

### BR-122: OpenAI-Compatible REST API

**Priority:** P1
**Status:** Draft

**Description:**
Ironjaw must expose an OpenAI-compatible REST API (`POST /v1/chat/completions`, `GET /v1/models`) so that any OpenAI SDK-compatible tool can use Ironjaw as its LLM backend.

**Rationale:**
Open WebUI on docker-host already connects to llama-server directly. With an OpenAI-compatible endpoint on Ironjaw, Open WebUI (and other tools) can route through Ironjaw's session, brain, and security layers.

**Acceptance Criteria:**
- [ ] `POST /v1/chat/completions` accepts a standard OpenAI request body (model, messages, stream, temperature)
- [ ] Streaming responses are returned as SSE (`text/event-stream`) in OpenAI delta format
- [ ] Non-streaming responses return a standard `choices[0].message` JSON response
- [ ] `GET /v1/models` returns the list of available models in OpenAI list format
- [ ] Auth uses Bearer token (same as WebSocket auth token)
- [ ] Requests are routed through `LlmRouter`, `repair_history`, `LeakDetector`, and brain auto-recall

**Source:** Gap plan Sprint 6, item #40
**Dependencies:** BR-082, BR-083, BR-061

---

### BR-123: A2A Protocol — Agent-to-Agent Federation

**Priority:** P2
**Status:** Draft

**Description:**
Ironjaw must implement the Google Agent-to-Agent (A2A) protocol to allow other A2A-compatible agents to send tasks to Ironjaw and for Ironjaw agents to send tasks to external A2A-capable agents.

**Rationale:**
A2A is an emerging interoperability standard. Implementing it positions Ironjaw to participate in multi-agent ecosystems without requiring custom integrations per partner system.

**Acceptance Criteria:**
- [ ] `GET /.well-known/agent.json` returns a valid A2A agent card with capabilities, auth requirements, and endpoints
- [ ] `POST /a2a/tasks/send` accepts an A2A task from an external agent and creates an internal task
- [ ] `GET /a2a/tasks/{id}` returns the current status and result of an A2A task
- [ ] `a2a_discover` agent tool (Auto tier): fetches an agent card from a remote URL
- [ ] `a2a_send` agent tool (Confirm tier): sends a task to a remote A2A-capable agent

**Source:** Gap plan Sprint 6, item #41
**Dependencies:** BR-071

---

## 11. Architecture and Infrastructure

### BR-130: Single Binary, Single Port

**Priority:** P0
**Status:** Draft

**Description:**
All Ironjaw capabilities must be delivered from a single compiled binary listening on a single port. No companion processes, separate binaries, or additional runtime dependencies (Node.js, Python) may be introduced in the primary execution path.

**Rationale:**
The user confirmed "maintain single binary" explicitly in Section 5.1. The single-binary architecture is a core value proposition.

**Acceptance Criteria:**
- [ ] `cargo build --release` produces one binary artifact
- [ ] The binary binds to one port (default 18794)
- [ ] All 38+ WebSocket methods, all tools, brain, proactive engine, voice, and vision are served from this single binary
- [ ] No runtime dependency on Node.js, Python, Ruby, or any managed runtime exists in the production execution path
- [ ] The binary self-contains all static dashboard assets via `rust-embed`

**Source:** Questionnaire Section 5.1, Section 6.3
**Dependencies:** None

---

### BR-131: Legacy Service Retention — No Decommission Date

**Priority:** P0
**Status:** Draft

**Description:**
OpenClaw Gateway (:18789), CC Bridge (:18790), and CC Chronicle (:18793) must continue running alongside Ironjaw as a safety net. No decommission date is set.

**Rationale:**
The user selected "keep all three running indefinitely (safety net)" in Section 5.1. Ironjaw proxies these services but does not replace them until confidence is established.

**Acceptance Criteria:**
- [ ] OpenClaw Gateway remains on :18789 and is not modified
- [ ] CC Bridge remains on :18790 and is not modified
- [ ] CC Chronicle remains on :18793 and is not modified
- [ ] Ironjaw on :18794 proxies cc_sessions and cc_chronicle data through CC Bridge and CC Chronicle respectively
- [ ] All four services start and run independently; failure of any one does not cause the others to fail

**Source:** Questionnaire Section 5.1
**Dependencies:** None

---

### BR-132: Full CI/CD Pipeline

**Priority:** P0
**Status:** Draft

**Description:**
Every push to the Gitea repository must trigger an automated pipeline that runs tests, linting, and formatting checks, and deploys the new binary to workstation or docker-host on a green build.

**Rationale:**
The user selected "automated test + build + deploy — push to workstation/docker-host on green CI" in Section 5.5. The current pre-commit-only workflow is insufficient for a daily-driver platform.

**Acceptance Criteria:**
- [ ] Gitea Actions pipeline is configured on the `scaso01/ironjaw` repository on docker-host
- [ ] Pipeline runs on every push: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test`
- [ ] A red pipeline blocks the push from being considered deployable
- [ ] A green pipeline automatically deploys the new binary to the configured target (workstation via task scheduler restart, or docker-host via Docker image rebuild)
- [ ] Deployment failure triggers a Dr. CLAW urgent notification (BR-031)
- [ ] Pipeline configuration is stored in the repository and version-controlled

**Source:** Questionnaire Section 5.5
**Dependencies:** BR-005

---

### BR-133: Prometheus Metrics Endpoint and Grafana Dashboard

**Priority:** P0
**Status:** Draft

**Description:**
Ironjaw must expose a Prometheus-compatible metrics endpoint, and a Grafana dashboard must be provisioned on docker-host to visualize Ironjaw's operational metrics.

**Rationale:**
docker-host already runs Grafana + Prometheus for link-monitor. The user requested "Grafana dashboard for Ironjaw metrics (expose Prometheus-compatible endpoint)" as a confirmed gap in Section 5.6.

**Acceptance Criteria:**
- [ ] `GET /metrics` returns Prometheus text format exposition with all metrics tracked by `infra.metrics`
- [ ] Prometheus on docker-host is configured to scrape Ironjaw's `/metrics` endpoint
- [ ] A Grafana dashboard JSON provisioning file is added to docker-host's Grafana provisioning directory
- [ ] The Grafana dashboard includes panels for: request counts per method, error rates, active WebSocket connections, LLM token throughput, circuit breaker states, and tool execution latency
- [ ] The dashboard is visible in Grafana without manual import (auto-provisioned)

**Source:** Questionnaire Section 5.6
**Dependencies:** BR-017

---

### BR-134: Observability Gaps — Trend Tracking and Proactive Alerting

**Priority:** P0
**Status:** Draft

**Description:**
Ironjaw must track LLM response latency and SQLite disk usage as time-series metrics, send push notifications on circuit breaker open events and silent schedule failures, and route all alerts to Dr. CLAW.

**Rationale:**
All six items in Section 5.6 were confirmed as gaps the user wants filled.

**Acceptance Criteria:**
- [ ] LLM response latency is recorded per-request and exposed as a histogram in `/metrics`
- [ ] SQLite file sizes (sessions.db, brain.db, audit.db, proactive.db, secrets.db) are sampled periodically and exposed as gauges in `/metrics`
- [ ] When a circuit breaker transitions from closed to open, an urgent Dr. CLAW push notification is sent immediately (not on next polling cycle)
- [ ] When a proactive engine scheduled task completes with an error or does not run when expected, an urgent Dr. CLAW push notification is sent
- [ ] All alert notifications include: service name, event type, timestamp, and relevant diagnostic detail

**Source:** Questionnaire Section 5.6
**Dependencies:** BR-031, BR-133

---

### BR-135: Docker Deployment on docker-host (Preferred) and Windows Service on workstation (Alternative)

**Priority:** P1
**Status:** Draft

**Description:**
The preferred production deployment of Ironjaw is as a Docker container on docker-host, managed via Docker Compose. A Windows Service deployment on workstation must be supported as an alternative.

**Rationale:**
The user selected "Docker on docker-host preferred (keep workstation focused on development)" in Section 5.4, with awareness of the LAN latency tradeoff.

**Acceptance Criteria:**
- [ ] A `Dockerfile` exists in the repository that produces a minimal Ironjaw image (Rust build in multi-stage build)
- [ ] A `docker-compose.yml` is provided for deployment on docker-host under `/srv/docker/ironjaw/`
- [ ] A Windows Service setup guide exists for workstation deployment as an alternative
- [ ] When running on docker-host, Ironjaw connects to llama-server on workstation via LAN (192.0.2.10:8080)
- [ ] Connection latency from docker-host Docker to workstation llama-server is documented and measured
- [ ] The Task Scheduler entry on workstation remains functional as the development/fallback deployment

**Source:** Questionnaire Section 5.4
**Dependencies:** BR-005

---

### BR-136: Release Binary Optimization

**Priority:** P1
**Status:** Draft

**Description:**
The release binary must be optimized for size and performance via LTO, single codegen unit, symbol stripping, and panic=abort profile settings.

**Rationale:**
The default Rust release profile does not enable LTO or strip debug symbols. These settings meaningfully reduce binary size and improve runtime performance for a long-running server process.

**Acceptance Criteria:**
- [ ] `Cargo.toml` `[profile.release]` includes: `lto = true`, `codegen-units = 1`, `strip = true`, `panic = "abort"`
- [ ] `cargo build --release` succeeds with these settings
- [ ] The resulting binary is smaller than the baseline release build without these settings
- [ ] No test failures are introduced by the `panic = "abort"` setting

**Source:** Gap plan Sprint 1, item #13
**Dependencies:** None

---

### BR-137: WASM Tool Sandbox — Deferred

**Priority:** P3
**Status:** Draft

**Description:**
WASM-based tool isolation (running tools in a wasmtime sandbox) is a future consideration, deferred until multi-user becomes a reality.

**Rationale:**
The user selected "Future — good idea, but not now. Only if multi-user becomes reality." in Section 4.2. Current HITL + permissions model is sufficient for single-user.

**Acceptance Criteria:**
- [ ] No WASM sandbox implementation is required before stable v1
- [ ] The tool registry interface (`Tool` trait) must not make WASM sandboxing architecturally impossible in the future
- [ ] This requirement is revisited if multi-user support is added (BR-003)

**Source:** Questionnaire Section 4.2
**Dependencies:** BR-003

---

## 12. Known Issues and Stabilization

### BR-140: Dashboard Response Parsing — Systematic Fix

**Priority:** P0
**Status:** Draft

**Description:**
All eleven dashboard pages have response parsing mismatches between the WebSocket protocol frames sent by Ironjaw and the parsing logic in the dashboard client. These must be systematically identified and fixed in priority order.

**Rationale:**
This is the primary blocker preventing the dashboard from being a daily driver. The user identified all pages as having issues in Section 7.1.

**Acceptance Criteria:**
- [ ] Chat page: sends messages, receives streaming responses, and renders them without parse errors
- [ ] CC Sessions page: lists sessions, searches, and displays transcripts without parse errors
- [ ] Tools page: lists tools, executes them, and displays results without parse errors
- [ ] Brain page: recalls, adds, and deletes memories without parse errors
- [ ] Metrics page: displays all metrics data without parse errors
- [ ] Models page: lists and displays model information without parse errors
- [ ] Settings page: reads and writes settings without parse errors
- [ ] Chronicle page: displays chronicle entries without parse errors
- [ ] Schedules page: lists and manages schedules without parse errors
- [ ] Devices page: lists connected devices without parse errors
- [ ] Zero console errors of the form "unexpected field" or "response parse error" in any fixed page
- [ ] Fix priority order: Chat, CC Sessions, Tools, Brain, Metrics, Models, Settings, Chronicle, Schedules, Devices

**Source:** Questionnaire Section 7.1
**Dependencies:** BR-010, BR-011

---

### BR-141: Dr. CLAW v1.0.0 Stabilization

**Priority:** P0
**Status:** Draft

**Description:**
Dr. CLAW v1.0.0 (with bottom navigation, 7 new screens, and HITL dialogs) must be stabilized to eliminate known regressions before new features are added.

**Rationale:**
The user reported "most of it does not work — needs major stabilization" in Section 7.2. New features on an unstable base compound existing bugs.

**Acceptance Criteria:**
- [ ] A testing audit is conducted to catalog all broken behaviors in v1.0.0
- [ ] All broken behaviors identified in the audit are triaged and assigned to sprint slots
- [ ] Chat screen accepts input, sends messages, and displays streaming responses without crashes
- [ ] All bottom navigation tabs navigate to their destinations without crashes
- [ ] HITL approval dialogs appear and can be approved or denied without crashing
- [ ] All screens introduced in v1.0.0 load without fatal errors
- [ ] The app does not regress any behavior that worked correctly in v0.9.3

**Source:** Questionnaire Section 7.2
**Dependencies:** BR-030, BR-031

---

### BR-142: Ironjaw Backend Method Audit

**Priority:** P0
**Status:** Draft

**Description:**
An audit must be performed to determine which of the 38 defined Protocol v4 methods have been verified end-to-end and which have not, and any unverified methods must be tested and fixed.

**Rationale:**
The side-by-side sprint verified 15 of 38 methods. The remaining 23 methods have not been confirmed as working. Shipping a platform with silent method failures is unacceptable.

**Acceptance Criteria:**
- [ ] A method audit document lists all 38 methods with their verified/unverified status
- [ ] Every unverified method is tested via a live WebSocket client against a running Ironjaw instance
- [ ] All methods that fail during verification have defects filed and fixed
- [ ] After fixes, all 38 methods are verified end-to-end
- [ ] The method reference table in `CLAUDE.md` is updated to reflect verified status

**Source:** Questionnaire Section 7.3
**Dependencies:** BR-140

---

## Glossary

| Term | Definition |
|------|-----------|
| **workstation** | Primary development machine. Ryzen AI 9 HX 370, 64GB RAM, Windows 11 Pro. Runs Ironjaw, llama-server, CC Bridge, CC Chronicle, OpenClaw, Caddy. |
| **docker-host** | Docker appliance. i7-3630QM, 16GB RAM, Ubuntu Server 24.04. Runs all Docker stacks including SearXNG, Perplexica, Grafana, Gitea. |
| **Dr. CLAW** | Native Android app (Samsung Galaxy S22 Ultra primary target). One of four Ironjaw client interfaces. |
| **Ironjaw** | Rust WebSocket gateway. Single binary on port 18794. 12 crates, 348+ tests. Replaces OpenClaw + CC Bridge + CC Chronicle long-term. |
| **HITL** | Human-in-the-Loop. Approval gate for Confirm-tier tool calls. |
| **llama-server** | Local LLM inference server (`llama.cpp`) on workstation. Port 8080, OpenAI-compatible API. No cold-start after first load. |
| **MCP** | Model Context Protocol. Anthropic-defined standard for tool exposure between AI systems and external tools. |
| **Protocol v4** | Ironjaw's WebSocket frame protocol. 8 frame types, 38 methods. |
| **RRF** | Reciprocal Rank Fusion. Score-merging algorithm for combining ranked result lists from heterogeneous sources. |
| **VAD** | Voice Activity Detection. Automatic detection of speech start and end without push-to-talk. |
| **P0** | Must have for stable v1. No deference. |
| **P1** | Should have. High-value, implement before declaring stability. |
| **P2** | Nice to have. Implement after P0/P1 are complete. |
| **P3** | Future. Do not implement until a specific need arises. |

---

## Change Log

| Date | Change | Author |
|------|--------|--------|
| 2026-03-11 | Initial document created from questionnaire answers and gap plan | Requirements Analyst |
