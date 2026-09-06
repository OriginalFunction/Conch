//! Text for people. Every wire reply the CLI prints has one formatter here; `--json`
//! bypasses the module entirely.

use serde_json::Value;

pub struct Context {
    /// The room the command was addressed to, when known.
    pub room: Option<String>,
    /// The `current-room` file's value, for the `rooms` marker.
    pub current_room: Option<String>,
    pub oneline: bool,
    pub width: usize,
}

/// The first eight characters of an id, then an ellipsis.
pub fn short(id: &str) -> String {
    match id.char_indices().nth(8) {
        Some((cut, _)) => format!("{}…", &id[..cut]),
        None => id.to_owned(),
    }
}

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn short_of(value: &Value, key: &str) -> String {
    short(str_of(value, key))
}

fn role_word(role: &str) -> &'static str {
    if role == "observe" {
        "observer"
    } else {
        "staker"
    }
}

fn agents(list: Option<&Vec<Value>>, empty: &str) -> String {
    let names: Vec<&str> = list
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.as_str()
                        .or_else(|| item.get("agent").and_then(Value::as_str))
                })
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        empty.to_owned()
    } else {
        names.join(", ")
    }
}

/// Text for a successful reply, or `None` when the command prints JSON only.
pub fn render(command: &str, data: &Value, ctx: &Context) -> Option<String> {
    let text = match command {
        "create" => format!(
            "created \"{}\" ({})\nticket: {}\nmagnet: {}",
            str_of(data, "name"),
            short_of(data, "id"),
            str_of(data, "ticket_path"),
            str_of(data, "magnet")
        ),
        "join" => {
            let id = short_of(data, "id");
            let role = role_word(str_of(data, "role"));
            match (
                data.get("name").and_then(Value::as_str),
                data.get("head_n").and_then(Value::as_u64),
            ) {
                (Some(name), Some(head)) => {
                    format!("joined \"{name}\" ({id}) as {role}, head {head}")
                }
                _ => format!("joined {id} as {role}"),
            }
        }
        "status" if data.get("rooms").is_some() => rooms_table(data, ctx),
        "rooms" => rooms_table(data, ctx),
        "status" => {
            let floor = match data.get("holder").filter(|holder| !holder.is_null()) {
                Some(holder) => format!(
                    "floor: {} since #{}",
                    str_of(holder, "agent"),
                    holder.get("since_n").and_then(Value::as_u64).unwrap_or(0)
                ),
                None => "floor: vacant".to_owned(),
            };
            format!(
                "{} ({})\nhead {}\nmode {}, timeout {} s\n{floor}\nqueue: {}\nparticipants: {}",
                str_of(data, "name"),
                short_of(data, "room"),
                data.get("head_n").and_then(Value::as_u64).unwrap_or(0),
                str_of(data, "mode"),
                data.get("timeout_secs")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                agents(data.get("queue").and_then(Value::as_array), "empty"),
                agents(data.get("participants").and_then(Value::as_array), "none"),
            )
        }
        "history" | "tail" => data
            .get("scenes")
            .and_then(Value::as_array)
            .map(|scenes| {
                scenes
                    .iter()
                    .map(|record| scene_line(record, ctx.oneline, ctx.width))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        "wait-for-floor" => format!(
            "floor is yours (grant #{})",
            data.get("n").and_then(Value::as_u64).unwrap_or(0)
        ),
        "speak" => format!(
            "appended (rev {})",
            data.get("rev").and_then(Value::as_u64).unwrap_or(0)
        ),
        "yield" => format!(
            "take frozen (rev {}); closes grant {}",
            data.get("rev").and_then(Value::as_u64).unwrap_or(0),
            short_of(data, "grant_hash")
        ),
        "raise-hand" => "queued".to_owned(),
        "grant" => format!(
            "granted to {} (#{})",
            str_of(&data["body"]["to"], "agent"),
            data.get("n").and_then(Value::as_u64).unwrap_or(0)
        ),
        "yank" => format!("yanked; closes grant {}", short_of(data, "closes_grant")),
        "config" => format!(
            "config committed (#{})",
            data.get("n").and_then(Value::as_u64).unwrap_or(0)
        ),
        "breakout" => format!(
            "breakout \"{}\" ({}) created",
            str_of(&data["ticket"], "name"),
            short_of(data, "id")
        ),
        "blob" => format!(
            "attached {} ({} bytes)",
            str_of(data, "name"),
            data.get("bytes").and_then(Value::as_u64).unwrap_or(0)
        ),
        "leave" => match ctx.room.as_deref().map(short) {
            Some(id) => format!("left {}", id),
            None => "left".to_owned(),
        },
        "say" => format!(
            "said #{} as {}",
            data.get("n").and_then(Value::as_u64).unwrap_or(0),
            str_of(&data["author"], "agent")
        ),
        "use" => format!(
            "using \"{}\" ({})",
            str_of(data, "name"),
            short_of(data, "id")
        ),
        _ => return None,
    };
    Some(text)
}

fn rooms_table(data: &Value, ctx: &Context) -> String {
    let rooms = data
        .get("rooms")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if rooms.is_empty() {
        return "no rooms; conch create --name …".to_owned();
    }
    rooms
        .iter()
        .map(|room| {
            let id = str_of(room, "id");
            let marker = if ctx.current_room.as_deref() == Some(id) {
                '*'
            } else {
                ' '
            };
            let holder = room
                .get("holder")
                .filter(|h| !h.is_null())
                .map(|h| str_of(h, "agent").to_owned())
                .unwrap_or_else(|| "vacant".into());
            format!(
                "{marker} {}  {:<17}  head {:<3} {holder}",
                short(id),
                str_of(room, "name"),
                room.get("head_n").and_then(Value::as_u64).unwrap_or(0)
            )
            .trim_end()
            .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One history line per scene: height, author or kind, content.
/// When a label is longer than 14 characters, continuation lines indent by the head's actual width.
/// The head is never truncated; the line fits `width` whenever `width` leaves room for one content character.
pub fn scene_line(record: &Value, oneline: bool, width: usize) -> String {
    let scene = &record["scene"];
    let body = &scene["body"];
    let n = scene.get("n").and_then(Value::as_u64).unwrap_or(0);
    let author = record
        .get("author")
        .and_then(|a| a.get("agent"))
        .and_then(Value::as_str);
    let (label, content) = match str_of(body, "type") {
        "genesis" => (
            "genesis".to_owned(),
            format!("\"{}\"", str_of(body, "name")),
        ),
        "grant" => (
            "grant".to_owned(),
            format!("→ {}", str_of(&body["to"], "agent")),
        ),
        "speech" => {
            let text = str_of(body, "text");
            (
                author.unwrap_or("take").to_owned(),
                if text.trim().is_empty() {
                    "(empty take)".to_owned()
                } else {
                    text.to_owned()
                },
            )
        }
        "breakout" => (
            author.unwrap_or("take").to_owned(),
            format!("breakout \"{}\"", str_of(&body["ticket"], "name")),
        ),
        "membership" => (
            "config".to_owned(),
            format!(
                "mode {}, timeout {} s",
                str_of(&body["floor"], "mode"),
                body["floor"]
                    .get("timeout_secs")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            ),
        ),
        "view-change" => {
            let list = |key: &str, sign: char| {
                body.get(key)
                    .and_then(Value::as_array)
                    .map(|ids| {
                        ids.iter()
                            .filter_map(Value::as_str)
                            .map(|id| format!("{sign} {}", short(id)))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            };
            let mut parts = list("add", '+');
            parts.extend(list("remove", '-'));
            ("roster".to_owned(), parts.join(" "))
        }
        other => (other.to_owned(), String::new()),
    };
    let head = format!("{:<5} {:<14} ", format!("#{n}"), label);
    let head_width = head.chars().count();
    let mut lines = content.lines();
    let first = lines.next().unwrap_or("");
    if oneline {
        let room_for_text = width.saturating_sub(head_width).max(1);
        let first: String = if first.chars().count() > room_for_text {
            let mut cut: String = first.chars().take(room_for_text - 1).collect();
            cut.push('…');
            cut
        } else {
            first.to_owned()
        };
        return format!("{head}{first}");
    }
    let mut out = format!("{head}{first}");
    for line in lines {
        out.push('\n');
        out.push_str(&" ".repeat(head_width));
        out.push_str(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> Context {
        Context {
            room: Some("0101010101010101010101010101010101010101010101010101010101010101".into()),
            current_room: None,
            oneline: false,
            width: 80,
        }
    }

    #[test]
    fn ids_are_shortened_to_eight_characters() {
        assert_eq!(short("0101010101010101abcd"), "01010101…");
        assert_eq!(short("short"), "short");
    }

    #[test]
    fn create_join_and_use_lines() {
        let created = json!({ "ticket_path": "./design-room.conch", "magnet": "conch:1:abc?g=1", "id": "a1b2c3d4e5f6", "name": "Design room" });
        assert_eq!(
            render("create", &created, &ctx()).unwrap(),
            "created \"Design room\" (a1b2c3d4…)\nticket: ./design-room.conch\nmagnet: conch:1:abc?g=1"
        );
        let joined =
            json!({ "id": "a1b2c3d4e5f6", "role": "stake", "name": "Design room", "head_n": 12 });
        assert_eq!(
            render("join", &joined, &ctx()).unwrap(),
            "joined \"Design room\" (a1b2c3d4…) as staker, head 12"
        );
        let observed = json!({ "id": "a1b2c3d4e5f6", "role": "observe" });
        assert_eq!(
            render("join", &observed, &ctx()).unwrap(),
            "joined a1b2c3d4… as observer"
        );
        assert_eq!(
            render(
                "use",
                &json!({ "id": "a1b2c3d4e5f6", "name": "Design room" }),
                &ctx()
            )
            .unwrap(),
            "using \"Design room\" (a1b2c3d4…)"
        );
    }

    #[test]
    fn status_for_a_room() {
        let status = json!({
            "room": "a1b2c3d4e5f6", "name": "Design room", "head_n": 12, "mode": "stick", "timeout_secs": 300,
            "holder": { "agent": "agent:claude", "node": "n", "grant_hash": "h", "since_n": 11, "granted_ts": 1 },
            "queue": [{ "agent": "agent:codex" }, { "agent": "human:ray" }],
            "participants": ["agent:claude", "agent:codex", "human:ray"]
        });
        assert_eq!(
            render("status", &status, &ctx()).unwrap(),
            "Design room (a1b2c3d4…)\nhead 12\nmode stick, timeout 300 s\nfloor: agent:claude since #11\nqueue: agent:codex, human:ray\nparticipants: agent:claude, agent:codex, human:ray"
        );
        let vacant = json!({ "room": "a1b2c3d4e5f6", "name": "Empty", "head_n": 0, "mode": "moderator", "timeout_secs": 60, "holder": null, "queue": [], "participants": [] });
        assert_eq!(
            render("status", &vacant, &ctx()).unwrap(),
            "Empty (a1b2c3d4…)\nhead 0\nmode moderator, timeout 60 s\nfloor: vacant\nqueue: empty\nparticipants: none"
        );
    }

    #[test]
    fn rooms_table_marks_the_current_room() {
        let rooms = json!({ "node": "n", "rooms": [
            { "id": "a1b2c3d4e5f6", "name": "Design room", "head_n": 12, "holder": { "agent": "agent:claude", "node": "n" }, "last_activity": 9, "role": "stake" },
            { "id": "b2c3d4e5f6a1", "name": "Side room", "head_n": 3, "holder": null, "last_activity": 2, "role": "observe" }
        ]});
        let mut ctx = ctx();
        ctx.current_room = Some("a1b2c3d4e5f6".into());
        assert_eq!(
            render("rooms", &rooms, &ctx).unwrap(),
            "* a1b2c3d4…  Design room        head 12  agent:claude\n  b2c3d4e5…  Side room          head 3   vacant"
        );
        assert_eq!(
            render("status", &rooms, &ctx).unwrap(),
            render("rooms", &rooms, &ctx).unwrap()
        );
        assert_eq!(
            render("rooms", &json!({ "node": "n", "rooms": [] }), &ctx).unwrap(),
            "no rooms; conch create --name …"
        );
    }

    fn record(n: u64, body: Value, author: Option<&str>) -> Value {
        let mut record = json!({ "scene": { "n": n, "ts": 1, "body": body }, "commit_proof": {} });
        if let Some(agent) = author {
            record["author"] = json!({ "agent": agent, "node": "n" });
        }
        record
    }

    #[test]
    fn history_lines_cover_every_scene_kind() {
        let page = json!({ "scenes": [
            record(0, json!({ "type": "genesis", "name": "Design room" }), None),
            record(11, json!({ "type": "grant", "to": { "agent": "agent:claude", "node": "n" } }), None),
            record(12, json!({ "type": "speech", "closes_grant": "h", "text": "the first line\na second line" }), Some("agent:claude")),
            record(13, json!({ "type": "membership", "stake": {}, "floor": { "mode": "stick", "timeout_secs": 300 } }), None),
            record(14, json!({ "type": "view-change", "add": ["9f8e7d6c5b4a"], "remove": [], "next_roster": [] }), None),
            record(15, json!({ "type": "speech", "closes_grant": "h", "text": "" }), Some("agent:codex")),
            record(16, json!({ "type": "breakout", "closes_grant": "h", "ticket": { "name": "Side room" }, "auto_join": [] }), Some("agent:codex")),
            record(17, json!({ "type": "speech", "closes_grant": "h", "text": "no author known" }), None),
        ], "syncing": false, "complete": true });
        let expected = "\
#0    genesis        \"Design room\"
#11   grant          → agent:claude
#12   agent:claude   the first line
                     a second line
#13   config         mode stick, timeout 300 s
#14   roster         + 9f8e7d6c…
#15   agent:codex    (empty take)
#16   agent:codex    breakout \"Side room\"
#17   take           no author known";
        assert_eq!(render("history", &page, &ctx()).unwrap(), expected);
    }

    #[test]
    fn oneline_keeps_the_first_line_within_the_width() {
        let long = record(
            12,
            json!({ "type": "speech", "closes_grant": "h", "text": format!("{}\nsecond", "x".repeat(100)) }),
            Some("agent:claude"),
        );
        let line = scene_line(&long, true, 60);
        assert_eq!(line.lines().count(), 1);
        assert!(line.chars().count() <= 60, "{line}");
        assert!(line.ends_with('…'));
        assert_eq!(
            render("history", &json!({ "scenes": [] }), &ctx()).unwrap(),
            ""
        );
    }

    #[test]
    fn one_liners_for_the_remaining_commands() {
        let c = ctx();
        assert_eq!(
            render(
                "wait-for-floor",
                &json!({ "n": 14, "body": { "type": "grant" } }),
                &c
            )
            .unwrap(),
            "floor is yours (grant #14)"
        );
        assert_eq!(
            render(
                "speak",
                &json!({ "ok": true, "grant_hash": "aa", "rev": 2 }),
                &c
            )
            .unwrap(),
            "appended (rev 2)"
        );
        assert_eq!(
            render(
                "yield",
                &json!({ "ok": true, "grant_hash": "a1b2c3d4e5f6", "rev": 2 }),
                &c
            )
            .unwrap(),
            "take frozen (rev 2); closes grant a1b2c3d4…"
        );
        assert_eq!(
            render("raise-hand", &json!({ "intent_id": "x" }), &c).unwrap(),
            "queued"
        );
        assert_eq!(render("grant", &json!({ "n": 20, "body": { "type": "grant", "to": { "agent": "agent:codex", "node": "n" } } }), &c).unwrap(), "granted to agent:codex (#20)");
        assert_eq!(
            render(
                "yank",
                &json!({ "ok": true, "closes_grant": "a1b2c3d4e5f6" }),
                &c
            )
            .unwrap(),
            "yanked; closes grant a1b2c3d4…"
        );
        assert_eq!(
            render(
                "config",
                &json!({ "n": 21, "body": { "type": "membership" } }),
                &c
            )
            .unwrap(),
            "config committed (#21)"
        );
        assert_eq!(render("breakout", &json!({ "id": "b2c3d4e5f6a1", "magnet": "m", "ticket": { "name": "Side room" }, "scene": {} }), &c).unwrap(), "breakout \"Side room\" (b2c3d4e5…) created");
        assert_eq!(
            render(
                "blob",
                &json!({ "name": "notes.txt", "sha256": "s", "bytes": 12345 }),
                &c
            )
            .unwrap(),
            "attached notes.txt (12345 bytes)"
        );
        assert_eq!(
            render("leave", &json!({ "ok": true, "role": "observe" }), &c).unwrap(),
            "left 01010101…"
        );
        assert_eq!(render("say", &json!({ "n": 15, "grant_hash": "h", "author": { "agent": "human:ray", "node": "n" } }), &c).unwrap(), "said #15 as human:ray");
        assert!(render("mcp", &json!({}), &c).is_none());
    }

    #[test]
    fn continuation_lines_align_with_long_labels() {
        let speech = record(
            2,
            json!({ "type": "speech", "closes_grant": "h", "text": "first\nsecond" }),
            Some("human:ray-hwang"),
        );
        let line = scene_line(&speech, false, 80);
        let lines: Vec<&str> = line.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "#2    human:ray-hwang first");
        assert_eq!(lines[1], "                      second");
        assert_eq!(lines[1].chars().take_while(|c| *c == ' ').count(), 22);
    }

    #[test]
    fn oneline_with_narrow_width() {
        let speech = record(
            2,
            json!({ "type": "speech", "closes_grant": "h", "text": "content" }),
            Some("agent:x"),
        );
        let line = scene_line(&speech, true, 10);
        assert_eq!(line.lines().count(), 1);
        assert!(line.ends_with('…'));
        // When width < head_width, the line does not fit in width, but head is never truncated
    }

    #[test]
    fn leave_with_no_room() {
        let c = Context {
            room: None,
            current_room: None,
            oneline: false,
            width: 80,
        };
        assert_eq!(
            render("leave", &json!({ "ok": true, "role": "observe" }), &c).unwrap(),
            "left"
        );
    }

    #[test]
    fn whitespace_only_text_renders_as_empty_take() {
        let speech_ws = record(
            3,
            json!({ "type": "speech", "closes_grant": "h", "text": "  \n  " }),
            Some("human:test"),
        );
        let line = scene_line(&speech_ws, false, 80);
        assert!(line.contains("(empty take)"));
    }
}
