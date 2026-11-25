# `@halil07/ferrite`

Playwright-style `.ferrite` E2E automation engine powered by a Chrome MV3 `chrome.debugger` extension, Chrome Native Messaging, and a Tokio Rust daemon.

## Installation & Quick Start

1. Install the **Ferrite E2E Bridge** extension from the Chrome Web Store (or load `extension/` unpacked in `chrome://extensions`).
2. Install the CLI in your project:
   ```bash
   npm install -D @halil07/ferrite
   ```
3. Register the Native Messaging host for Chrome using the extension ID:
   ```bash
   npx ferrite setup --extension-id <32_CHAR_EXTENSION_ID>
   ```
4. Initialize a `.ferrite` test project and run it:
   ```bash
   npx ferrite init
   npx ferrite check
   npx ferrite run
   ```

To compile `.ferrite` files into JSON `TestSuite` IR without executing:
```bash
npx ferrite compile tests/login.ferrite
```

## AI Agent MCP Server

Expose your attached Chrome tabs (JS evaluation, AX snapshots, clicks/fills, screenshots, console/network logs, and `.ferrite` script execution) to any MCP-compatible AI client (Claude Desktop, Cursor, Antigravity, VS Code):

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
