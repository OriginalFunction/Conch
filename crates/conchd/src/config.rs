//! `conchd.toml` in the data directory: settings the daemon reads at start that its
//! launcher (a service manager, `conch up`) cannot pass on the command line.
//!
//! ```toml
//! [operator]
//! origins = ["https://my-mac.tailnet.ts.net"]
//! ```

use std::path::Path;

use toml_edit::{DocumentMut, Item};

pub const FILE_NAME: &str = "conchd.toml";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct DaemonConfig {
    /// Browser origins trusted for the operator console besides loopback.
    pub operator_origins: Vec<String>,
}

impl DaemonConfig {
    /// Read `<data_dir>/conchd.toml`; a missing file is the default configuration.
    /// Errors name the file and the offending key.
    pub fn load(data_dir: &Path) -> Result<Self, String> {
        let path = data_dir.join(FILE_NAME);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        Self::parse(&text).map_err(|error| format!("{}: {error}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let document: DocumentMut = text.parse().map_err(|error| format!("{error}"))?;
        let mut config = Self::default();
        for (key, item) in document.iter() {
            match key {
                "operator" => config.operator_origins = operator_origins(item)?,
                other => return Err(format!("unknown table [{other}]")),
            }
        }
        Ok(config)
    }
}

fn operator_origins(item: &Item) -> Result<Vec<String>, String> {
    let table = item
        .as_table_like()
        .ok_or_else(|| "[operator] must be a table".to_owned())?;
    let mut origins = Vec::new();
    for (key, value) in table.iter() {
        match key {
            "origins" => {
                let array = value
                    .as_array()
                    .ok_or_else(|| "operator.origins must be an array of strings".to_owned())?;
                for entry in array.iter() {
                    let origin = entry
                        .as_str()
                        .ok_or_else(|| "operator.origins must be an array of strings".to_owned())?;
                    origins.push(origin.to_owned());
                }
            }
            other => return Err(format!("unknown key operator.{other}")),
        }
    }
    Ok(origins)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_the_default() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(
            DaemonConfig::load(dir.path()).unwrap(),
            DaemonConfig::default()
        );
    }

    #[test]
    fn parses_operator_origins() {
        let config = DaemonConfig::parse(
            "[operator]\norigins = [\"https://a.example\", \"http://b.example:8080\"]\n",
        )
        .unwrap();
        assert_eq!(
            config.operator_origins,
            vec![
                "https://a.example".to_owned(),
                "http://b.example:8080".to_owned()
            ]
        );
        assert_eq!(DaemonConfig::parse("").unwrap(), DaemonConfig::default());
        assert_eq!(
            DaemonConfig::parse("[operator]\n").unwrap(),
            DaemonConfig::default()
        );
    }

    #[test]
    fn rejects_wrong_shapes_and_unknown_keys() {
        for (text, needle) in [
            (
                "[operator]\norigins = \"https://a.example\"\n",
                "operator.origins",
            ),
            ("[operator]\norigins = [1]\n", "operator.origins"),
            ("[operator]\nallow = []\n", "operator.allow"),
            ("operator = 1\n", "[operator]"),
            ("[swarm]\n", "[swarm]"),
            ("not toml", "expected"),
        ] {
            let error = DaemonConfig::parse(text).unwrap_err();
            assert!(error.contains(needle), "{text:?} -> {error}");
        }
    }

    #[test]
    fn load_names_the_file_in_errors() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), "[operator]\norigins = 1\n").unwrap();
        let error = DaemonConfig::load(dir.path()).unwrap_err();
        assert!(
            error.contains(FILE_NAME) && error.contains("operator.origins"),
            "{error}"
        );
    }
}
