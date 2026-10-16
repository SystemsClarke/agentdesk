//! A packet as a web page: server-rendered HTML for `GET /packet/<id>` on the ops console (`web.rs`), live from the board. John ticks steps,
//! chooses, adds notes and finishes it here; each action is `kinds::step` / `note` / `finish`, the same calls the window makes, so the
//! agent that asked is woken exactly as before. The page polls `/packet/<id>/version` and swaps itself when the agent revises the packet.
//!
//! Everything from the board is data: it is escaped, links are kept only when they are http(s), and the page runs no script from the board.

use crate::db::Db;
use crate::kinds;
use anyhow::Result;
use serde_json::Value;

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

fn safe_url(u: &str) -> bool {
    let l = u.trim().to_ascii_lowercase();
    (l.starts_with("https://") || l.starts_with("http://")) && !u.contains(char::is_whitespace)
}

/// `**bold**`, `` `code` `` and `[text](http-url)`; everything else is text.
pub fn inline(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut o = String::new();
    let mut i = 0;
    let find = |from: usize, pat: &[char]| -> Option<usize> { (from..c.len().saturating_sub(pat.len() - 1)).find(|&j| c[j..j + pat.len()] == *pat) };
    while i < c.len() {
        if c[i] == '*' && c.get(i + 1) == Some(&'*') {
            if let Some(j) = find(i + 2, &['*', '*']).filter(|j| *j > i + 2) {
                o.push_str(&format!("<strong>{}</strong>", inline(&c[i + 2..j].iter().collect::<String>())));
                i = j + 2;
                continue;
            }
        } else if c[i] == '`' {
            if let Some(j) = find(i + 1, &['`']).filter(|j| *j > i + 1) {
                o.push_str(&format!("<code>{}</code>", esc(&c[i + 1..j].iter().collect::<String>())));
                i = j + 1;
                continue;
            }
        } else if c[i] == '[' {
            if let Some(m) = find(i + 1, &[']', '(']) {
                if let Some(e) = find(m + 2, &[')']) {
                    let (label, url) = (c[i + 1..m].iter().collect::<String>(), c[m + 2..e].iter().collect::<String>());
                    if safe_url(&url) {
                        o.push_str(&format!("<a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">{}</a>", esc(&url), esc(&label)));
                        i = e + 1;
                        continue;
                    }
                }
            }
        }
        o.push_str(&esc(&c[i].to_string()));
        i += 1;
    }
    o
}

/// Paragraphs, `-` lists and fenced code (with a Copy button), from the Markdown an agent writes.
pub fn blocks(md: &str) -> String {
    let mut o = String::new();
    let (mut para, mut list): (Vec<String>, Vec<String>) = (vec![], vec![]);
    let (mut fence, mut code, mut indent): (Option<String>, Vec<String>, usize) = (None, vec![], 0);
    let flush = |o: &mut String, para: &mut Vec<String>, list: &mut Vec<String>| {
        if !para.is_empty() {
            o.push_str(&format!("<p>{}</p>", para.join("<br>")));
            para.clear();
        }
        if !list.is_empty() {
            o.push_str(&format!("<ul>{}</ul>", list.iter().map(|l| format!("<li>{l}</li>")).collect::<String>()));
            list.clear();
        }
    };
    for line in md.lines() {
        let t = line.trim_start();
        if let Some(lang) = &fence {
            if t.starts_with("```") {
                let text = code.join("\n");
                o.push_str(&format!(
                    "<div class=\"code\"><div class=\"codebar\"><span>{}</span><button type=\"button\" data-copy>Copy</button></div><pre><code>{}</code></pre></div>",
                    esc(lang),
                    esc(&text)
                ));
                fence = None;
                code.clear();
            } else {
                let lead = line.len() - t.len();
                code.push(line[lead.min(indent)..].to_string());
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("```") {
            flush(&mut o, &mut para, &mut list);
            fence = Some(rest.trim().to_string());
            indent = line.len() - t.len();
            continue;
        }
        if t.is_empty() {
            flush(&mut o, &mut para, &mut list);
        } else if let Some(item) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            if !para.is_empty() {
                flush(&mut o, &mut para, &mut list);
            }
            list.push(inline(item));
        } else {
            if !list.is_empty() {
                flush(&mut o, &mut para, &mut list);
            }
            para.push(inline(t.trim_start_matches("> ")));
        }
    }
    if let Some(lang) = fence {
        o.push_str(&format!("<div class=\"code\"><div class=\"codebar\"><span>{}</span></div><pre><code>{}</code></pre></div>", esc(&lang), esc(&code.join("\n"))));
    }
    flush(&mut o, &mut para, &mut list);
    o
}

fn meta_of(doc: &Value) -> Value {
    doc["thread"]["meta"].as_str().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null)
}

fn is_packet(doc: &Value) -> bool {
    doc["thread"]["channel"] == "question" && kinds::kind_in(Some(&meta_of(doc))) == kinds::PACKET
}

/// A token that changes when anything on the packet does (its spec, John's progress, its status, a new message): the page compares it.
pub fn version(db: &Db, id: i64) -> Result<String> {
    use std::hash::{Hash, Hasher};
    let doc = db.get_thread(id)?;
    if !is_packet(&doc) {
        anyhow::bail!("#{id} is not a packet");
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (
        doc["thread"]["status"].as_str(),
        doc["thread"]["meta"].as_str(),
        doc["messages"].as_array().map(|m| m.len()),
        doc["messages"].as_array().and_then(|m| m.last()).map(|m| m["id"].as_i64()),
    )
        .hash(&mut h);
    Ok(format!("{:x}", h.finish()))
}

fn when(ts: &str) -> String {
    ts.replace('T', " ").chars().take(16).collect()
}

/// The body of the page (inside `#app`): what the packet says, the steps as controls, the agent's activity.
pub fn app(db: &Db, id: i64) -> Result<String> {
    let doc = db.get_thread(id)?;
    if !is_packet(&doc) {
        anyhow::bail!("#{id} is not a packet");
    }
    let meta = meta_of(&doc);
    let spec = &meta["packet"];
    let status = doc["thread"]["status"].as_str().unwrap_or("");
    let open = status == "open";
    let dis = if open { "" } else { " disabled" };
    let (done, total) = kinds::progress(&meta);
    let asker = doc["thread"]["opened_by"].as_str().unwrap_or("an agent");
    let mut h = String::new();

    h.push_str(&format!(
        "<header><div class=\"pill\">PACKET</div><h1>{}</h1><div class=\"by\">from <b>{}</b> · #{id}</div>",
        esc(doc["thread"]["subject"].as_str().unwrap_or("")),
        esc(asker)
    ));
    let pct = (done * 100).checked_div(total).unwrap_or(0);
    h.push_str(&format!("<div class=\"prog\"><div class=\"bar\"><i style=\"width:{pct}%\"></i></div><span><b>{done}</b> of {total} steps done</span>"));
    if let Some(date) = spec["deadline"]["date"].as_str() {
        let today = chrono::Local::now().date_naive();
        let cls = match chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") {
            Ok(d) if d < today && open => "due late",
            Ok(d) if d == today && open => "due today",
            _ => "due",
        };
        h.push_str(&format!("<span class=\"{cls}\">due {}</span>", esc(date)));
    }
    if let Some(rev) = meta["rev"].as_i64().filter(|r| *r > 1) {
        h.push_str(&format!("<span class=\"rev\">revised by the agent, version {rev}</span>"));
    }
    h.push_str("</div></header>");
    if !open {
        h.push_str(&format!("<div class=\"closed\">This packet is {}. It is kept for the record; nothing here changes it.</div>", esc(status)));
    }
    if let Some(intro) = meta["intro"].as_str().filter(|i| !i.trim().is_empty()) {
        h.push_str(&format!("<section class=\"intro\">{}</section>", blocks(intro)));
    }
    h.push_str(&format!("<section class=\"outcome\"><h2>The result</h2><p>{}</p></section>", inline(spec["outcome"].as_str().unwrap_or(""))));
    h.push_str(&format!("<section><h2>Why it is yours</h2>{}</section>", blocks(spec["why"].as_str().unwrap_or(""))));

    h.push_str("<section><h2>What happened</h2><ul>");
    for f in spec["happened"].as_array().into_iter().flatten() {
        let link =
            f["link"].as_str().filter(|l| safe_url(l)).map(|l| format!(" <a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">source</a>", esc(l))).unwrap_or_default();
        h.push_str(&format!("<li>{}{link}</li>", inline(f["fact"].as_str().unwrap_or(""))));
    }
    h.push_str("</ul></section>");

    h.push_str("<section><h2>What the agent found</h2>");
    let found = spec["found"].as_array().cloned().unwrap_or_default();
    if found.is_empty() {
        h.push_str("<p class=\"mu\">Nothing to open: everything needed is on this page.</p>");
    } else {
        h.push_str("<ul class=\"links\">");
        for f in &found {
            let url = f["url"].as_str().unwrap_or("");
            let label = f["label"].as_str().filter(|l| !l.is_empty()).unwrap_or_else(|| f["domain"].as_str().unwrap_or(url));
            let mut extra = format!("<span class=\"mu\">{}</span>", esc(f["domain"].as_str().unwrap_or("")));
            if f["cited"].as_i64().unwrap_or(1) > 1 {
                extra.push_str(&format!(" <span class=\"mu\">cited {} times</span>", f["cited"]));
            }
            if f["unknown_domain"].as_bool().unwrap_or(false) {
                extra.push_str(" <span class=\"warn\">new domain: check before you sign in</span>");
            }
            if let Some(n) = f["note"].as_str().filter(|n| !n.is_empty()) {
                extra.push_str(&format!(" <span class=\"mu\">{}</span>", esc(n)));
            }
            if safe_url(url) {
                h.push_str(&format!("<li><a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">{}</a> {extra}</li>", esc(url), esc(label)));
            }
        }
        h.push_str("</ul>");
    }
    h.push_str("</section>");

    h.push_str("<section class=\"steps\"><h2>Your steps</h2>");
    for st in spec["steps"].as_array().into_iter().flatten() {
        let sid = st["id"].as_str().unwrap_or("");
        let state = &meta["state"][sid];
        let ticked = state["done"].as_bool().unwrap_or(false);
        let value = state["value"].as_str().unwrap_or("");
        h.push_str(&format!("<article class=\"step{}\" data-step=\"{}\">", if ticked { " ticked" } else { "" }, esc(sid)));
        h.push_str(&format!(
            "<label class=\"head\"><input type=\"checkbox\" data-tick{}{}><span class=\"t\">{}</span></label>",
            if ticked { " checked" } else { "" },
            dis,
            esc(st["title"].as_str().unwrap_or(""))
        ));
        if let Some(d) = st["detail"].as_str() {
            h.push_str(&format!("<div class=\"detail\">{}</div>", blocks(d)));
        }
        if st["input"] == "choice" {
            h.push_str("<div class=\"choices\">");
            for o in st["options"].as_array().into_iter().flatten().filter_map(|o| o.as_str()) {
                h.push_str(&format!("<button type=\"button\" class=\"chip{}\" data-choose=\"{}\"{}>{}</button>", if o == value { " on" } else { "" }, esc(o), dis, esc(o)));
            }
            h.push_str("</div>");
        }
        let note = state["note"].as_str().unwrap_or("");
        h.push_str(&format!("<textarea rows=\"1\" placeholder=\"A note for the agent with this step (optional)\" data-note{dis}>{}</textarea>", esc(note)));
        h.push_str("</article>");
    }
    h.push_str("</section>");

    h.push_str(&format!("<section><h2>What the agent does next</h2>{}</section>", blocks(spec["next"].as_str().unwrap_or(""))));
    h.push_str(&format!(
        "<section><h2>Deadline</h2><p><b>{}</b>: {}</p></section>",
        esc(spec["deadline"]["date"].as_str().unwrap_or("")),
        inline(spec["deadline"]["reason"].as_str().unwrap_or(""))
    ));
    if let Some(w) = spec["work"].as_object() {
        h.push_str("<details><summary>Show my work</summary>");
        for (k, label) in [
            ("recommendation", "Recommendation"),
            ("confidence", "Confidence"),
            ("ruled_out", "Ruled out"),
            ("would_change", "Would change my mind"),
            ("prepared_with", "Prepared with"),
        ] {
            if let Some(v) = w.get(k).and_then(|v| v.as_str()) {
                h.push_str(&format!("<p><b>{label}:</b> {}</p>", inline(v)));
            }
        }
        h.push_str("</details>");
    }

    h.push_str("<section class=\"talk\"><h2>Talk to the agent</h2>");
    let mut any = false;
    for m in doc["messages"].as_array().into_iter().flatten().skip(1) {
        let kind = m["meta"].as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()).and_then(|v| v["kind"].as_str().map(String::from)).unwrap_or_default();
        if matches!(kind.as_str(), "read-receipt" | "ack" | "ack-note") {
            continue;
        }
        any = true;
        let who = m["author"].as_str().unwrap_or("");
        h.push_str(&format!(
            "<div class=\"msg{}\"><div class=\"who\"><b>{}</b> <span class=\"mu\">{}</span></div>{}</div>",
            if who == "john" { " me" } else { "" },
            esc(who),
            esc(&when(m["ts"].as_str().unwrap_or(""))),
            blocks(m["body"].as_str().unwrap_or(""))
        ));
    }
    if !any {
        h.push_str("<p class=\"mu\">Nothing yet. Ticking a step, or writing here, wakes the agent.</p>");
    }
    h.push_str(&format!("<textarea rows=\"3\" id=\"say\" placeholder=\"Write to the agent\" data-say{dis}></textarea><div class=\"row\"><button type=\"button\" data-send{dis}>Send</button></div></section>"));
    h.push_str(&format!(
        "<footer><button type=\"button\" class=\"finish\" data-finish data-left=\"{}\"{dis}>Finish this packet</button><span class=\"mu\">Finishing closes it and tells the agent what is left.</span></footer>",
        total - done
    ));
    Ok(h)
}

pub fn page(db: &Db, id: i64) -> Result<String> {
    let app = app(db, id)?;
    let v = version(db, id)?;
    let title = db.get_thread(id)?["thread"]["subject"].as_str().unwrap_or("Packet").to_string();
    Ok(format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{} · AgentDesk</title><style>{CSS}</style></head>\
<body><div id=\"bar\" hidden></div><main id=\"app\" data-v=\"{v}\" data-id=\"{id}\">{app}</main><div id=\"toast\" hidden></div><script>{JS}</script></body></html>",
        esc(&title)
    ))
}

const CSS: &str = r#"
:root{--bg:#f6f7f9;--card:#fff;--fg:#1d2330;--mu:#6b7385;--rule:#dde1e8;--ye:#a66a00;--gr:#17803d;--pk:#c2255c;--cy:#0b7285;--pu:#6f42c1;--chip:#eef1f6;--code:#f0f2f6}
@media(prefers-color-scheme:dark){:root{--bg:#0f1218;--card:#171b24;--fg:#dde3ee;--mu:#8791a5;--rule:#2a3140;--ye:#e5c07b;--gr:#7ee0a0;--pk:#ff7aa8;--cy:#6fd3e6;--pu:#b794f6;--chip:#212736;--code:#10141c}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:16px/1.55 system-ui,-apple-system,"Segoe UI",sans-serif}
main{max-width:46rem;margin:0 auto;padding:1.5rem 1rem 6rem}
header{margin-bottom:1rem}h1{font-size:1.6rem;line-height:1.25;margin:.4rem 0 .2rem}h2{font-size:.78rem;letter-spacing:.09em;text-transform:uppercase;color:var(--mu);margin:0 0 .5rem}
.pill{display:inline-block;background:var(--pu);color:#fff;font-size:.7rem;font-weight:700;letter-spacing:.1em;padding:.15rem .55rem;border-radius:99px}
.by,.mu{color:var(--mu)}.by{font-size:.9rem}
.prog{display:flex;flex-wrap:wrap;align-items:center;gap:.4rem 1rem;margin-top:.9rem;font-size:.92rem}
.bar{flex:1 1 9rem;min-width:8rem;height:.55rem;background:var(--chip);border-radius:9px;overflow:hidden}.bar i{display:block;height:100%;background:var(--gr);transition:width .3s}
.due{color:var(--mu)}.due.today{color:#d9730d;font-weight:600}.due.late{color:var(--pk);font-weight:700}.rev{color:#d9730d}
section,details{background:var(--card);border:1px solid var(--rule);border-radius:12px;padding:1rem 1.15rem;margin:.9rem 0}
section p,section ul{margin:.4rem 0}ul{padding-left:1.2rem}
.outcome{border-left:4px solid var(--gr)}.outcome p{font-size:1.1rem;font-weight:600;margin:0}
.intro{border-left:4px solid var(--cy)}
.closed{background:var(--chip);border:1px solid var(--rule);border-radius:10px;padding:.7rem 1rem;margin:.7rem 0;color:var(--mu)}
a{color:var(--cy)}.warn{color:var(--pk)}.links{list-style:none;padding:0}.links li{margin:.35rem 0}
code{background:var(--code);padding:.05rem .3rem;border-radius:5px;font-size:.92em}
.code{margin:.6rem 0;border:1px solid var(--rule);border-radius:9px;overflow:hidden}.codebar{display:flex;justify-content:space-between;align-items:center;background:var(--chip);padding:.15rem .6rem;font-size:.75rem;color:var(--mu)}
.codebar button{background:none;border:0;color:var(--cy);cursor:pointer;font:inherit;padding:.2rem}
pre{margin:0;padding:.7rem .8rem;background:var(--code);overflow-x:auto;font-size:.88rem}pre code{background:none;padding:0}
.step{border:1px solid var(--rule);border-radius:10px;padding:.8rem .9rem;margin:.7rem 0;background:var(--bg)}.step.ticked{opacity:.78;border-color:var(--gr)}
.head{display:flex;gap:.7rem;align-items:flex-start;cursor:pointer;font-weight:600}.head input{width:1.25rem;height:1.25rem;margin-top:.2rem;accent-color:var(--gr);flex:none}
.step.ticked .t{text-decoration:line-through;text-decoration-color:var(--mu)}
.detail{margin:.4rem 0 .2rem 1.95rem;font-size:.95rem}
.choices{display:flex;flex-wrap:wrap;gap:.45rem;margin:.5rem 0 .2rem 1.95rem}
.chip{border:1px solid var(--rule);background:var(--chip);color:var(--fg);border-radius:99px;padding:.3rem .9rem;cursor:pointer;font:inherit}.chip.on{background:var(--ye);color:#fff;border-color:var(--ye)}
textarea{width:100%;margin-top:.55rem;background:var(--card);color:var(--fg);border:1px solid var(--rule);border-radius:8px;padding:.5rem .65rem;font:inherit;resize:vertical}
.step textarea{margin-left:1.95rem;width:calc(100% - 1.95rem);font-size:.92rem}
textarea:focus{outline:2px solid var(--cy);outline-offset:1px}
button{font:inherit}button:disabled,textarea:disabled,input:disabled{opacity:.5;cursor:not-allowed}
.row{margin-top:.5rem}[data-send]{background:var(--cy);color:#fff;border:0;border-radius:8px;padding:.5rem 1.2rem;cursor:pointer}
.msg{border-top:1px solid var(--rule);padding:.6rem 0}.msg .who{font-size:.85rem}.msg.me .who b{color:var(--ye)}.msg p{margin:.2rem 0}
footer{display:flex;gap:1rem;align-items:center;flex-wrap:wrap;margin-top:1.4rem}
.finish{background:var(--gr);color:#fff;border:0;border-radius:10px;padding:.7rem 1.5rem;font-weight:700;cursor:pointer}
details summary{cursor:pointer;color:var(--mu)}
#bar{position:sticky;top:0;z-index:5;background:var(--ye);color:#fff;text-align:center;padding:.5rem;cursor:pointer}
#toast{position:fixed;bottom:1rem;left:50%;transform:translateX(-50%);background:var(--pk);color:#fff;border-radius:9px;padding:.6rem 1.1rem;max-width:90vw}
"#;

const JS: &str = r#"
const $=(s,r=document)=>r.querySelector(s),app=()=>$('#app');
let dirty=false;
function toast(t){const e=$('#toast');e.textContent=t;e.hidden=false;clearTimeout(toast.t);toast.t=setTimeout(()=>e.hidden=true,5000)}
async function post(path,body){
  const r=await fetch('/packet/'+app().dataset.id+'/'+path,{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(body||{})});
  const j=await r.json().catch(()=>({error:'no answer from AgentDesk'}));
  if(!r.ok||j.error){toast(j.error||'failed');return false}
  dirty=false;await refresh(true);return true
}
async function refresh(force){
  const r=await fetch(location.pathname,{cache:'no-store'});if(!r.ok)return;
  const d=new DOMParser().parseFromString(await r.text(),'text/html'),n=$('#app',d);
  if(!n)return;app().innerHTML=n.innerHTML;app().dataset.v=n.dataset.v;$('#bar').hidden=true
}
setInterval(async()=>{
  try{
    const r=await fetch('/packet/'+app().dataset.id+'/version',{cache:'no-store'});if(!r.ok)return;
    const v=(await r.json()).v;if(v===app().dataset.v)return;
    const busy=document.activeElement&&document.activeElement.tagName==='TEXTAREA'&&document.activeElement.value!==document.activeElement.defaultValue;
    if(busy||dirty){const b=$('#bar');b.textContent='The agent changed this packet. Click to load the changes (what you are typing is kept until you do).';b.hidden=false;b.onclick=()=>refresh(true)}
    else refresh()
  }catch(e){}
},2500);
document.addEventListener('input',e=>{if(e.target.matches('textarea'))dirty=true});
document.addEventListener('click',async e=>{
  const t=e.target,step=t.closest('[data-step]');
  if(t.matches('[data-copy]')){const code=$('code',t.closest('.code'));try{await navigator.clipboard.writeText(code.textContent);t.textContent='Copied'}catch(x){t.textContent='Select and copy'}setTimeout(()=>t.textContent='Copy',1500);return}
  if(t.matches('[data-choose]')&&step){await post('step',{step:step.dataset.step,done:true,value:t.dataset.choose,note:$('[data-note]',step).value});return}
  if(t.matches('[data-send]')){const s=$('#say');if(!s.value.trim())return;await post('note',{body:s.value});return}
  if(t.matches('[data-finish]')){const left=+t.dataset.left;if(confirm(left?left+' step(s) are not ticked. Finish anyway?':'Finish this packet?'))await post('finish');return}
});
document.addEventListener('change',async e=>{
  const t=e.target;if(!t.matches('[data-tick]'))return;
  const step=t.closest('[data-step]'),chosen=$('.chip.on',step);
  const ok=await post('step',{step:step.dataset.step,done:t.checked,value:chosen?chosen.dataset.choose:undefined,note:$('[data-note]',step).value});
  if(!ok)t.checked=!t.checked
});
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_from_the_board_cannot_become_markup() {
        assert_eq!(inline("<script>alert(1)</script>"), "&lt;script&gt;alert(1)&lt;/script&gt;");
        assert_eq!(inline("[x](javascript:alert(1))"), "[x](javascript:alert(1))");
        assert!(inline("[x](https://a.b/c?d=1&e=2)").contains("href=\"https://a.b/c?d=1&amp;e=2\""));
        assert!(inline("[x](\"onmouseover=\"y)").starts_with("[x]("));
        assert_eq!(inline("**a <b>**"), "<strong>a &lt;b&gt;</strong>");
        assert_eq!(inline("a `<i>` b"), "a <code>&lt;i&gt;</code> b");
    }

    #[test]
    fn blocks_make_paragraphs_lists_and_copyable_code() {
        let h = blocks("One\ntwo\n\n- a\n- b\n\n  ```powershell\n  gh pr view 3\n    nested\n  ```\nafter");
        assert!(h.contains("<p>One<br>two</p>"), "{h}");
        assert!(h.contains("<ul><li>a</li><li>b</li></ul>"), "{h}");
        assert!(h.contains("data-copy"), "{h}");
        assert!(h.contains("<code>gh pr view 3\n  nested</code>"), "an indented fence loses its indent: {h}");
        assert!(h.ends_with("<p>after</p>"), "{h}");
        assert!(blocks("```sh\nunclosed <b>").contains("&lt;b&gt;"));
    }
}
