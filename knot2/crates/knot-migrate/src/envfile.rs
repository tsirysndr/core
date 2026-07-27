use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum EnvFileError {
    #[error("read env file {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[derive(Debug, Default)]
pub struct EnvFile {
    values: BTreeMap<String, String>,
}

impl EnvFile {
    pub fn read(path: &Path) -> Result<Self, EnvFileError> {
        let body = std::fs::read_to_string(path).map_err(|source| EnvFileError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self::parse(&body))
    }

    pub fn parse(body: &str) -> Self {
        let values = body
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| line.strip_prefix("export ").unwrap_or(line))
            .filter_map(|line| line.split_once('='))
            .filter_map(|(key, value)| unquote(value).map(|value| (key.trim().to_string(), value)))
            .collect();
        Self { values }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }
}

fn unquote(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    ['"', '\'']
        .into_iter()
        .find_map(|quote| trimmed.strip_prefix(quote).map(|rest| (quote, rest)))
        .map_or_else(
            || Some(strip_comment(trimmed).to_string()),
            |(quote, rest)| rest.split_once(quote).map(|(inner, _)| inner.to_string()),
        )
}

fn strip_comment(value: &str) -> &str {
    value
        .match_indices('#')
        .find(|(index, _)| {
            value[..*index]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace)
        })
        .map_or(value, |(index, _)| &value[..index])
        .trim_end()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing_keeps_assignments_and_drops_every_other_line() {
        let file = EnvFile::parse(
            r#"# knot1
KNOT_SERVER_HOSTNAME=oyster.cafe
export KNOT_SERVER_OWNER="did:plc:nel"

KNOT_SERVER_PLC_URL='https://plc.oyster.cafe'
broken line
KNOT_REPO_SCAN_PATH=/data/repos # tangled default
KNOT_QUOTED_HOSTNAME="nel.pet" # quoted
KNOT_SERVER_SECRET='a # b'
KNOT_DEV_FLAGS=#literal
KNOT_APPVIEW_URL=https://tangled.test/#frag
KNOT_UNTERMINATED="oyster.cafe
KNOT_ALSO_UNTERMINATED='did:plc:nel
KNOT_LAST=kept
"#,
        );
        assert_eq!(file.get("KNOT_SERVER_HOSTNAME"), Some("oyster.cafe"));
        assert_eq!(file.get("KNOT_SERVER_OWNER"), Some("did:plc:nel"));
        assert_eq!(
            file.get("KNOT_SERVER_PLC_URL"),
            Some("https://plc.oyster.cafe")
        );
        assert_eq!(file.get("broken"), None);
        assert_eq!(file.get("KNOT_REPO_SCAN_PATH"), Some("/data/repos"));
        assert_eq!(file.get("KNOT_QUOTED_HOSTNAME"), Some("nel.pet"));
        assert_eq!(file.get("KNOT_SERVER_SECRET"), Some("a # b"));
        assert_eq!(file.get("KNOT_DEV_FLAGS"), Some("#literal"));
        assert_eq!(
            file.get("KNOT_APPVIEW_URL"),
            Some("https://tangled.test/#frag")
        );
        assert_eq!(file.get("KNOT_UNTERMINATED"), None);
        assert_eq!(file.get("KNOT_ALSO_UNTERMINATED"), None);
        assert_eq!(
            file.get("KNOT_LAST"),
            Some("kept"),
            "an unterminated quote drops its own line and nothing after it"
        );
    }
}
