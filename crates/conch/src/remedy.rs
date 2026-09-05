//! Human remedies for the errors a new user hits first.

pub fn connect_error(node_addr: &str) -> String {
    conch_launch::connect_error(node_addr)
}

/// A second line for a wire error, keyed by error code and the CLI command that produced it.
pub fn for_code(code: &str, command: &str) -> Option<&'static str> {
    Some(match (code, command) {
        ("no_grant", _) => {
            "raise your hand and wait for the floor: `conch raise-hand && conch wait-for-floor`"
        }
        ("unknown_room", _) => "join it first: `conch join <ticket>`",
        ("not_moderator", _) => {
            "this room is in stick mode; `grant`/`yank` need `conch config --mode moderator`"
        }
        ("timeout", "wait-for-floor") => {
            "your hand stays raised for 24 h; run `conch wait-for-floor` again"
        }
        ("unavailable", "join") => {
            "no peer could provide the room; check the ticket still carries its token"
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_codes_get_a_remedy_and_unknown_ones_none() {
        assert_eq!(
            for_code("no_grant", "speak"),
            Some("raise your hand and wait for the floor: `conch raise-hand && conch wait-for-floor`")
        );
        assert_eq!(
            for_code("unknown_room", "history"),
            Some("join it first: `conch join <ticket>`")
        );
        assert!(for_code("not_moderator", "grant")
            .unwrap()
            .contains("moderator"));
        // Command-specific remedies only fire for their command.
        assert!(for_code("timeout", "wait-for-floor").is_some());
        assert_eq!(for_code("timeout", "wait-for-history"), None);
        assert!(for_code("unavailable", "join").is_some());
        assert_eq!(for_code("unavailable", "speak"), None);
        assert_eq!(for_code("internal", "speak"), None);
        assert_eq!(
            connect_error("127.0.0.1:7421"),
            "conchd is not running on 127.0.0.1:7421. Start it with `conch up` \
             (or `brew services start conch`)."
        );
    }
}
