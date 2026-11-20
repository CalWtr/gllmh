//! Standalone implementation of the JSON-lines contract in docs/composer-contract.md.

use std::error::Error;
use std::io::{self, BufRead, Write};

use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

struct Composer {
    store: Connection,
    llm_profile: String,
}

impl Composer {
    fn init(input: &Value) -> Result<Self> {
        let path = input["store"].as_str().ok_or("missing store path")?;
        let llm_profile = input["llm_profiles"][0]["name"].as_str().ok_or("missing LLM profile name")?;
        let store = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Self { store, llm_profile: llm_profile.into() })
    }

    fn compose(&self, input: &Value) -> Result<Value> {
        let turn = input["turn"].as_str().ok_or("missing turn id")?;
        let received: String = self.store.query_row(
            "SELECT input FROM v1_action WHERE turn = ?1 AND interaction = ?1 AND kind = 'receive'",
            [turn],
            |row| row.get(0),
        )?;
        let trigger: Value = serde_json::from_str(&received)?;
        let content = if let Some(blocks) = trigger["context"].as_array() {
            blocks.iter().map(|block| json!({"type": "text", "text": block.to_string()})).collect::<Vec<_>>()
        } else {
            let body = &trigger["body"];
            let text = body.as_str().map_or_else(|| body.to_string(), str::to_owned);
            vec![json!({"type": "text", "text": text})]
        };
        Ok(json!({
            "llm_profile": self.llm_profile,
            "messages": [
                {"role": "system", "content": [{"type": "text", "text": "Echo the user's input."}]},
                {"role": "user", "content": content}
            ],
            "tools": [],
            "marks": [{"target": turn, "key": "seen_by", "value": "echo_composer"}]
        }))
    }
}

fn main() -> Result<()> {
    let mut composer = None;
    let mut out = io::stdout().lock();
    for (index, line) in io::stdin().lock().lines().enumerate() {
        let request: Value = serde_json::from_str(&line?)?;
        let re = request["id"].as_str().ok_or("missing request id")?;
        let id = format!("echo-{index}");
        let input = &request["input"];
        let reply = match (request["op"].as_str(), &composer) {
            (Some("init"), None) => {
                composer = Some(Composer::init(input)?);
                json!({"id": id, "re": re, "output": {}})
            }
            (Some("compose"), Some(composer)) => match composer.compose(input) {
                Ok(output) => json!({"id": id, "re": re, "output": output}),
                Err(error) => json!({
                    "id": id, "re": re,
                    "error": {"code": "invalid_reply", "message": error.to_string(), "retry": false}
                }),
            },
            _ => return Err("expected one init followed by compose requests".into()),
        };
        writeln!(out, "{reply}")?;
        out.flush()?;
    }
    Ok(())
}
