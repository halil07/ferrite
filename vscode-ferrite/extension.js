const vscode = require("vscode");
const fs = require("node:fs");
const path = require("node:path");

function makeItem(label, kind, insertSnippet, detail, documentation, triggerSuggest = false) {
  const item = new vscode.CompletionItem(label, kind);
  item.insertText = new vscode.SnippetString(insertSnippet);
  item.detail = detail;
  if (documentation) {
    item.documentation = new vscode.MarkdownString(documentation);
  }
  if (triggerSuggest) {
    item.command = {
      command: "editor.action.triggerSuggest",
      title: "Trigger Ferrite Suggestions",
    };
  }
  return item;
}

/**
 * Collects the active statement chain up to `position` (supporting multi-line fluent chains).
 */
function getStatementPrefix(document, position) {
  const maxLookback = Math.max(0, position.line - 12);
  const parts = [];
  for (let line = position.line; line >= maxLookback; line--) {
    const raw =
      line === position.line
        ? document.lineAt(line).text.slice(0, position.character)
        : document.lineAt(line).text;

    const cleaned = raw.replace(/\/\/.*$/, "");
    const semiIdx = cleaned.lastIndexOf(";");
    const braceIdx = cleaned.lastIndexOf("{");
    const cut = Math.max(semiIdx, braceIdx);
    if (cut !== -1) {
      parts.unshift(cleaned.slice(cut + 1));
      break;
    }
    parts.unshift(cleaned);
    if (/^\s*(?:await\s+)?(?:\$|\$text|\$role|@[A-Za-z0-9_-]+|console|mock)\b/.test(cleaned)) {
      break;
    }
  }
  return parts.join(" ").trim();
}

function detectSelectorKind(prefix) {
  if (/\$text\s*\(/.test(prefix)) return "text";
  if (/\$role\s*\(/.test(prefix)) return "role";
  if (/\$\s*\(\s*@/.test(prefix) || /(?:^|\bawait\s+)@[A-Za-z0-9_-]+\s*\./.test(prefix)) {
    return "ref";
  }
  if (/\$\s*\(/.test(prefix)) return "css";
  return null;
}

function collectMacrosAndVars(document) {
  const seenFiles = new Set();
  const macros = new Map();
  const vars = new Set();

  function scanText(text, baseDir) {
    const macroRe = /\b(?:macro|fn|function)\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(([^)]*)\)/g;
    let m;
    while ((m = macroRe.exec(text)) !== null) {
      const name = m[1];
      const params = m[2]
        .split(",")
        .map((p) => p.trim())
        .filter(Boolean);
      if (!macros.has(name)) {
        macros.set(name, params);
      }
    }

    const varRe = /\b(?:let|const)\s+([A-Za-z_][A-Za-z0-9_]*)\s*=/g;
    while ((m = varRe.exec(text)) !== null) {
      vars.add(m[1]);
    }

    if (!baseDir) return;
    const importRe = /\bimport\s*\(?\s*["']([^"']+)["']\s*\)?/g;
    while ((m = importRe.exec(text)) !== null) {
      const target = path.resolve(baseDir, m[1]);
      if (seenFiles.has(target)) continue;
      seenFiles.add(target);
      try {
        const imported = fs.readFileSync(target, "utf8");
        scanText(imported, path.dirname(target));
      } catch {
        // Ignore unresolved relative imports while user is typing.
      }
    }
  }

  const docPath = document.uri.scheme === "file" ? document.uri.fsPath : null;
  if (docPath) seenFiles.add(docPath);
  scanText(document.getText(), docPath ? path.dirname(docPath) : null);

  return { macros, vars };
}

function expectAssertionItems(selectorKind) {
  const items = [
    makeItem(
      "visible",
      vscode.CompletionItemKind.Method,
      "visible($1);$0",
      "expect().visible(timeout?)",
      "Assert that the selected element or text is visible (auto-waits up to timeout, default `5s`)."
    ),
    makeItem(
      "hidden",
      vscode.CompletionItemKind.Method,
      "hidden($1);$0",
      "expect().hidden(timeout?)",
      "Assert that the selected element or text is hidden or removed from the DOM."
    ),
  ];

  if (selectorKind === "css" || selectorKind === null) {
    items.push(
      makeItem(
        "count",
        vscode.CompletionItemKind.Method,
        "count(${1:1});$0",
        "expect().count(expected, timeout?)",
        "Assert exact `querySelectorAll` match count for the CSS selector."
      )
    );
  }

  items.push(
    makeItem(
      "timeout",
      vscode.CompletionItemKind.Method,
      "timeout(${1:5s}).",
      "expect().timeout(duration)",
      "Override assertion auto-wait timeout before calling `.visible()`, `.hidden()`, or `.count()`.",
      true
    )
  );

  return items;
}

function selectorActionItems(selectorKind) {
  const items = [];

  if (selectorKind === "css" || selectorKind === "role" || selectorKind === "ref") {
    items.push(
      makeItem(
        "click",
        vscode.CompletionItemKind.Method,
        "click($1)",
        ".click(timeout?)",
        "Click the selected element."
      ),
      makeItem(
        "fill",
        vscode.CompletionItemKind.Method,
        'fill("${1:text}")',
        '.fill("text", timeout?)',
        "Focus, clear, and fill text into the selected input."
      ),
      makeItem(
        "val",
        vscode.CompletionItemKind.Method,
        'val("${1:text}")',
        '.val("text", timeout?)',
        "jQuery alias for `.fill(\"text\")`."
      )
    );
  }

  if (selectorKind === "css" || selectorKind === "text" || selectorKind === "ref") {
    items.push(
      makeItem(
        "dblclick",
        vscode.CompletionItemKind.Method,
        "dblclick($1)",
        ".dblclick(timeout?)",
        "Double-click the selected element."
      ),
      makeItem(
        "hover",
        vscode.CompletionItemKind.Method,
        "hover($1)",
        ".hover(timeout?)",
        "Move the mouse pointer over the selected element."
      )
    );
  }

  items.push(
    makeItem(
      "press",
      vscode.CompletionItemKind.Method,
      'press("${1:Enter}")',
      '.press("key")',
      "Press a keyboard key (chainable after `.fill()` or `.dblclick()`)."
    ),
    makeItem(
      "type",
      vscode.CompletionItemKind.Method,
      'type("${1:text}")',
      '.type("text")',
      "Type raw text into the currently focused element."
    ),
    makeItem(
      "sleep",
      vscode.CompletionItemKind.Method,
      "sleep(${1:500ms})",
      ".sleep(duration)",
      "Pause the chain for a duration (e.g. `500ms`, `2s`)."
    )
  );

  if (selectorKind === "css" || selectorKind === "role" || selectorKind === "text") {
    items.push(
      makeItem(
        "eq",
        vscode.CompletionItemKind.Method,
        "eq(${1:0}).",
        ".eq(index)",
        "Select the zero-based Nth matched element (jQuery `.eq(n)`).",
        true
      ),
      makeItem(
        "first",
        vscode.CompletionItemKind.Method,
        "first().",
        ".first()",
        "Select the first matched element (`.eq(0)`).",
        true
      ),
      makeItem(
        "timeout",
        vscode.CompletionItemKind.Method,
        "timeout(${1:5s}).",
        ".timeout(duration)",
        "Set the auto-wait timeout for the chained action or assertion.",
        true
      )
    );
  }

  if (selectorKind === "text" || selectorKind === "role") {
    items.push(
      makeItem(
        "exact",
        vscode.CompletionItemKind.Method,
        "exact().",
        ".exact()",
        "Require an exact full-string match on text or accessible name.",
        true
      )
    );
  }

  if (selectorKind === "css" || selectorKind === "text") {
    items.push(
      makeItem(
        "expect",
        vscode.CompletionItemKind.Method,
        "expect().",
        ".expect()",
        "Start an assertion chain (`.visible()`, `.hidden()`, `.count(n)`).",
        true
      )
    );
  }

  return items;
}

function consoleMethodItems() {
  return [
    makeItem(
      "start",
      vscode.CompletionItemKind.Method,
      "start();$0",
      "console.start()",
      "Start capturing browser console messages and uncaught exceptions."
    ),
    makeItem(
      "clear",
      vscode.CompletionItemKind.Method,
      "clear();$0",
      "console.clear()",
      "Clear previously captured console messages."
    ),
    makeItem(
      "noErrors",
      vscode.CompletionItemKind.Method,
      "noErrors();$0",
      "console.noErrors()",
      "Assert that zero console errors or uncaught JS exceptions occurred."
    ),
    makeItem(
      "expect",
      vscode.CompletionItemKind.Method,
      'expect("${1:text}", "${2|info,log,warn,error,debug|}", ${3:5s});$0',
      'console.expect("text", level?, timeout?)',
      "Assert that a console entry containing text appears."
    ),
  ];
}

function mockMethodItems() {
  return [
    makeItem(
      "when",
      vscode.CompletionItemKind.Method,
      'when("${1|GET,POST,PUT,PATCH,DELETE|}", "${2:/api/users/:id/orders}").',
      'mock.when("METHOD", "url")',
      "Start a fluent service mock rule (chain `.params()`, `.data()`, `.reply()`, `.delay()`, `.fail()`, `.abort()`).",
      true
    ),
    makeItem(
      "load",
      vscode.CompletionItemKind.Method,
      'load("${1:./mocks/scenario.mock.json}");$0',
      'mock.load("file.mock.json")',
      "Load an external mock scenario JSON file with multiple service rules."
    ),
    makeItem(
      "clear",
      vscode.CompletionItemKind.Method,
      "clear();$0",
      "mock.clear()",
      "Clear all active Fetch mock routes on the current tab."
    ),
  ];
}

function mockChainItems() {
  return [
    makeItem(
      "params",
      vscode.CompletionItemKind.Method,
      'params({ ${1:id}: "${2:1}" }).',
      ".params({ key: value })",
      "Provide `:param` path values and query parameter subset to match.",
      true
    ),
    makeItem(
      "data",
      vscode.CompletionItemKind.Method,
      'data({ ${1:key}: "${2:value}" }).',
      ".data({ ... })",
      "Match JSON request body as a recursive subset (extra fields ignored).",
      true
    ),
    makeItem(
      "reply",
      vscode.CompletionItemKind.Method,
      'reply(${1:200}, "${2:./responses/mock.json}");$0',
      '.reply(status, "response.json")',
      "Respond with HTTP status and a pure external response file."
    ),
    makeItem(
      "replyJson",
      vscode.CompletionItemKind.Method,
      "replyJson(${1:200}, { ${2:ok}: true });$0",
      ".replyJson(status, { ... })",
      "Respond with HTTP status and inline JSON object."
    ),
    makeItem(
      "delay",
      vscode.CompletionItemKind.Method,
      "delay(${1:300ms})",
      ".delay(duration)",
      "Simulate network latency (e.g. `300ms`, `2s`) without blocking other requests."
    ),
    makeItem(
      "header",
      vscode.CompletionItemKind.Method,
      'header("${1:x-mock}", "${2:true}").',
      '.header("name", "value")',
      "Add a response header.",
      true
    ),
    makeItem(
      "headers",
      vscode.CompletionItemKind.Method,
      'headers({ "${1:x-mock}": "${2:true}" }).',
      ".headers({ ... })",
      "Add multiple response headers.",
      true
    ),
    makeItem(
      "pattern",
      vscode.CompletionItemKind.Method,
      'pattern("${1:*://api.example.com/*}").',
      '.pattern("wildcard")',
      "Restrict CDP Fetch interception URL pattern.",
      true
    ),
    makeItem(
      "fail",
      vscode.CompletionItemKind.Method,
      'fail("${1|TimedOut,Failed,Aborted,ConnectionRefused,ConnectionReset,InternetDisconnected|}");$0',
      '.fail("TimedOut")',
      "Simulate a network-level failure (CDP `Network.ErrorReason`)."
    ),
    makeItem(
      "abort",
      vscode.CompletionItemKind.Method,
      "abort();$0",
      ".abort()",
      "Abort the matched network request (`Aborted`)."
    ),
  ];
}

function topLevelAndBuiltinItems(document) {
  const items = [
    makeItem(
      "$",
      vscode.CompletionItemKind.Function,
      '$("${1:.selector}").',
      '$("css-selector")',
      "Select DOM element(s) via CSS (`querySelectorAll`, supports `:eq(n)` and `:first`).",
      true
    ),
    makeItem(
      "$text",
      vscode.CompletionItemKind.Function,
      '$text("${1:Visible text}").',
      '$text("text")',
      "Select an element by visible text in the Accessibility Tree.",
      true
    ),
    makeItem(
      "$role",
      vscode.CompletionItemKind.Function,
      '$role("${1|button,textbox,checkbox,link,heading,tab|}", "${2:Name}").',
      '$role("role", "name"?)',
      "Select an element by ARIA role and optional accessible name.",
      true
    ),
    makeItem("await", vscode.CompletionItemKind.Keyword, "await ", "await", "Await a browser step."),
    makeItem(
      "goto",
      vscode.CompletionItemKind.Function,
      'goto("${1:https://example.com}");$0',
      'goto("url", wait?, timeout?)',
      "Navigate the active tab to a URL."
    ),
    makeItem(
      "sleep",
      vscode.CompletionItemKind.Function,
      "sleep(${1:1s});$0",
      "sleep(duration)",
      "Wait for a duration (e.g. `500ms`, `5s`)."
    ),
    makeItem(
      "reload",
      vscode.CompletionItemKind.Function,
      "reload($1);$0",
      "reload(ignoreCache?)",
      "Reload the current page."
    ),
    makeItem(
      "press",
      vscode.CompletionItemKind.Function,
      'press("${1|Enter,Escape,Tab,Backspace,Control+a|}");$0',
      'press("key")',
      "Send a keyboard key press."
    ),
    makeItem(
      "type",
      vscode.CompletionItemKind.Function,
      'type("${1:text}");$0',
      'type("text")',
      "Insert raw text into the focused element."
    ),
    makeItem(
      "snapshot",
      vscode.CompletionItemKind.Function,
      'snapshot("${1|interactive,full|}");$0',
      'snapshot("interactive" | "full")',
      "Capture an Accessibility Tree snapshot with `@e1`, `@e2` refs."
    ),
    makeItem(
      "screenshot",
      vscode.CompletionItemKind.Function,
      'screenshot("${1:baseline-name}", ${2:0.1}, ${3:0.005});$0',
      'screenshot("name", threshold?, maxDiff?)',
      "Assert visual regression against baseline PNG."
    ),
    makeItem(
      "console",
      vscode.CompletionItemKind.Module,
      "console.",
      "console",
      "Console capture and error assertions (`start`, `clear`, `noErrors`, `expect`).",
      true
    ),
    makeItem(
      "mock",
      vscode.CompletionItemKind.Function,
      'mock("${1|GET,POST,PUT,PATCH,DELETE|}", "${2:real_service}", "${3:mock.json}");$0',
      'mock("METHOD", "url", "file.json", status?)',
      "Intercept matching service requests and return a mock fixture file or custom response."
    ),
    makeItem(
      "cdp",
      vscode.CompletionItemKind.Function,
      'cdp("${1:Page.bringToFront}", ${2:{}});$0',
      'cdp("Domain.method", params?)',
      "Send a raw Chrome DevTools Protocol command."
    ),
    makeItem(
      "tab",
      vscode.CompletionItemKind.Function,
      "tab(${1:1});$0",
      "tab(tabId)",
      "Bind the current test case to a specific Chrome tab ID."
    ),
    makeItem(
      "suite",
      vscode.CompletionItemKind.Keyword,
      'suite("${1:my-suite}");$0',
      'suite("name");',
      "Declare the test suite name."
    ),
    makeItem(
      "import",
      vscode.CompletionItemKind.Keyword,
      'import "${1:./helpers/common.ferrite}";$0',
      'import "path";',
      "Import macros and variables from another `.ferrite` file."
    ),
    makeItem(
      "let",
      vscode.CompletionItemKind.Keyword,
      'let ${1:name} = "${2:value}";$0',
      'let name = "value";',
      "Define a string variable for `${name}` interpolation."
    ),
    makeItem(
      "beforeEach",
      vscode.CompletionItemKind.Keyword,
      "beforeEach {\n  $0\n}",
      "beforeEach { ... }",
      "Steps executed before every `test(...)` in this suite."
    ),
    makeItem(
      "macro",
      vscode.CompletionItemKind.Keyword,
      "macro ${1:helper_name}(${2:param}) {\n  $0\n}",
      "macro name(params) { ... }",
      "Define a reusable macro callable like a function."
    ),
    makeItem(
      "test",
      vscode.CompletionItemKind.Keyword,
      'test("${1:test-case-name}") {\n  $0\n}',
      'test("name") { ... }',
      "Define an E2E test case."
    ),
  ];

  const { macros, vars } = collectMacrosAndVars(document);
  for (const [name, params] of macros.entries()) {
    const placeholders = params
      .map((p, idx) => '"${' + (idx + 1) + ":" + p + '}"')
      .join(", ");
    items.push(
      makeItem(
        name,
        vscode.CompletionItemKind.Function,
        `${name}(${placeholders});$0`,
        `macro ${name}(${params.join(", ")})`,
        "User-defined `.ferrite` macro."
      )
    );
  }
  for (const v of vars) {
    items.push(
      makeItem(
        v,
        vscode.CompletionItemKind.Variable,
        "\\${" + v + "}",
        `let ${v}`,
        "Interpolated `.ferrite` variable."
      )
    );
  }

  return items;
}

function activate(context) {
  const provider = vscode.languages.registerCompletionItemProvider(
    "ferrite",
    {
      provideCompletionItems(document, position) {
        const prefix = getStatementPrefix(document, position);

        // 1. Right after `.expect().` (or `.expect(10s).timeout(5s).`) -> ONLY assertions!
        if (/\.expect\s*\([^)]*\)\s*(?:\.timeout\s*\([^)]*\)\s*)*\.\s*[A-Za-z_]*$/.test(prefix)) {
          const kind = detectSelectorKind(prefix);
          return expectAssertionItems(kind);
        }

        // 2. Right after `console.`
        if (/(?:^|\bawait\s+)console\.\s*[A-Za-z_]*$/.test(prefix)) {
          return consoleMethodItems();
        }

        // 3. Right after `mock.` or inside a `mock.when(...).` chain
        if (/(?:^|\bawait\s+)mock\.when\s*\(/.test(prefix) && /\.\s*[A-Za-z_]*$/.test(prefix)) {
          return mockChainItems();
        }
        if (/(?:^|\bawait\s+)mock\.\s*[A-Za-z_]*$/.test(prefix)) {
          return mockMethodItems();
        }

        // 4. Right after a selector chain `$(...).`, `$text(...).`, `$role(...).`, `@e1.`
        const selectorKind = detectSelectorKind(prefix);
        if (selectorKind && /\.\s*[A-Za-z_]*$/.test(prefix)) {
          return selectorActionItems(selectorKind);
        }

        // 5. General statement / top-level completion (never triggered by bare `.`)
        if (prefix.endsWith(".")) {
          return [];
        }
        return topLevelAndBuiltinItems(document);
      },
    },
    ".",
    "$"
  );

  context.subscriptions.push(provider);
}

function deactivate() {}

module.exports = { activate, deactivate };
