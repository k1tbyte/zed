use regex::Regex;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SecretsFormat {
    Dotenv,
    Yaml,
    Json,
}

impl SecretsFormat {
    pub(crate) fn sops_type(self) -> &'static str {
        match self {
            Self::Dotenv => "dotenv",
            Self::Yaml => "yaml",
            Self::Json => "json",
        }
    }

    pub(crate) fn from_file_name(name: &str) -> Option<Self> {
        let (_, extension) = name.rsplit_once('.')?;
        match extension {
            "yaml" | "yml" => Some(Self::Yaml),
            "json" => Some(Self::Json),
            "env" => Some(Self::Dotenv),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct GroupKey {
    pattern_index: usize,
    path: String,
}

impl GroupKey {
    pub(crate) fn title(&self) -> &str {
        &self.path
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SecretsFile {
    pub group: GroupKey,
    pub environment: Option<String>,
    pub format: SecretsFormat,
}

pub(crate) struct FilePatterns {
    patterns: Vec<(usize, Regex)>,
}

impl FilePatterns {
    pub(crate) fn new(patterns: &[String]) -> Self {
        let patterns = patterns
            .iter()
            .enumerate()
            .filter_map(|(index, pattern)| match compile_pattern(pattern) {
                Ok(regex) => Some((index, regex)),
                Err(error) => {
                    log::warn!("invalid sops file pattern {pattern:?}: {error}");
                    None
                }
            })
            .collect();
        Self { patterns }
    }

    pub(crate) fn classify(&self, path: &str) -> Option<SecretsFile> {
        let file_name = path.rsplit('/').next()?;
        let format = SecretsFormat::from_file_name(file_name)?;
        let (pattern_index, captures) = self
            .patterns
            .iter()
            .find_map(|(index, regex)| Some((*index, regex.captures(path)?)))?;
        let environment = captures.name("env");
        let group_path = match environment {
            Some(environment) => format!(
                "{}{{env}}{}",
                path.get(..environment.start())?,
                path.get(environment.end()..)?
            ),
            None => path.to_string(),
        };
        Some(SecretsFile {
            group: GroupKey {
                pattern_index,
                path: group_path,
            },
            environment: environment.map(|environment| environment.as_str().to_string()),
            format,
        })
    }
}

fn compile_pattern(pattern: &str) -> Result<Regex, regex::Error> {
    let mut expression = String::from("^");
    let mut rest = pattern;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("**/") {
            expression.push_str("(?:[^/]+/)*");
            rest = after;
        } else if let Some(after) = rest.strip_prefix("{env}") {
            expression.push_str("(?P<env>[^/.]+)");
            rest = after;
        } else if let Some(after) = rest.strip_prefix('*') {
            expression.push_str("[^/]+");
            rest = after;
        } else {
            let mut characters = rest.chars();
            if let Some(character) = characters.next() {
                expression.push_str(&regex::escape(character.encode_utf8(&mut [0; 4])));
            }
            rest = characters.as_str();
        }
    }
    expression.push('$');
    Regex::new(&expression)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patterns() -> FilePatterns {
        let defaults = ["**/*.{env}.sops.*", "**/{env}/*.sops.*", "**/*.sops.*"].map(String::from);
        FilePatterns::new(&defaults)
    }

    #[test]
    fn test_same_group_across_environments() {
        let patterns = patterns();
        let staging = patterns.classify("config/secrets.stg.sops.yaml").unwrap();
        let production = patterns.classify("config/secrets.prod.sops.yaml").unwrap();
        assert_eq!(staging.group, production.group);
        assert_eq!(staging.group.title(), "config/secrets.{env}.sops.yaml");
        assert_eq!(staging.environment.as_deref(), Some("stg"));
        assert_eq!(production.environment.as_deref(), Some("prod"));
        assert_eq!(staging.format, SecretsFormat::Yaml);
    }

    #[test]
    fn test_dotenv() {
        let file = patterns().classify(".env.stg.sops.env").unwrap();
        assert_eq!(file.format, SecretsFormat::Dotenv);
        assert_eq!(file.environment.as_deref(), Some("stg"));
        assert_eq!(file.group.title(), ".env.{env}.sops.env");
        // sops encrypts files without a known extension in its binary format.
        assert_eq!(patterns().classify(".env.stg.sops"), None);
    }

    #[test]
    fn test_environment_directories() {
        let patterns = patterns();
        let staging = patterns.classify("stg/secrets.sops.yaml").unwrap();
        let production = patterns.classify("prod/secrets.sops.yaml").unwrap();
        assert_eq!(staging.group, production.group);
        assert_eq!(staging.group.title(), "{env}/secrets.sops.yaml");
        assert_eq!(staging.environment.as_deref(), Some("stg"));
        assert_eq!(production.environment.as_deref(), Some("prod"));
    }

    #[test]
    fn test_standalone_file() {
        let file = patterns().classify("secrets.sops.yaml").unwrap();
        assert_eq!(file.environment, None);
        assert_eq!(file.group.title(), "secrets.sops.yaml");
    }

    #[test]
    fn test_non_matching_files() {
        let patterns = patterns();
        assert_eq!(patterns.classify(".sops.yaml"), None);
        assert_eq!(patterns.classify("README.md"), None);
        assert_eq!(patterns.classify("secrets.sops.ini"), None);
    }

    #[test]
    fn test_dotted_name_with_environment() {
        let file = patterns().classify("a.b.stg.sops.json").unwrap();
        assert_eq!(file.environment.as_deref(), Some("stg"));
        assert_eq!(file.format, SecretsFormat::Json);
        assert_eq!(file.group.title(), "a.b.{env}.sops.json");
    }

    #[test]
    fn test_invalid_pattern_is_skipped() {
        let patterns = FilePatterns::new(&["{env}.{env}.env".to_string(), "*.env".to_string()]);
        assert!(patterns.classify("a.env").is_some());
    }
}
