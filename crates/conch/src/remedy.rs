//! Human remedies for the errors a new user hits first.

pub fn connect_error(node_addr: &str) -> String {
    format!("conchd is not running on {node_addr}. Start it with `conch up` (or `brew services start conch`).")
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
