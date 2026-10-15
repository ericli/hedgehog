//! The few HTML pages the gateway serves itself, in the same quiet style as
//! the dashboard: white page, black type, hairlines, square blocks.

const STYLE: &str = r#"
:root {
  --gray0: #FFF; --gray1: #F7F7F7; --gray2: #EDEDED; --gray3: #DEDEDE; --gray4: #999; --gray5: #696969; --gray7: #000;
  --red1: #FAF4F3; --red2: #DFBEBE; --red3: #B93D3D; --green1: #F4F8F3; --green2: #B4D6B3; --green3: #238020;
}
@media (prefers-color-scheme: dark) {
  :root {
    --gray0: #000; --gray1: #1A1A1A; --gray2: #333; --gray3: #4F4F4F; --gray4: #696969; --gray5: #B2B2B2; --gray7: #FFF;
    --red1: #1A0404; --red2: #412020; --red3: #EB6864; --green1: #121D12; --green2: #2A4C29; --green3: #98DC89;
  }
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--gray0); color: var(--gray7);
  font: 0.9rem/1.45 "Helvetica Neue", Helvetica, Arial, sans-serif; }
.top { padding: 15px 20px; font-weight: bold; }
main { max-width: 1240px; margin: 0 auto; padding: 0 20px 80px; }
.channel { text-align: center; padding: 45px 0 35px; }
h1 { margin: 0; font-size: 2rem; font-weight: normal; letter-spacing: -0.01em; }
.channel p { margin: 6px 0 0; color: var(--gray5); }
.note { max-width: 420px; margin: 12vh auto 0; text-align: center; }
.note h1 { font-size: 1.5rem; margin-bottom: 12px; }
.note p { color: var(--gray5); margin: 0 0 10px; }
code { font-family: ui-monospace, "SF Mono", Menlo, monospace; font-size: 0.9em; color: var(--gray7); }
.grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(220px, 1fr)); gap: 35px 25px; }
.block { display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 6px;
  aspect-ratio: 1 / 1; padding: 20px; border: 1px solid var(--gray2); text-align: center; text-decoration: none; color: var(--gray5); }
.block .title { font-size: 1.2rem; overflow-wrap: anywhere; }
.block .state { font-size: 0.78125rem; }
.block:hover { border-color: var(--gray4); }
.block.private { background: var(--red1); border-color: var(--red2); color: var(--red3); }
.block.public { background: var(--green1); border-color: var(--green2); color: var(--green3); }
.block.private:hover { border-color: var(--red3); }
.block.public:hover { border-color: var(--green3); }
.caption { margin-top: 8px; font-size: 0.78125rem; color: var(--gray5); }
@media (max-width: 520px) {
  main { padding: 0 15px 60px; }
  .grid { grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 25px 15px; }
  .block { padding: 12px; }
  .block .title { font-size: 1rem; }
}
"#;

/// An app as listed on the home page for devices that open apps.
pub struct AppLink {
    pub name: String,
    pub url: String,
    pub public: bool,
    pub running: bool,
}

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
         <meta name=\"color-scheme\" content=\"light dark\">\
         <title>{}</title><style>{STYLE}</style></head>\
         <body><div class=\"top\">🦔 hedgehog</div><main>{body}</main></body></html>",
        escape(title)
    )
}

fn note(title: &str, paragraphs: &[&str]) -> String {
    let body: String = paragraphs
        .iter()
        .map(|text| format!("<p>{text}</p>"))
        .collect();
    page(
        title,
        &format!("<div class=\"note\"><h1>{}</h1>{body}</div>", escape(title)),
    )
}

pub fn wall() -> String {
    note(
        "This is a private app",
        &[
            "It's running on someone's hedgehog, and this device isn't paired with it.",
            "If it's yours, run <code>hedgehog pair</code> on your Mac and open the link it prints on this device.",
        ],
    )
}

pub fn unavailable(app: &str, reason: &str) -> String {
    note(
        &format!("{app} isn't running"),
        &[
            &escape(reason),
            &format!(
                "Check <code>hedgehog ls</code> and <code>hedgehog logs {}</code>.",
                escape(app)
            ),
        ],
    )
}

pub fn pair_failed() -> String {
    note(
        "That pairing link didn't work",
        &[
            "Links work once and expire after 10 minutes. Run <code>hedgehog pair</code> again for a new one.",
        ],
    )
}

pub fn home(apps: &[AppLink]) -> String {
    let count = match apps.len() {
        0 => "No apps yet".to_string(),
        1 => "1 app".to_string(),
        n => format!("{n} apps"),
    };
    let blocks: String = apps
        .iter()
        .map(|app| {
            let access = if app.public { "public" } else { "private" };
            let state = if app.running { "open →" } else { "not running" };
            format!(
                "<div><a class=\"block {access}\" href=\"{url}\"><span class=\"title\">{name}</span>\
                 <span class=\"state\">{state}</span></a><div class=\"caption\">{access}</div></div>",
                url = escape(&app.url),
                name = escape(&app.name),
            )
        })
        .collect();
    page(
        "hedgehog",
        &format!(
            "<header class=\"channel\"><h1>Apps</h1><p>{count} on this hedgehog</p></header>\
             <div class=\"grid\">{blocks}</div>"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_app_names_and_urls() {
        let html = home(&[AppLink {
            name: "<script>".into(),
            url: "https://x\"onclick=\"y".into(),
            public: false,
            running: true,
        }]);
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("x&quot;onclick"));
        assert!(unavailable("<b>", "<i>").contains("&lt;i&gt;"));
    }
}
