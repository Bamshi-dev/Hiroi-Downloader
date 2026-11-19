//! A small XML tree for the formats we read whole: DASH manifests and
//! Metalink files. Both are a few kilobytes, so building a tree is
//! simpler than reacting to a stream of events, and costs little.
//!
//! quick-xml does not expand entities for us; the predefined ones and
//! character references are enough here, and no DTD is ever read.

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

/// An element with its attributes, children, and text. Names are local
/// names: namespace prefixes are dropped.
#[derive(Debug, Clone, Default)]
pub(crate) struct Element {
    pub name: String,
    pub attributes: Vec<(String, String)>,
    pub children: Vec<Element>,
    pub text: String,
}

impl Element {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn children<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |child| child.name == name)
    }

    pub fn child<'a>(&'a self, name: &'a str) -> Option<&'a Element> {
        self.children(name).next()
    }
}

pub(crate) fn parse(text: &str) -> Result<Element, String> {
    let mut reader = Reader::from_str(text);
    let mut stack: Vec<Element> = vec![Element::default()];
    loop {
        match reader.read_event().map_err(|e| format!("bad XML: {e}"))? {
            Event::Start(start) => stack.push(element(&start)?),
            Event::Empty(start) => {
                let element = element(&start)?;
                stack.last_mut().expect("root").children.push(element);
            }
            Event::End(_) => {
                let done = stack.pop().ok_or("unbalanced elements")?;
                stack
                    .last_mut()
                    .ok_or("unbalanced elements")?
                    .children
                    .push(done);
            }
            Event::Text(text) => {
                if let Some(current) = stack.last_mut() {
                    current.text.push_str(&text.html_content());
                }
            }
            // `&amp;` and friends arrive as their own events.
            Event::GeneralRef(reference) => {
                if let Some(current) = stack.last_mut() {
                    let name: &str = reference.as_ref();
                    current.text.push_str(entity(name).as_deref().unwrap_or(""));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    match stack.pop() {
        Some(document) if stack.is_empty() => Ok(document),
        _ => Err("unclosed elements".into()),
    }
}

/// The predefined XML entities and character references.
fn entity(name: &str) -> Option<String> {
    let text = match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        _ => {
            let code = match name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => name.strip_prefix('#')?.parse().ok()?,
            };
            return char::from_u32(code).map(String::from);
        }
    };
    Some(text.to_string())
}

fn element(start: &BytesStart<'_>) -> Result<Element, String> {
    let name = start.local_name().as_ref().to_string();
    let mut attributes = Vec::new();
    for attribute in start.attributes() {
        let attribute = attribute.map_err(|e| e.to_string())?;
        let key = attribute.key.local_name().as_ref().to_string();
        let value = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|e| e.to_string())?
            .into_owned();
        attributes.push((key, value));
    }
    Ok(Element {
        name,
        attributes,
        ..Element::default()
    })
}
