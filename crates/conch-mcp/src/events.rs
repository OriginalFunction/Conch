//! Agent-facing view of committed scenes: which takes mention me, when the floor
//! is mine, who holds it now. Pure functions over the JSON the daemon serves.

use conch_core::{
    encoding::scene_hash,
    types::{AgentId, Hash32, Mouth},
};
use serde_json::{json, Value};

/// How a take addresses one agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Address {
    /// This take asks this agent, by name, to answer.
    Mention,
    /// A person addressed the whole room. Every agent sees it, but the room
    /// wants one answer, not one answer each.
    Broadcast,
    /// This take names this agent without asking it to answer.
    Cc,
    /// This take does not name this agent.
    None,
}

/// Every `@handle` in the order it appears, lowercased and without the `@`. A
/// handle starts the text or follows whitespace or punctuation, so an email
/// address is not a handle.
pub fn mentioned_handles(text: &str) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut handles = Vec::new();
    for (at, _) in lower.match_indices('@') {
        let preceded_ok =
            at == 0 || bytes[at - 1].is_ascii_whitespace() || bytes[at - 1].is_ascii_punctuation();
        if !preceded_ok {
            continue;
        }
        let handle: String = lower[at + 1..]
            .chars()
            .take_while(|c| is_id_char(*c))
            .collect();
        if !handle.is_empty() {
            handles.push(handle);
        }
    }
    handles
}

/// Only the first handle in a take is asked to answer. Everyone else it names
/// is copied, so one question draws one answer instead of four. A broadcast
/// stays a summons only when a person wrote it.
pub fn address(text: &str, agent: &AgentId, author_is_human: bool) -> Address {
    let full = agent.as_str().to_ascii_lowercase();
    let short = full.split_once(':').map(|(_, short)| short.to_owned());
    let is_me =
        |handle: &String| *handle == full || short.as_deref().is_some_and(|short| handle == short);
    let is_broadcast = |handle: &String| handle == "all" || handle == "everyone";
    let handles = mentioned_handles(text);
    if author_is_human && handles.iter().any(is_broadcast) {
        return Address::Broadcast;
    }
    // A broadcast reaches Mention only through the check above, so `@all` from
    // an agent is a copy however early it appears.
    match handles.first() {
        Some(first) if is_me(first) => Address::Mention,
        _ if handles
            .iter()
            .any(|handle| is_me(handle) || is_broadcast(handle)) =>
        {
            Address::Cc
        }
        _ => Address::None,
    }
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
                    let author_is_human = author
                        .as_ref()
                        .and_then(|author| author["agent"].as_str())
                        .is_some_and(|agent| agent.starts_with("human:"));
                    let addressed = if mine {
                        Address::None
                    } else {
                        address(&text, &you.agent, author_is_human)
                    };
                    let mut event = base(match addressed {
                        Address::Mention | Address::Broadcast => "mention",
                        Address::Cc => "cc",
                        Address::None => "speech",
                    });
                    if addressed == Address::Broadcast {
                        // Everyone was asked. Whoever reaches the floor first
                        // answers it; the rest add only what that answer missed.
                        event["broadcast"] = json!(true);
                    }
                    event["author"] = author.clone().unwrap_or(Value::Null);
                    event["text"] = json!(text);
                    if matches!(addressed, Address::Mention | Address::Broadcast) {
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
            assert_ne!(address(text, &me, true), Address::None, "{text:?}");
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
            assert_eq!(address(text, &me, true), Address::None, "{text:?}");
        }
        // An id without a colon has only its full form.
        assert_eq!(
            address("@local hi", &agent("local"), false),
            Address::Mention
        );
        assert_eq!(address("@loc hi", &agent("local"), false), Address::None);
    }

    #[test]
    fn only_the_first_handle_is_summoned() {
        let codex = agent("agent:codex");
        let claude = agent("agent:claude");
        let text = "@codex please fold this in, @claude @grok review after";
        assert_eq!(address(text, &codex, false), Address::Mention);
        assert_eq!(address(text, &claude, false), Address::Cc);
        assert_eq!(address("nobody named here", &codex, false), Address::None);
    }

    #[test]
    fn a_broadcast_only_summons_when_a_person_writes_it() {
        let codex = agent("agent:codex");
        assert_eq!(
            address("@all status please", &codex, true),
            Address::Broadcast
        );
        assert_eq!(address("@all status please", &codex, false), Address::Cc);
        assert_eq!(
            address("@claude and @all should see this", &codex, true),
            Address::Broadcast
        );
        assert_eq!(
            address("@claude and @all should see this", &codex, false),
            Address::Cc
        );
    }

    #[test]
    fn a_broadcast_take_is_flagged_so_one_agent_can_own_the_answer() {
        let you = Mouth {
            agent: AgentId::new("agent:codex").unwrap(),
            node: node(1),
        };
        let records = vec![record(
            2,
            json!({ "type": "speech", "closes_grant": "aa", "text": "@all what is the bug?" }),
            Some(("human:operator", 2)),
        )];
        let events = flatten(&records, &you);
        assert_eq!(events[0]["kind"], json!("mention"));
        assert_eq!(events[0]["broadcast"], json!(true));

        // A take that names one agent is not a broadcast.
        let records = vec![record(
            4,
            json!({ "type": "speech", "closes_grant": "bb", "text": "@codex what breaks?" }),
            Some(("human:operator", 2)),
        )];
        let events = flatten(&records, &you);
        assert_eq!(events[0]["kind"], json!("mention"));
        assert!(events[0]["broadcast"].is_null(), "{:?}", events[0]);
    }

    #[test]
    fn handles_are_returned_in_the_order_they_appear() {
        assert_eq!(
            mentioned_handles("hi @Codex and @claude, cc @all"),
            vec!["codex".to_owned(), "claude".to_owned(), "all".to_owned()]
        );
        assert_eq!(mentioned_handles("no handles here"), Vec::<String>::new());
        assert_eq!(mentioned_handles("email@example.com"), Vec::<String>::new());
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
