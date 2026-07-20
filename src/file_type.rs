use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileType {
    Fb2,
    Fb2Zip,
    Epub,
    Mobi,
    Html,
}

impl FileType {
    pub fn as_str(self) -> &'static str {
        match self {
            FileType::Fb2 => "fb2",
            FileType::Fb2Zip => "fb2zip",
            FileType::Epub => "epub",
            FileType::Mobi => "mobi",
            FileType::Html => "html",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Result<FileType, serde_json::Error> {
        let json = serde_json::to_string(raw).unwrap();
        serde_json::from_str(&json)
    }

    #[test]
    fn accepts_all_allowlisted_values() {
        assert_eq!(parse("fb2").unwrap(), FileType::Fb2);
        assert_eq!(parse("fb2zip").unwrap(), FileType::Fb2Zip);
        assert_eq!(parse("epub").unwrap(), FileType::Epub);
        assert_eq!(parse("mobi").unwrap(), FileType::Mobi);
        assert_eq!(parse("html").unwrap(), FileType::Html);
    }

    #[test]
    fn rejects_path_traversal_payload() {
        // What "%2F" decodes to before it reaches our validation.
        assert!(parse("../admin").is_err());
    }

    #[test]
    fn rejects_query_injection_payload() {
        // What "%3F" decodes to before it reaches our validation.
        assert!(parse("epub?x=y").is_err());
    }

    #[test]
    fn rejects_case_variants() {
        assert!(parse("EPUB").is_err());
        assert!(parse("Fb2").is_err());
    }

    #[test]
    fn rejects_unknown_extension() {
        assert!(parse("pdf").is_err());
    }

    #[test]
    fn rejects_empty_string() {
        assert!(parse("").is_err());
    }

    #[test]
    fn as_str_round_trips_through_deserialize() {
        for ft in [
            FileType::Fb2,
            FileType::Fb2Zip,
            FileType::Epub,
            FileType::Mobi,
            FileType::Html,
        ] {
            assert_eq!(parse(ft.as_str()).unwrap(), ft);
        }
    }
}
