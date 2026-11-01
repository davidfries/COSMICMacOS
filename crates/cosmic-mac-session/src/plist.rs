// SPDX-License-Identifier: GPL-3.0-or-later
//! A tiny XML property-list writer.
//!
//! launchd agent plists only need strings, booleans, integers, arrays and
//! dictionaries, so a full plist crate is not worth the dependency. Keys keep
//! their insertion order, which makes the output stable and easy to assert
//! in tests. Text is escaped for XML element content (`&`, `<`, `>`).

/// A property-list value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    String(String),
    Bool(bool),
    Integer(i64),
    Array(Vec<Value>),
    Dict(Dict),
}

/// A dictionary that keeps keys in insertion order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dict(Vec<(String, Value)>);

impl Dict {
    pub fn new() -> Self {
        Dict(Vec::new())
    }

    /// Appends `key` (builder style). Keys are expected to be unique.
    pub fn with(mut self, key: &str, value: Value) -> Self {
        self.0.push((key.to_owned(), value));
        self
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| k.as_str())
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Value::String(value.to_owned())
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Value::String(value)
    }
}

/// Serialises `root` as an XML plist document (tab-indented, like Apple's
/// tools write it).
pub fn to_xml(root: &Dict) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n",
    );
    write_dict(&mut out, root, 0);
    out.push_str("</plist>\n");
    out
}

/// Escapes text for XML element content.
pub fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push('\t');
    }
}

fn write_dict(out: &mut String, dict: &Dict, depth: usize) {
    indent(out, depth);
    out.push_str("<dict>\n");
    for (key, value) in &dict.0 {
        indent(out, depth + 1);
        out.push_str("<key>");
        out.push_str(&escape(key));
        out.push_str("</key>\n");
        write_value(out, value, depth + 1);
    }
    indent(out, depth);
    out.push_str("</dict>\n");
}

fn write_value(out: &mut String, value: &Value, depth: usize) {
    match value {
        Value::String(text) => {
            indent(out, depth);
            out.push_str("<string>");
            out.push_str(&escape(text));
            out.push_str("</string>\n");
        }
        Value::Bool(flag) => {
            indent(out, depth);
            out.push_str(if *flag { "<true/>\n" } else { "<false/>\n" });
        }
        Value::Integer(number) => {
            indent(out, depth);
            out.push_str(&format!("<integer>{number}</integer>\n"));
        }
        Value::Array(items) => {
            indent(out, depth);
            out.push_str("<array>\n");
            for item in items {
                write_value(out, item, depth + 1);
            }
            indent(out, depth);
            out.push_str("</array>\n");
        }
        Value::Dict(dict) => write_dict(out, dict, depth),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_xml_special_characters() {
        assert_eq!(escape("a & b < c > d"), "a &amp; b &lt; c &gt; d");
        assert_eq!(escape("plain/path with spaces"), "plain/path with spaces");
        assert_eq!(escape("&amp;"), "&amp;amp;");
    }

    #[test]
    fn writes_nested_values_in_insertion_order() {
        let root = Dict::new()
            .with("Label", "x".into())
            .with("On", Value::Bool(true))
            .with("Off", Value::Bool(false))
            .with("N", Value::Integer(5))
            .with("List", Value::Array(vec!["a<b".into()]))
            .with("Sub", Value::Dict(Dict::new().with("K&", "v".into())));
        let xml = to_xml(&root);
        let expected = "\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
\t<key>Label</key>
\t<string>x</string>
\t<key>On</key>
\t<true/>
\t<key>Off</key>
\t<false/>
\t<key>N</key>
\t<integer>5</integer>
\t<key>List</key>
\t<array>
\t\t<string>a&lt;b</string>
\t</array>
\t<key>Sub</key>
\t<dict>
\t\t<key>K&amp;</key>
\t\t<string>v</string>
\t</dict>
</dict>
</plist>
";
        assert_eq!(xml, expected);
    }

    #[test]
    fn dict_lookup_and_keys() {
        let dict = Dict::new()
            .with("A", Value::Integer(1))
            .with("B", "b".into());
        assert_eq!(dict.get("A"), Some(&Value::Integer(1)));
        assert_eq!(dict.get("missing"), None);
        assert_eq!(dict.keys().collect::<Vec<_>>(), ["A", "B"]);
    }
}
