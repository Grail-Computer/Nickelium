---
name: nickelium
description: Use when the user wants an agent-first browser runtime for authenticated SaaS flows, DOM extraction, dashboard audits, form automation, or proof screenshots with the built-in `nickelium` CLI. Best for DOM-first back-office work, not media-heavy browsing.
---

# Nickelium

Use this skill when the task is web automation and the browser should be optimized for agents instead of people.

Nickelium is a Servo-derived runtime with a native agent control path, lean task profiles, and a CLI that can run one isolated browser instance per workflow.

If `nickelium` is not already on PATH, run [scripts/install_runtime.sh](scripts/install_runtime.sh).

Core commands:

```bash
nickelium start
nickelium open https://example.com
nickelium wait "input[name=email]"
nickelium fill "input[name=email]" "user@example.com"
nickelium click "button[type=submit]"
nickelium text "h1"
nickelium eval "return document.title"
nickelium screenshot /tmp/page.png
nickelium shutdown
```

Use `--instance <name>` when you need isolated concurrent sessions:

```bash
nickelium --instance qa-1 start
nickelium --instance qa-2 start
```

Use `nickelium workflow <path>` for repeatable JSON workflows.

What Nickelium is optimized for:
- authenticated SaaS and admin flows
- DOM-first extraction and verification
- dashboard audits
- deterministic form workflows
- proof screenshots

What it intentionally trades away in lean profiles:
- media-heavy browsing
- visual review of full-fidelity consumer sites
- Chrome-specific compatibility edge cases

Use the `workflow` profile for back-office automation, `dom-audit` for extraction-heavy work, `visual-proof` when screenshots matter more than raw memory, and `compat-signin` only when a fragile auth flow needs the broader runtime.
