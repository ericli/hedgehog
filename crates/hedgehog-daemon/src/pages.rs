//! The few HTML pages the gateway serves itself.

const STYLE: &str = r#"
:root { --bg: #fbfaf7; --fg: #1d1b16; --muted: #6b665c; --card: #ffffff; --line: #e6e1d6; --accent: #8a5a2b; }
@media (prefers-color-scheme: dark) {
  :root { --bg: #171512; --fg: #f1ece2; --muted: #a39d90; --card: #211e1a; --line: #37322b; --accent: #d9a066; }
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--fg); font: 16px/1.5 -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
main { max-width: 560px; margin: 0 auto; padding: 48px 16px; }
h1 { font-size: 22px; margin: 0 0 4px; }
p { color: var(--muted); margin: 0 0 16px; }
code { background: var(--card); border: 1px solid var(--line); border-radius: 6px; padding: 1px 6px; font-size: 14px; }
ul { list-style: none; padding: 0; margin: 24px 0 0; }
li a { display: flex; justify-content: space-between; gap: 12px; padding: 14px 16px; margin-bottom: 8px; background: var(--card);
  border: 1px solid var(--line); border-radius: 10px; color: var(--fg); text-decoration: none; }
li a:hover { border-color: var(--accent); }
.tag { color: var(--muted); font-size: 14px; }
"#;

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{}</title><style>{STYLE}</style></head><body><main>{body}</main></body></html>",
        escape(title)
    )
}

pub fn wall() -> String {
    page(
        "Private app",
        "<h1>🦔 This is a private app</h1>\
         <p>It's running on someone's hedgehog, and this device isn't paired with it.</p>\
         <p>If it's yours, run <code>hedgehog pair</code> on your Mac and open the link it prints on this device.</p>",
    )
}

pub fn unavailable(app: &str, reason: &str) -> String {
    page(
        app,
        &format!(
            "<h1>🦔 {} is not running</h1><p>{}</p><p>Check <code>hedgehog ls</code> and <code>hedgehog logs {}</code>.</p>",
            escape(app),
            escape(reason),
            escape(app)
        ),
    )
}

pub fn pair_failed() -> String {
    page(
        "Pairing failed",
        "<h1>🦔 That pairing link didn't work</h1>\
         <p>Links work once and expire after 10 minutes. Run <code>hedgehog pair</code> again for a new one.</p>",
    )
}

pub fn home(apps: &[(String, String, &'static str)]) -> String {
    let items: String = if apps.is_empty() {
        "<p>No apps yet. Drop a project folder into <code>~/Hedgehog/apps</code>, or run <code>hedgehog deploy</code> in one.</p>"
            .to_string()
    } else {
        let rows: String = apps
            .iter()
            .map(|(name, url, status)| {
                format!(
                    "<li><a href=\"{}\"><span>{}</span><span class=\"tag\">{}</span></a></li>",
                    escape(url),
                    escape(name),
                    status
                )
            })
            .collect();
        format!("<ul>{rows}</ul>")
    };
    page(
        "hedgehog",
        &format!("<h1>🦔 hedgehog</h1><p>Your apps on this Mac.</p>{items}"),
    )
}
