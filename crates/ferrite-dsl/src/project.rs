#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};

use ferrite_daemon::TestSuite;

use crate::compiler::compile_file;
use crate::error::{DslError, Span};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectConfig {
    pub name: String,
    pub test_dir: PathBuf,
    pub snapshots_dir: PathBuf,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            name: "ferrite-e2e-project".to_string(),
            test_dir: PathBuf::from("tests"),
            snapshots_dir: PathBuf::from(".ferrite-snapshots"),
        }
    }
}

pub fn load_project_config(dir_or_file: &Path) -> Result<Option<(PathBuf, ProjectConfig)>, DslError> {
    let start_dir = if dir_or_file.is_file() {
        dir_or_file.parent().unwrap_or_else(|| Path::new("."))
    } else {
        dir_or_file
    };

    let mut current = Some(start_dir);
    let mut found_dir: Option<PathBuf> = None;
    while let Some(dir) = current {
        if dir.join("ferrite.toml").exists() {
            found_dir = Some(dir.to_path_buf());
            break;
        }
        current = dir.parent();
    }

    let Some(project_dir) = found_dir else {
        return Ok(None);
    };
    let manifest_path = project_dir.join("ferrite.toml");

    let raw = fs::read_to_string(&manifest_path).map_err(|source| DslError::Io {
        path: manifest_path.clone(),
        source,
    })?;

    let mut cfg = ProjectConfig::default();
    let label = manifest_path.to_string_lossy().into_owned();

    for (idx, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with("//")
            || (trimmed.starts_with('[') && trimmed.ends_with(']'))
        {
            continue;
        }
        let Some((k, v)) = trimmed.split_once('=') else {
            return Err(DslError::Parse {
                file: label,
                span: Span {
                    line: idx + 1,
                    col: 1,
                },
                message: format!("expected `key = \"value\"` in ferrite.toml, found `{trimmed}`"),
            });
        };
        let key = k.trim();
        let val = v.trim().trim_matches('"').trim_matches('\'');
        match key {
            "name" => cfg.name = val.to_string(),
            "test_dir" => cfg.test_dir = PathBuf::from(val),
            "snapshots_dir" => cfg.snapshots_dir = PathBuf::from(val),
            _ => {}
        }
    }

    Ok(Some((project_dir, cfg)))
}

/// Compiles a `.ferrite` file, a `.json` `TestSuite`, or a project/test directory into runnable
/// `TestSuite` values. Helper-only `.ferrite` files (with zero `test` cases) are validated and
/// omitted from the runnable list.
pub fn compile_path(path: &Path) -> Result<Vec<(PathBuf, TestSuite)>, DslError> {
    if path.is_file() {
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        if ext.eq_ignore_ascii_case("json") {
            let raw = fs::read_to_string(path).map_err(|source| DslError::Io {
                path: path.to_path_buf(),
                source,
            })?;
            let suite: TestSuite = serde_json::from_str(&raw).map_err(|err| DslError::Parse {
                file: path.to_string_lossy().into_owned(),
                span: Span { line: 1, col: 1 },
                message: format!("invalid JSON TestSuite: {err}"),
            })?;
            return Ok(vec![(path.to_path_buf(), suite)]);
        }

        let suite = compile_file(path)?;
        return Ok(vec![(path.to_path_buf(), suite)]);
    }

    if !path.is_dir() {
        return Err(DslError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "target path does not exist",
            ),
        });
    }

    let search_root = if let Some((root, cfg)) = load_project_config(path)? {
        let candidate = root.join(&cfg.test_dir);
        if candidate.exists() {
            candidate
        } else {
            path.to_path_buf()
        }
    } else {
        path.to_path_buf()
    };

    let mut ferrite_files = Vec::new();
    collect_ferrite_files(&search_root, &mut ferrite_files)?;
    ferrite_files.sort();

    let mut runnable = Vec::new();
    for file in ferrite_files {
        let suite = compile_file(&file)?;
        if !suite.cases.is_empty() {
            runnable.push((file, suite));
        }
    }

    Ok(runnable)
}

fn collect_ferrite_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), DslError> {
    let entries = fs::read_dir(dir).map_err(|source| DslError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| DslError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let p = entry.path();
        if p.is_dir() {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if matches!(name, "node_modules" | "target" | ".git" | ".ferrite-snapshots") {
                continue;
            }
            collect_ferrite_files(&p, out)?;
        } else if p.extension().and_then(|s| s.to_str()) == Some("ferrite") {
            out.push(p);
        }
    }
    Ok(())
}

/// Generates a starter `.ferrite` E2E project layout inside `dir`.
pub fn default_project_scaffold(dir: &Path, force: bool) -> Result<Vec<PathBuf>, DslError> {
    let files: Vec<(&str, &str)> = vec![
        (
            "ferrite.toml",
            "name = \"my-ferrite-e2e\"\ntest_dir = \"tests\"\nsnapshots_dir = \".ferrite-snapshots\"\n",
        ),
        (
            ".gitignore",
            ".ferrite-snapshots/actual/\n.ferrite-snapshots/diffs/\nnode_modules/\n",
        ),
        (
            "tests/fixtures/login.html",
            "<!doctype html>\n<html lang=\"en\">\n  <head>\n    <meta charset=\"utf-8\" />\n    <title>Ferrite Portal</title>\n    <style>\n      body { font-family: system-ui, sans-serif; padding: 32px; background: #f8fafc; color: #0f172a; }\n      main { max-width: 420px; margin: auto; background: #fff; padding: 24px; border: 1px solid #cbd5e1; border-radius: 12px; }\n      label, input, button { display: block; width: 100%; margin-top: 10px; box-sizing: border-box; }\n      input, button { padding: 10px; font-size: 14px; }\n      button { background: #0f172a; color: #fff; border: 0; border-radius: 6px; cursor: pointer; }\n    </style>\n  </head>\n  <body>\n    <main>\n      <h1>Ferrite Portal</h1>\n      <form aria-label=\"Login\">\n        <label for=\"email\">Email</label>\n        <input id=\"email\" type=\"email\" placeholder=\"name@example.com\" />\n        <button id=\"signin\" type=\"button\">Sign In</button>\n      </form>\n      <section id=\"out\" aria-live=\"polite\"></section>\n    </main>\n    <script>\n      document.getElementById('signin').addEventListener('click', async () => {\n        const email = document.getElementById('email').value;\n        const res = await fetch('/api/session');\n        const data = await res.json();\n        const h = document.createElement('h2');\n        h.textContent = 'Dashboard Ready';\n        const p = document.createElement('p');\n        p.textContent = 'Welcome ' + data.user + ' (' + email + ')';\n        document.getElementById('out').replaceChildren(h, p);\n      });\n    </script>\n  </body>\n</html>\n",
        ),
        (
            "tests/helpers/auth.ferrite",
            "// Shared reusable macros & variables for authentication flows\nlet default_user = \"ada\";\nlet default_email = \"ada@example.com\";\n\nmacro mock_login_portal(domain) {\n  await mock(\"GET\", \"/api/session\", {\n    status: 200,\n    pattern: \"*://${domain}/*\",\n    headers: { \"content-type\": \"application/json\" },\n    body: \"{\\\"authenticated\\\":true,\\\"user\\\":\\\"${default_user}\\\"}\"\n  });\n\n  await mock(\"GET\", \"/login\", {\n    status: 200,\n    pattern: \"*://${domain}/*\",\n    headers: { \"content-type\": \"text/html; charset=utf-8\" },\n    file: \"../fixtures/login.html\"\n  });\n}\n\nmacro sign_in(email) {\n  await $(\"#email\").fill(\"${email}\");\n  await $(\"#signin\").click();\n}\n",
        ),
        (
            "tests/login.ferrite",
            "suite(\"ferrite-login-e2e-demo\");\nimport \"./helpers/auth.ferrite\";\n\nlet target_domain = \"example.com\";\n\nbeforeEach {\n  await mock_login_portal(\"${target_domain}\");\n  await goto(\"https://${target_domain}/login\", \"load\", 15s);\n}\n\ntest(\"authenticate-and-verify-dashboard\") {\n  await sign_in(\"${default_email}\");\n  await $text(\"Dashboard Ready\").expect().visible();\n  await $text(\"Welcome ${default_user} (${default_email})\").expect().visible();\n  await screenshot(\"dashboard-ready\", 0.1, 0.001);\n}\n",
        ),
    ];

    let mut written = Vec::new();
    for (rel, content) in files {
        let dest = dir.join(rel);
        if dest.exists() && !force {
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|source| DslError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        fs::write(&dest, content).map_err(|source| DslError::Io {
            path: dest.clone(),
            source,
        })?;
        written.push(dest);
    }

    Ok(written)
}
