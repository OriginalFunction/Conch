//! Agent-facing view of committed scenes: which takes mention me, when the floor
//! is mine, who holds it now. Pure functions over the JSON the daemon serves.

use conch_core::{
    encoding::scene_hash,
    types::{AgentId, Hash32, Mouth},
};
use serde_json::{json, Value};

/// Does `text` address `agent`? `@` plus the full id, its short name after the
/// first colon, or `all`/`everyone`; case-insensitive; the `@` starts the text or
/// follows whitespace or punctuation; the name is not followed by an id character.
pub fn mentions(text: &str, agent: &AgentId) -> bool {
    let full = agent.as_str().to_ascii_lowercase();
    let short = full.split_once(':').map(|(_, short)| short.to_owned());
    let lower = text.to_ascii_lowercase();
    let mut names: Vec<&str> = vec![full.as_str(), "all", "everyone"];
    if let Some(short) = short.as_deref() {
        names.push(short);
    }
    let bytes = lower.as_bytes();
    for (at, _) in lower.match_indices('@') {
        let preceded_ok =
            at == 0 || bytes[at - 1].is_ascii_whitespace() || bytes[at - 1].is_ascii_punctuation();
        if !preceded_ok {
            continue;
        }
        let rest = &lower[at + 1..];
        for name in &names {
            if let Some(after) = rest.strip_prefix(name) {
                let boundary = after.chars().next().is_none_or(|c| !is_id_char(c));
                if boundary {
                    return true;
                }
            }
        }
    }
    false
}

fn is_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-' | '.')
}

/// The last committed height in a page, whether or not it produced an event.
pub fn last_height(records: &[Value]) -> Option<u64> {
    records
        .last()
        .and_then(|record| record["scene"]["n"].as_u64())
}

/// One event per scene that concerns an agent; genesis produces none.
pub fn flatten(records: &[Value], you: &Mouth) -> Vec<Value> {
    let you_json = serde_json::to_value(you).expect("mouth is serializable");
    records
        .iter()
        .filter_map(|record| {
            let scene = &record["scene"];
            let body = &scene["body"];
            let n = scene["n"].clone();
            let ts = scene["ts"].clone();
            let author = record.get("author").cloned();
            let base = |kind: &str| json!({ "kind": kind, "n": n, "ts": ts });
            let mut event = match body["type"].as_str()? {
                "genesis" => return None,
                "grant" => {
                    if body["to"] == you_json {
                        let hash = Hash32::from_bytes(scene_hash(scene));
                        let mut event = base("granted");
                        event["grant_hash"] = json!(hash);
                        event
                    } else {
                        let mut event = base("floor");
                        event["holder"] = body["to"].clone();
                        event
                    }
                }
                "speech" => {
                    let text = body["text"].as_str().unwrap_or("").to_owned();
                    let mine = author.as_ref().is_some_and(|author| *author == you_json);
                    let mentioned = !mine && mentions(&text, &you.agent);
                    let mut event = base(if mentioned { "mention" } else { "speech" });
                    event["author"] = author.clone().unwrap_or(Value::Null);
                    event["text"] = json!(text);
                    if mentioned {
                        event["grant_hash"] = body["closes_grant"].clone();
                    } else {
                        event["empty"] = json!(
                            text.is_empty()
                                && body["blobs"].as_array().is_none_or(|b| b.is_empty())
                        );
                    }
                    event
                }
                "view-change" if body.get("closes_grant").is_none() => {
                    let mut event = base("roster");
                    event["added"] = body["add"].clone();
                    event["removed"] = body["remove"].clone();
                    event
                }
                "membership" if body.get("closes_grant").is_none() => {
                    let mut event = base("config");
                    event["mode"] = body["floor"]["mode"].clone();
                    event["timeout_secs"] = body["floor"]["timeout_secs"].clone();
                    event
                }
                // A breakout, or a membership/view-change issued as a take: the
                // scene closed a grant, so the floor is vacant again.
                "breakout" | "membership" | "view-change" => {
                    let mut event = base("floor");
                    event["holder"] = Value::Null;
                    event["author"] = author.clone().unwrap_or(Value::Null);
                    event
                }
                // A body type this build does not know: report nothing rather
                // than inventing a floor change from it.
                _ => return None,
            };
            if event["author"].is_null() {
                event.as_object_mut().map(|object| object.remove("author"));
            }
            Some(event)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use conch_core::types::NodeId;

    fn agent(id: &str) -> AgentId {
        AgentId::new(id).unwrap()
    }

    #[test]
    fn mentions_match_full_id_short_name_and_everyone_with_word_bounds() {
        let me = agent("agent:claude");
        for text in [
            "@agent:claude ping",
            "hey @claude, ping",
            "@Claude?",
            "(@claude)",
            "all hands @all",
            "@everyone look",
            "line\n@claude",
        ] {
            assert!(mentions(text, &me), "{text:?}");
        }
        for text in [
            "email@claude.ai",
            "@claudette",
            "@claude_2",
            "@claude.dev",
            "claude without at",
            "@agent:codex",
            "",
        ] {
            assert!(!mentions(text, &me), "{text:?}");
        }
        // An id without a colon has only its full form.
        assert!(mentions("@local hi", &agent("local")));
        assert!(!mentions("@loc hi", &agent("local")));
    }

    fn node(byte: u8) -> NodeId {
        NodeId::from_bytes([byte; 32])
    }

    fn record(n: u64, body: Value, author: Option<(&str, u8)>) -> Value {
        let mut record = json!({
            "scene": { "n": n, "ts": 1_700_000_000 + n, "body": body },
            "commit_proof": {}
        });
        if let Some((agent, node)) = author {
            record["author"] = json!({ "agent": agent, "node": node_json(node) });
        }
        record
    }

    fn node_json(byte: u8) -> Value {
        serde_json::to_value(node(byte)).unwrap()
    }

    fn me() -> Mouth {
        Mouth {
            agent: agent("agent:claude"),
            node: node(1),
        }
    }

    #[test]
    fn every_scene_kind_flattens_to_at_most_one_event() {
        let records = vec![
            record(0, json!({ "type": "genesis", "name": "r" }), None),
            record(
                1,
                json!({ "type": "grant", "to": { "agent": "agent:codex", "node": node_json(2) }, "reason": "queue", "intent_id": "00" }),
                None,
            ),
            record(
                2,
                json!({ "type": "speech", "closes_grant": "aa", "text": "hi @claude" }),
                Some(("agent:codex", 2)),
            ),
            record(
                3,
                json!({ "type": "grant", "to": { "agent": "agent:claude", "node": node_json(1) }, "reason": "queue", "intent_id": "01" }),
                None,
            ),
            record(
                4,
                json!({ "type": "speech", "closes_grant": "bb", "text": "@claude talking to myself" }),
                Some(("agent:claude", 1)),
            ),
            record(
                5,
                json!({ "type": "grant", "to": { "agent": "agent:codex", "node": node_json(2) }, "reason": "queue", "intent_id": "02" }),
                None,
            ),
            record(
                6,
                json!({ "type": "speech", "closes_grant": "cc", "text": "" }),
                Some(("agent:codex", 2)),
            ),
            record(
                7,
                json!({ "type": "view-change", "add": [node_json(3)], "remove": [], "next_roster": [] }),
                None,
            ),
            record(
                8,
                json!({ "type": "membership", "stake": {}, "floor": { "mode": "stick", "timeout_secs": 300 } }),
                None,
            ),
            record(
                9,
                json!({ "type": "grant", "to": { "agent": "agent:codex", "node": node_json(2) }, "reason": "queue", "intent_id": "03" }),
                None,
            ),
            record(
                10,
                json!({ "type": "membership", "closes_grant": "dd", "stake": {}, "floor": { "mode": "stick", "timeout_secs": 60 } }),
                Some(("agent:codex", 2)),
            ),
            record(
                11,
                json!({ "type": "grant", "to": { "agent": "agent:codex", "node": node_json(2) }, "reason": "queue", "intent_id": "04" }),
                None,
            ),
            record(
                12,
                json!({ "type": "breakout", "closes_grant": "ee", "ticket": {}, "auto_join": [] }),
                Some(("agent:codex", 2)),
            ),
            record(
                13,
                json!({ "type": "grant", "to": { "agent": "agent:codex", "node": node_json(2) }, "reason": "queue", "intent_id": "05" }),
                None,
            ),
            record(
                14,
                json!({ "type": "view-change", "add": [], "remove": [], "next_roster": [], "closes_grant": "ff" }),
                Some(("agent:codex", 2)),
            ),
            record(
                15,
                json!({ "type": "grant", "to": { "agent": "agent:codex", "node": node_json(2) }, "reason": "queue", "intent_id": "06" }),
                None,
            ),
            record(
                16,
                json!({ "type": "speech", "closes_grant": "gg", "text": "", "blobs": [{ "name": "a.txt", "sha256": "00", "bytes": 1 }] }),
                Some(("agent:codex", 2)),
            ),
        ];
        let events = flatten(&records, &me());
        let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            [
                "floor", "mention", "granted", "speech", "floor", "speech", "roster", "config",
                "floor", "floor", "floor", "floor", "floor", "floor", "floor", "speech"
            ]
        );
        assert_eq!(events[0]["holder"]["agent"], "agent:codex");
        assert_eq!(events[1]["author"]["agent"], "agent:codex");
        assert_eq!(events[1]["text"], "hi @claude");
        assert_eq!(events[1]["grant_hash"], "aa");
        // `granted` carries the grant scene's own hash, computed from the scene JSON.
        assert_eq!(events[2]["grant_hash"].as_str().unwrap().len(), 64);
        assert_eq!(
            events[3]["author"]["agent"], "agent:claude",
            "own take is speech, never mention"
        );
        assert_eq!(events[5]["empty"], true);
        assert_eq!(events[6]["added"], json!([node_json(3)]));
        assert_eq!(events[7]["timeout_secs"], 300);
        // A take closed by a non-speech scene vacates the floor and names its author.
        assert_eq!(events[9]["holder"], Value::Null);
        assert_eq!(events[9]["author"]["agent"], "agent:codex");
        // Breakout and view-change with closes_grant also vacate the floor.
        assert_eq!(events[11]["holder"], Value::Null);
        assert_eq!(events[11]["author"]["agent"], "agent:codex");
        assert_eq!(events[13]["holder"], Value::Null);
        assert_eq!(events[13]["author"]["agent"], "agent:codex");
        // Speech with empty text and blobs is not empty.
        assert_eq!(events[15]["empty"], false);
        for (event, n) in events
            .iter()
            .zip([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16])
        {
            assert_eq!(event["n"], n);
            assert_eq!(event["ts"], 1_700_000_000 + n);
        }
        assert_eq!(last_height(&records), Some(16));
        assert_eq!(last_height(&[]), None);
    }

    #[test]
    fn an_unrecognised_body_type_produces_no_event() {
        let records = vec![
            record(1, json!({ "type": "something-newer", "field": 1 }), None),
            record(
                2,
                json!({ "type": "breakout", "closes_grant": "ee", "ticket": {}, "auto_join": [] }),
                Some(("agent:codex", 2)),
            ),
        ];
        let events = flatten(&records, &me());
        let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            ["floor"],
            "a body type this build does not know is not a floor change"
        );
        assert_eq!(events[0]["n"], 2);
        assert_eq!(last_height(&records), Some(2));
    }
}
