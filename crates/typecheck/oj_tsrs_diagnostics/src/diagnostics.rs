use std::fmt;
use std::sync::OnceLock;

use rustc_hash::FxHashMap;

use crate::generated::ALL_MESSAGES;

#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Category {
    Warning,
    Error,
    Suggestion,
    Message,
}

impl Category {
    pub fn name(self) -> &'static str {
        match self {
            Category::Warning => "warning",
            Category::Error => "error",
            Category::Suggestion => "suggestion",
            Category::Message => "message",
        }
    }
}

// Go stringer output (`Category.String()`).
impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Category::Warning => "CategoryWarning",
            Category::Error => "CategoryError",
            Category::Suggestion => "CategorySuggestion",
            Category::Message => "CategoryMessage",
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct Key(pub &'static str);

impl Key {
    pub fn as_str(self) -> &'static str {
        self.0
    }
}

impl std::ops::Deref for Key {
    type Target = str;
    fn deref(&self) -> &str {
        self.0
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

#[derive(Debug)]
pub struct Message {
    pub(crate) code: i32,
    pub(crate) category: Category,
    pub(crate) key: Key,
    pub(crate) text: &'static str,
    pub(crate) reports_unnecessary: bool,
    pub(crate) elided_in_compatibility_pyramid: bool,
    pub(crate) reports_deprecated: bool,
}

impl Message {
    pub fn code(&self) -> i32 {
        self.code
    }
    pub fn category(&self) -> Category {
        self.category
    }
    pub fn key(&self) -> Key {
        self.key
    }
    pub fn text(&self) -> &'static str {
        self.text
    }
    pub fn reports_unnecessary(&self) -> bool {
        self.reports_unnecessary
    }
    pub fn elided_in_compatibility_pyramid(&self) -> bool {
        self.elided_in_compatibility_pyramid
    }
    pub fn reports_deprecated(&self) -> bool {
        self.reports_deprecated
    }

    pub fn localize(&self, args: &[&dyn fmt::Display]) -> String {
        localize(Some(self), Key::default(), &stringify_args(args))
    }

    pub fn format(&self, args: &[&dyn fmt::Display]) -> String {
        format(self.text, args)
    }
}

// For debugging only.
impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.text)
    }
}

// Most diagnostics carry a message pointer, so only build the lookup when a key is used.
static MESSAGES_BY_KEY: OnceLock<FxHashMap<&'static str, &'static Message>> = OnceLock::new();

fn messages_by_key() -> &'static FxHashMap<&'static str, &'static Message> {
    MESSAGES_BY_KEY.get_or_init(|| {
        let mut messages =
            FxHashMap::with_capacity_and_hasher(ALL_MESSAGES.len(), rustc_hash::FxBuildHasher);
        messages.extend(ALL_MESSAGES.iter().map(|&message| (message.key.0, message)));
        messages
    })
}

pub fn key_to_message(key: &str) -> Option<&'static Message> {
    messages_by_key().get(key).copied()
}

pub fn localize(message: Option<&Message>, key: Key, args: &[String]) -> String {
    let message = match message {
        Some(message) => message,
        None => match key_to_message(key.0) {
            Some(message) => message,
            None => panic!("Unknown diagnostic message: {}", key.0),
        },
    };
    format_strings(message.text, args)
}

pub fn format(text: &str, args: &[&dyn fmt::Display]) -> String {
    if args.is_empty() {
        return text.to_string();
    }
    format_strings(text, &stringify_args(args))
}

// Go `Format(text, args []string)`, replacing every `{N}` (regexp `{(\d+)}`) with args[N].
pub fn format_strings(text: &str, args: &[String]) -> String {
    if args.is_empty() {
        return text.to_string();
    }
    let bytes = text.as_bytes();
    let mut result = String::with_capacity(text.len());
    let mut last = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let mut j = i + 1;
            j += (j..bytes.len()).take_while(|&index| bytes[index].is_ascii_digit()).count();
            if j > i + 1 && j < bytes.len() && bytes[j] == b'}' {
                let index = match text[i + 1..j].parse::<usize>() {
                    Ok(index) if index < args.len() => index,
                    _ => panic!("Invalid formatting placeholder"),
                };
                result.push_str(&text[last..i]);
                result.push_str(&args[index]);
                i = j + 1;
                last = i;
                continue;
            }
        }
        i += 1;
    }
    result.push_str(&text[last..]);
    result
}

pub fn stringify_args(args: &[&dyn fmt::Display]) -> Vec<String> {
    args.iter().map(std::string::ToString::to_string).collect()
}

impl Message {
    /// A message outside the catalog: code -1, an error, the given text. The text is `'static` like the
    /// catalog's; a caller with run-time text allocates it where the diagnostic that uses it lives
    /// (`tsrs_ast::new_ad_hoc_message`).
    pub const fn ad_hoc(text: &'static str) -> Message {
        Message {
            code: -1,
            category: Category::Error,
            key: Key("-1"),
            text,
            reports_unnecessary: false,
            elided_in_compatibility_pyramid: false,
            reports_deprecated: false,
        }
    }
}
