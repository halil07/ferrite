# Ferrite E2E

> **[Türkçe Dokümantasyon (README.tr.md)](README.tr.md)**

**Ferrite E2E** is a fast, hybrid end-to-end browser automation framework powered by a **Chrome Manifest V3 (`chrome.debugger`) extension + Chrome Native Messaging + a native Rust/Tokio engine**.

Write clean, jQuery-style `.ferrite` test suites with strict semicolon syntax, `.expect()` assertions, network mocking, console assertions, visual regression (`pixelmatch`), action recording, and built-in **MCP** support for AI agents — all running directly in your real Chrome browser without Playwright/Puppeteer server overhead.

---

## Installation & Quick Start

### 1. Install the Chrome Extension
Install **Ferrite E2E Bridge** from the Chrome Web Store *(or download `ferrite-e2e-chrome-extension.zip` from [GitHub Releases](../../releases), unzip it, enable **Developer mode** at `chrome://extensions`, and click **Load unpacked**)*.

### 2. Install the CLI in Your Project
```bash
npm install -D @halil07/ferrite
```

### 3. Connect Chrome to Ferrite (One-Time Setup)
Copy the 32-character **Extension ID** from `chrome://extensions` and register the Native Messaging host:

```bash
npx ferrite setup --extension-id <EXTENSION_ID>
```
*(Supported browsers: `--browser chrome` (default), `edge`, `brave`, `chromium`.)*

Reload the extension in `chrome://extensions` (↻). Clicking the extension icon should show **Native Host: Connected**.

### 4. (Optional) Install VS Code Extension
For `.ferrite` syntax highlighting, snippets, and context-aware IntelliSense (auto-completing `.expect().visible()`, `.expect().count()`, selectors, and user macros):
- Install from the VS Code package (`ferrite-e2e-vscode-extension.zip` in Releases) or run `.\scripts\install-vscode-ext.ps1` if you cloned the repo, then reload VS Code (`Ctrl+Shift+P` → `Developer: Reload Window`).

### 5. Initialize and Run Tests
```bash
npx ferrite init
npx ferrite check
npx ferrite run
```

---

## Writing `.ferrite` Tests

`.ferrite` uses a strict, readable, jQuery-inspired programming syntax. Every statement ends with a semicolon (`;`), actions chain directly on selectors, and assertions require `.expect()`.

### Suite Structure, Variables, Imports & Macros

```ferrite
suite("checkout-flow");
import "./helpers/auth.ferrite";

let base_url = "https://example.com";

beforeEach {
  await goto("about:blank");
  await goto("${base_url}", "load", 15s);
}

macro login(email, password) {
  await $("#email").fill("${email}");
  await $("#password").fill("${password}").press("Enter");
}

test("user-can-sign-in") {
  await login("ada@example.com", "secret");
  await $text("Welcome back, Ada").expect().visible();
}
```

### Selectors: `$`, `$text`, `$role`, and `@ref`

| Selector | Target | Example |
| :--- | :--- | :--- |
| `$("css")` | Any CSS selector (`querySelectorAll`), supports `:eq(n)` & `:first` | `await $(".todo-list li label:eq(0)").dblclick();` |
| `$text("...")` | Visible text in the Chrome Accessibility Tree | `await $text("1 item left").expect().visible();` |
| `$role("role", "name"?)` | ARIA role and optional accessible name | `await $role("button", "Sign In").exact().click();` |
| `$(@e1)` / `@e1` | Deterministic ref from `await snapshot("interactive");` | `await $(@e1).fill("ada@example.com");` |

> **Tip:** Single quotes (`'...'`) work too, which is handy for attribute selectors without escaping:
> `await $('.filters a[href="#/active"]').click();`

### Actions & Chaining

Selectors return chainable targets so you can select once and perform one or more actions:

```ferrite
// Click, double-click, hover
await $("#submit-btn").click();
await $(".todo-list li label:eq(0)").dblclick();
await $(".todo-list li:first").hover();

// Fill / val + keyboard chaining
await $("input.new-todo").fill("Buy milk").press("Enter");
await $("input.search").val("Ferrite").press("Enter");

// Edit with keyboard shortcuts & raw typing
await $(".todo-list li label:eq(0)")
  .dblclick()
  .press("Control+a")
  .press("Backspace")
  .type("Buy oat milk")
  .press("Enter");

// Index & timeout modifiers (before the action or .expect())
await $(".item").eq(2).timeout(10s).click();
await $role("button", "Save").exact().click(8s);
```

### Assertions (`.expect()`)

All DOM/text assertions live under `.expect()` (auto-waiting up to `5s` by default, or pass a custom duration):

```ferrite
await $("header.header h1").expect().visible();
await $("input.new-todo").expect().visible(10s);
await $(".footer").expect().hidden();
await $(".todo-list li").expect().count(3);
await $(".todo-list li.completed").expect().count(1, 5s);

await $text("Buy milk").exact().expect().visible();
await $text("Old task").expect().hidden();
```

### Navigation, Sleep & Visual Regression

```ferrite
await goto("https://example.com");
await goto("https://example.com", "domcontentloaded", 10s);
await reload();       // or await reload(true); to bypass cache
await sleep(1s);      // accepts ms, s, m (e.g. 500ms, 5s)

// Visual regression (pixelmatch with anti-aliasing tolerance)
await screenshot("home-page", 0.1, 0.005);
```
To update visual baseline PNGs after an intentional UI change:
```bash
npx ferrite run --update-baselines
```

### Network Mocking (`mock.when` & `mock.load`)

Intercept HTTP/XHR/Fetch requests directly in Chrome with `:param` path substitution, subset matching on `params` and JSON `data`, non-blocking `delay`, network failure simulation, and pure response JSON files:

```ferrite
// Fluent jQuery-style rule (`:id` is resolved from `params`; remaining params match query subset)
await mock.when("GET", "/api/users/:id/orders")
  .params({ id: "42", page: "1" })
  .reply(200, "./mocks/responses/orders.json")
  .delay(300ms);

// POST body subset matching (extra fields like timestamp/csrfToken are ignored)
await mock.when("POST", "/api/login")
  .data({ email: "admin@test.com" })
  .header("x-mock", "true")
  .reply(200, "./mocks/responses/login-ok.json");

// Simulate network timeout or abort
await mock.when("GET", "/api/health").delay(200ms).fail("TimedOut");
await mock.when("GET", "/api/down").abort();

// Load an external mock scenario file (`*.mock.json`) pointing to pure response JSON files
await mock.load("./mocks/checkout.mock.json");

// Clear all active mocks on the tab
await mock.clear();
```

### Console Capture & Error Assertions (`console`)

```ferrite
await console.start();
await console.clear();

// ... run user flow ...

await console.expect("App initialized", "info", 5s);
await console.noErrors();
```

---

## Recording Live Browser Flows

Turn your manual clicks and typing in Chrome directly into `.ferrite` code (press `Ctrl+C` when finished):

```bash
npx ferrite record --url https://todomvc.com/examples/react/dist/ --out tests/recorded.ferrite
```

---

## CLI Reference

| Command | Description |
| :--- | :--- |
| `npx ferrite init [dir]` | Scaffold a new `.ferrite` project (`ferrite.toml`, helpers, fixtures, starter test) |
| `npx ferrite check [path]` | Parse, validate, and type-check `.ferrite` suites without opening a browser |
| `npx ferrite compile [path]` | Compile `.ferrite` files into JSON `TestSuite` IR |
| `npx ferrite run [path]` | Execute `.ferrite` suites in live Chrome (`--update-baselines`, `--json`, `--snapshots-dir`) |
| `npx ferrite record` | Record live Chrome interactions into a `.ferrite` file |
| `npx ferrite setup --extension-id <ID>` | Register Chrome/Edge/Brave/Chromium Native Messaging host |
| `npx ferrite mcp` | Start the stdio MCP (JSON-RPC 2.0) server for AI agents |

---

## AI Agent Integration (`ferrite mcp`)

Ferrite includes a built-in **Model Context Protocol (MCP)** stdio server so AI assistants (Claude Desktop, Cursor, Antigravity, VS Code Copilot) can inspect attached Chrome tabs, run JavaScript (`browser_evaluate`), take low-token Accessibility snapshots (`browser_snapshot`), mock network requests (`browser_mock`), capture screenshots (`browser_screenshot`), and execute `.ferrite` tests (`browser_run_ferrite`).

Add to your AI client's MCP configuration:

```json
{
  "mcpServers": {
    "ferrite": {
      "command": "npx",
      "args": ["-y", "@halil07/ferrite", "mcp"]
    }
  }
}
```

---

## Running the TodoMVC Example

This repository includes a complete 5-suite / 11-case `.ferrite` project in [examples/todomvc](examples/todomvc) targeting `https://todomvc.com/examples/react/dist/`:

```bash
npx ferrite check examples/todomvc
npx ferrite run examples/todomvc
```
