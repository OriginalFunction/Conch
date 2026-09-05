//! Byte-preserving edits of host config files. JSON/JSONC use a small scanner so unrelated
//! bytes (comments, key order, indentation) survive; TOML uses `toml_edit`.

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("config is not a JSON object: {0}")]
    Json(String),
    #[error("config is not valid TOML: {0}")]
    Toml(#[from] toml_edit::TomlError),
}

pub mod json {
    use super::EditError;

    /// Set `path` (an object member chain) to `value` (compact JSON text), creating
    /// intermediate objects. Everything outside the touched member is byte-identical.
    pub fn set_member(text: &str, path: &[&str], value: &str) -> Result<String, EditError> {
        assert!(!path.is_empty());
        let unit = detect_indent(text);
        let mut text = if text.trim().is_empty() {
            "{\n}\n".to_string()
        } else {
            text.to_string()
        };
        let mut object = root_object(&text)?;
        let mut depth_indent = String::new();
        for (index, key) in path.iter().enumerate() {
            let last = index + 1 == path.len();
            let members = members_of(&text, object)?;
            let member_indent = members
                .first()
                .map(|m| line_indent(&text, m.key_start))
                .unwrap_or_else(|| format!("{depth_indent}{unit}"));
            match members.iter().find(|m| m.key == *key) {
                Some(m) if last => {
                    let rendered = render_value(value, &member_indent, &unit);
                    text.replace_range(m.value_start..m.value_end, &rendered);
                    return Ok(text);
                }
                Some(m) => {
                    let span = (m.value_start, m.value_end);
                    if text.as_bytes()[span.0] != b'{' {
                        return Err(EditError::Json(format!("member {key} is not an object")));
                    }
                    object = span;
                    depth_indent = member_indent;
                }
                None => {
                    let body = if last {
                        render_value(value, &member_indent, &unit)
                    } else {
                        "{\n".to_string() + &member_indent + "}"
                    };
                    let new_member = format!("\"{key}\": {body}");
                    if members.is_empty() {
                        // The object interior is empty (or whitespace-only, as with a freshly
                        // created placeholder's "\n{indent}"). Replace it outright rather than
                        // inserting after `{`, or the placeholder's own closing whitespace would
                        // survive alongside the new member.
                        let prefix = format!("\n{member_indent}");
                        let suffix = format!("\n{depth_indent}");
                        text.replace_range(
                            object.0 + 1..object.1 - 1,
                            &format!("{prefix}{new_member}{suffix}"),
                        );
                    } else {
                        let insert_at = insertion_point(&text, object, &members);
                        let prefix = if has_trailing_comma(&text, &members, object) {
                            format!("\n{member_indent}")
                        } else {
                            format!(",\n{member_indent}")
                        };
                        text.insert_str(insert_at, &format!("{prefix}{new_member}"));
                    }
                    if last {
                        return Ok(text);
                    }
                    object = root_object(&text)?; // re-scan from the root along the path so far
                    for step in &path[..=index] {
                        let ms = members_of(&text, object)?;
                        let m = ms.iter().find(|m| m.key == *step).expect("just inserted");
                        object = (m.value_start, m.value_end);
                    }
                    depth_indent = member_indent;
                }
            }
        }
        unreachable!()
    }

    /// Remove `//` and `/* */` comments (outside strings). Used for validation and doctor reads.
    pub fn strip_comments(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => {
                    let end = string_end(bytes, i);
                    out.push_str(&text[i..end]);
                    i = end;
                }
                b'/' if bytes.get(i + 1) == Some(&b'/') => {
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    i += 2;
                    while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                        i += 1;
                    }
                    i += 2;
                }
                b => {
                    out.push(b as char);
                    i += 1;
                }
            }
        }
        out
    }

    struct Member {
        key: String,
        key_start: usize,
        value_start: usize,
        value_end: usize,
    }

    fn string_end(bytes: &[u8], start: usize) -> usize {
        let mut i = start + 1;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 2,
                b'"' => return i + 1,
                _ => i += 1,
            }
        }
        bytes.len()
    }

    /// Skip whitespace and comments from `i`.
    fn skip_trivia(bytes: &[u8], mut i: usize) -> usize {
        loop {
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if bytes.get(i) == Some(&b'/') && bytes.get(i + 1) == Some(&b'/') {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if bytes.get(i) == Some(&b'/') && bytes.get(i + 1) == Some(&b'*') {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            return i;
        }
    }

    /// End index (exclusive) of the value starting at `i`.
    fn value_end(bytes: &[u8], i: usize) -> Result<usize, EditError> {
        match bytes.get(i) {
            Some(b'"') => Ok(string_end(bytes, i)),
            Some(b'{') | Some(b'[') => {
                let mut depth = 0usize;
                let mut j = i;
                while j < bytes.len() {
                    match bytes[j] {
                        b'"' => {
                            j = string_end(bytes, j);
                            continue;
                        }
                        b'/' => {
                            let k = skip_trivia(bytes, j);
                            if k != j {
                                j = k;
                                continue;
                            }
                            j += 1;
                            continue;
                        }
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                return Ok(j + 1);
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                Err(EditError::Json("unterminated object or array".into()))
            }
            Some(_) => {
                let mut j = i;
                while j < bytes.len()
                    && !matches!(bytes[j], b',' | b'}' | b']')
                    && !bytes[j].is_ascii_whitespace()
                    && bytes[j] != b'/'
                {
                    j += 1;
                }
                Ok(j)
            }
            None => Err(EditError::Json("unexpected end of input".into())),
        }
    }

    fn root_object(text: &str) -> Result<(usize, usize), EditError> {
        let bytes = text.as_bytes();
        let start = skip_trivia(bytes, 0);
        if bytes.get(start) != Some(&b'{') {
            return Err(EditError::Json("top level is not an object".into()));
        }
        let end = value_end(bytes, start)?;
        if skip_trivia(bytes, end) != bytes.len() {
            return Err(EditError::Json("trailing content after object".into()));
        }
        Ok((start, end))
    }

    fn members_of(text: &str, object: (usize, usize)) -> Result<Vec<Member>, EditError> {
        let bytes = text.as_bytes();
        let mut members = Vec::new();
        let mut i = skip_trivia(bytes, object.0 + 1);
        while i < object.1 - 1 {
            if bytes[i] == b',' {
                i = skip_trivia(bytes, i + 1);
                continue;
            }
            if bytes[i] != b'"' {
                return Err(EditError::Json(format!("expected a key at byte {i}")));
            }
            let key_start = i;
            let key_end = string_end(bytes, i);
            let key: String = serde_json::from_str(&text[key_start..key_end])
                .map_err(|e| EditError::Json(e.to_string()))?;
            let colon = skip_trivia(bytes, key_end);
            if bytes.get(colon) != Some(&b':') {
                return Err(EditError::Json(format!("expected ':' after key {key}")));
            }
            let value_start = skip_trivia(bytes, colon + 1);
            let value_end = value_end(bytes, value_start)?;
            members.push(Member {
                key,
                key_start,
                value_start,
                value_end,
            });
            i = skip_trivia(bytes, value_end);
        }
        Ok(members)
    }

    fn line_indent(text: &str, at: usize) -> String {
        let line_start = text[..at].rfind('\n').map_or(0, |n| n + 1);
        text[line_start..at]
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect()
    }

    fn detect_indent(text: &str) -> String {
        text.lines()
            .find_map(|line| {
                let ws: String = line
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect();
                (!ws.is_empty() && ws.len() < line.len()).then_some(ws)
            })
            .unwrap_or_else(|| "  ".into())
    }

    fn has_trailing_comma(text: &str, members: &[Member], object: (usize, usize)) -> bool {
        let last = members.last().expect("non-empty");
        let after = skip_trivia(text.as_bytes(), last.value_end);
        after < object.1 && text.as_bytes()[after] == b','
    }

    fn insertion_point(text: &str, object: (usize, usize), members: &[Member]) -> usize {
        match members.last() {
            None => object.0 + 1,
            Some(last) => {
                let after = skip_trivia(text.as_bytes(), last.value_end);
                if after < object.1 && text.as_bytes()[after] == b',' {
                    after + 1
                } else {
                    last.value_end
                }
            }
        }
    }

    /// Pretty-print compact JSON `value` so nested lines sit at `base` + `unit` multiples.
    fn render_value(value: &str, base: &str, unit: &str) -> String {
        // serde_json's Value sorts object keys, but the entry text from `hosts` is authoritative
        // for member order, so render directly from the compact text instead of round-tripping
        // through `serde_json::Value`.
        let pretty = reorder_like(value);
        pretty
            .lines()
            .enumerate()
            .map(|(n, line)| {
                let depth = line.len() - line.trim_start().len();
                let re = format!("{base}{}{}", unit.repeat(depth / 2), line.trim_start());
                if n == 0 {
                    line.trim_start().to_string()
                } else {
                    re
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Re-render compact JSON `compact` with two-space indentation, preserving member order.
    fn reorder_like(compact: &str) -> String {
        fn walk(bytes: &[u8], i: usize, depth: usize, out: &mut String) -> usize {
            let pad = |d: usize| "  ".repeat(d);
            match bytes[i] {
                b'{' => {
                    let mut j = skip_trivia(bytes, i + 1);
                    if bytes[j] == b'}' {
                        out.push_str("{}");
                        return j + 1;
                    }
                    out.push_str("{\n");
                    loop {
                        let key_end = string_end(bytes, j);
                        out.push_str(&pad(depth + 1));
                        out.push_str(&String::from_utf8_lossy(&bytes[j..key_end]));
                        out.push_str(": ");
                        j = skip_trivia(bytes, skip_trivia(bytes, key_end) + 1);
                        j = walk(bytes, j, depth + 1, out);
                        j = skip_trivia(bytes, j);
                        if bytes[j] == b',' {
                            out.push_str(",\n");
                            j = skip_trivia(bytes, j + 1);
                        } else {
                            out.push('\n');
                            break;
                        }
                    }
                    out.push_str(&pad(depth));
                    out.push('}');
                    j + 1
                }
                b'[' => {
                    let mut j = skip_trivia(bytes, i + 1);
                    if bytes[j] == b']' {
                        out.push_str("[]");
                        return j + 1;
                    }
                    out.push_str("[\n");
                    loop {
                        out.push_str(&pad(depth + 1));
                        j = walk(bytes, j, depth + 1, out);
                        j = skip_trivia(bytes, j);
                        if bytes[j] == b',' {
                            out.push_str(",\n");
                            j = skip_trivia(bytes, j + 1);
                        } else {
                            out.push('\n');
                            break;
                        }
                    }
                    out.push_str(&pad(depth));
                    out.push(']');
                    j + 1
                }
                _ => {
                    let end = value_end(bytes, i).expect("valid scalar");
                    out.push_str(&String::from_utf8_lossy(&bytes[i..end]));
                    end
                }
            }
        }
        let mut out = String::new();
        walk(compact.as_bytes(), 0, 0, &mut out);
        out
    }
}

pub mod toml {
    use super::EditError;
    use crate::hosts::Env;
    use toml_edit::{value, Array, DocumentMut, InlineTable, Item, Table};

    #[derive(Clone, Copy)]
    pub enum EnvStyle {
        SubTable,
        Inline,
    }

    pub struct Server<'a> {
        pub command: &'a str,
        pub args: &'a [String],
        pub env: &'a Env,
        pub env_style: EnvStyle,
    }

    pub fn set_server(
        text: &str,
        table: &str,
        name: &str,
        server: &Server,
    ) -> Result<String, EditError> {
        let mut doc: DocumentMut = text.parse()?;
        let parent = doc.entry(table).or_insert(Item::Table(Table::new()));
        let parent = parent
            .as_table_mut()
            .ok_or_else(|| EditError::Json(format!("{table} is not a table")))?;
        parent.set_implicit(true);
        let mut entry = Table::new();
        entry["command"] = value(server.command);
        entry["args"] = value(server.args.iter().map(|s| s.as_str()).collect::<Array>());
        if !server.env.0.is_empty() {
            match server.env_style {
                EnvStyle::SubTable => {
                    let mut env = Table::new();
                    for (k, v) in &server.env.0 {
                        env[k.as_str()] = value(v.as_str());
                    }
                    entry["env"] = Item::Table(env);
                }
                EnvStyle::Inline => {
                    let mut env = InlineTable::new();
                    for (k, v) in &server.env.0 {
                        env.insert(k, v.as_str().into());
                    }
                    entry["env"] = value(env);
                }
            }
        }
        parent.insert(name, Item::Table(entry));
        Ok(doc.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosts::Env;

    #[test]
    fn json_inserts_into_existing_object_preserving_indent_and_neighbours() {
        let input = "{\n    \"other\": 1,\n    \"mcpServers\": {\n        \"pencil\": {\"command\": \"p\"}\n    }\n}\n";
        let out = json::set_member(
            input,
            &["mcpServers", "conch"],
            r#"{"command":"c","args":["mcp"]}"#,
        )
        .unwrap();
        assert_eq!(out, "{\n    \"other\": 1,\n    \"mcpServers\": {\n        \"pencil\": {\"command\": \"p\"},\n        \"conch\": {\n            \"command\": \"c\",\n            \"args\": [\n                \"mcp\"\n            ]\n        }\n    }\n}\n");
    }

    #[test]
    fn json_replaces_existing_member_in_place() {
        let input = "{\n  \"mcpServers\": {\n    \"conch\": {\"command\": \"old\"},\n    \"z\": 1\n  }\n}\n";
        let out =
            json::set_member(input, &["mcpServers", "conch"], r#"{"command":"new"}"#).unwrap();
        assert_eq!(out, "{\n  \"mcpServers\": {\n    \"conch\": {\n      \"command\": \"new\"\n    },\n    \"z\": 1\n  }\n}\n");
    }

    #[test]
    fn json_creates_missing_parent_and_empty_file() {
        let out = json::set_member("{}", &["mcpServers", "conch"], r#"{"command":"c"}"#).unwrap();
        assert_eq!(
            out,
            "{\n  \"mcpServers\": {\n    \"conch\": {\n      \"command\": \"c\"\n    }\n  }\n}"
        );
        let out = json::set_member("", &["mcp", "conch"], r#"{"type":"local"}"#).unwrap();
        assert_eq!(
            out,
            "{\n  \"mcp\": {\n    \"conch\": {\n      \"type\": \"local\"\n    }\n  }\n}\n"
        );
    }

    #[test]
    fn jsonc_keeps_comments_and_trailing_commas_elsewhere() {
        let input = "{\n  // keep me\n  \"$schema\": \"x\",\n  \"mcp\": {\n    \"pencil\": { \"type\": \"local\" }, // trailing\n  },\n}\n";
        let out = json::set_member(input, &["mcp", "conch"], r#"{"type":"local"}"#).unwrap();
        assert!(out.contains("// keep me"));
        assert!(out.contains("// trailing"));
        assert!(out.contains("\"conch\": {\n      \"type\": \"local\"\n    }"));
        let _: serde_json::Value = serde_json::from_str(
            &json::strip_comments(&out)
                .replace(",\n}", "\n}")
                .replace(",\n  }", "\n  }"),
        )
        .unwrap();
    }

    #[test]
    fn json_refuses_malformed_input() {
        assert!(json::set_member("{ \"a\": ", &["a", "b"], "1").is_err());
        assert!(json::set_member("[1,2]", &["a"], "1").is_err());
    }

    #[test]
    fn toml_inserts_server_table_preserving_comments() {
        let input = "# my codex\nmodel = \"gpt\"\n\n[mcp_servers.pencil]\ncommand = \"p\"\nargs = [\"a\"]\n";
        let env = Env(vec![("K".into(), "V".into())]);
        let server = toml::Server {
            command: "/b/conch",
            args: &["--agent".into(), "agent:codex".into(), "mcp".into()],
            env: &env,
            env_style: toml::EnvStyle::SubTable,
        };
        let out = toml::set_server(input, "mcp_servers", "conch", &server).unwrap();
        assert!(out.starts_with("# my codex\nmodel = \"gpt\"\n"));
        assert!(out.contains("[mcp_servers.pencil]\ncommand = \"p\""));
        assert!(out.contains("[mcp_servers.conch]\ncommand = \"/b/conch\"\nargs = [\"--agent\", \"agent:codex\", \"mcp\"]\n"));
        assert!(out.contains("[mcp_servers.conch.env]\nK = \"V\"\n"));
        let inline = toml::Server {
            env_style: toml::EnvStyle::Inline,
            ..server
        };
        let out = toml::set_server("", "mcp_servers", "conch", &inline).unwrap();
        assert!(out.contains("env = { K = \"V\" }"), "{out}");
    }
}
