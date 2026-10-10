//! Two-language output (English, Traditional Chinese).

/// Output language for digests, CLI text and notifications.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    En,
    Zh,
}

impl Lang {
    /// Resolve the `language` config value. `auto` reads `SCREENSENSE_LANG`,
    /// then the usual locale variables; anything starting with `zh` selects Chinese.
    pub fn resolve(setting: &str) -> Lang {
        match setting.trim().to_ascii_lowercase().as_str() {
            "zh" | "zh-tw" | "zh_tw" | "zh-hant" => Lang::Zh,
            "en" => Lang::En,
            _ => {
                for var in ["SCREENSENSE_LANG", "LC_ALL", "LC_MESSAGES", "LANG"] {
                    if let Ok(v) = std::env::var(var) {
                        if !v.is_empty() {
                            return if v.to_ascii_lowercase().starts_with("zh") {
                                Lang::Zh
                            } else {
                                Lang::En
                            };
                        }
                    }
                }
                Lang::En
            }
        }
    }

    /// Pick the string for this language.
    pub fn t<'a>(self, en: &'a str, zh: &'a str) -> &'a str {
        match self {
            Lang::En => en,
            Lang::Zh => zh,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_setting_wins() {
        assert_eq!(Lang::resolve("zh"), Lang::Zh);
        assert_eq!(Lang::resolve("EN"), Lang::En);
        assert_eq!(Lang::Zh.t("a", "b"), "b");
    }
}
