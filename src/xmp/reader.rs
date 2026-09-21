use super::{
    checked_rating, sidecar_path, writer::snapshot, XmpError, MAX_SIDECAR_BYTES, RDF_NS, XMP_NS,
};
use quick_xml::{
    events::{BytesStart, Event},
    Reader,
};
use std::{collections::HashSet, ops::Range, path::Path};

const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

pub fn read_rating(raw_path: &Path) -> Result<Option<i8>, XmpError> {
    let Some(source) = snapshot(&sidecar_path(raw_path))? else {
        return Ok(None);
    };
    Ok(parse(&source.bytes)?.rating.map(|field| field.value))
}

pub(super) struct RatingField {
    pub value: i8,
    pub span: Range<usize>,
    /// Entire attribute or element; removing a rating must not leave an empty
    /// property, which is invalid in Adobe's integer-valued representation.
    pub property_span: Range<usize>,
}
pub(super) struct Document {
    pub rating: Option<RatingField>,
    pub insertion: usize,
    pub prefix: String,
}
struct Frame {
    start: usize,
    name: String,
    namespace: String,
    local: String,
    declarations: Vec<(String, String)>,
    description: bool,
    rating: bool,
}
struct Attribute {
    name: String,
    value: String,
    span: Range<usize>,
    property_span: Range<usize>,
}

#[derive(Clone)]
struct AttributeSpan {
    value: Range<usize>,
    property: Range<usize>,
}

fn invalid(reason: &'static str) -> XmpError {
    XmpError::Invalid(reason)
}
fn xml_error(error: impl std::fmt::Display) -> XmpError {
    XmpError::Xml(error.to_string())
}

fn name_start(c: char) -> bool {
    matches!(c, 'A'..='Z' | 'a'..='z' | '_' | '\u{c0}'..='\u{d6}' |
        '\u{d8}'..='\u{f6}' | '\u{f8}'..='\u{2ff}' | '\u{370}'..='\u{37d}' |
        '\u{37f}'..='\u{1fff}' | '\u{200c}'..='\u{200d}' | '\u{2070}'..='\u{218f}' |
        '\u{2c00}'..='\u{2fef}' | '\u{3001}'..='\u{d7ff}' | '\u{f900}'..='\u{fdcf}' |
        '\u{fdf0}'..='\u{fffd}' | '\u{10000}'..='\u{effff}')
}
fn valid_name(name: &str) -> bool {
    let pieces: Vec<_> = name.split(':').collect();
    pieces.len() <= 2 && pieces.iter().all(|piece| {
        let mut chars = piece.chars();
        chars.next().is_some_and(name_start) && chars.all(|c| name_start(c) ||
            matches!(c, '0'..='9' | '-' | '.' | '\u{b7}' | '\u{300}'..='\u{36f}' | '\u{203f}'..='\u{2040}'))
    })
}
fn valid_xml_text(text: &str) -> bool {
    text.chars().all(|c| {
        matches!(c, '\t' | '\r' | '\n' | '\u{20}'..='\u{d7ff}' |
        '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
    })
}
fn whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

/// Locate only quoted attribute values after quick-xml validates their syntax.
/// Ranges are offsets into the original UTF-8 document, not decoded strings.
fn attribute_spans(bytes: &[u8], start: usize, end: usize) -> Result<Vec<AttributeSpan>, XmpError> {
    let mut pos = start + 1;
    while pos < end && !whitespace(bytes[pos]) && !matches!(bytes[pos], b'>' | b'/') {
        pos += 1;
    }
    let mut result = Vec::new();
    loop {
        while pos < end && whitespace(bytes[pos]) {
            pos += 1;
        }
        if pos >= end || matches!(bytes[pos], b'>' | b'/') {
            return Ok(result);
        }
        let property_start = pos;
        while pos < end && !whitespace(bytes[pos]) && bytes[pos] != b'=' {
            pos += 1;
        }
        while pos < end && whitespace(bytes[pos]) {
            pos += 1;
        }
        if bytes.get(pos) != Some(&b'=') {
            return Err(invalid("invalid attribute assignment"));
        }
        pos += 1;
        while pos < end && whitespace(bytes[pos]) {
            pos += 1;
        }
        let quote = *bytes
            .get(pos)
            .ok_or_else(|| invalid("unterminated attribute"))?;
        if !matches!(quote, b'\'' | b'"') {
            return Err(invalid("attribute value must be quoted"));
        }
        pos += 1;
        let begin = pos;
        while pos < end && bytes[pos] != quote {
            if bytes[pos] == b'<' {
                return Err(invalid("unescaped less-than in attribute"));
            }
            pos += 1;
        }
        if pos >= end {
            return Err(invalid("unterminated attribute value"));
        }
        result.push(AttributeSpan {
            value: begin..pos,
            property: property_start..pos + 1,
        });
        pos += 1;
        if pos < end && !whitespace(bytes[pos]) && !matches!(bytes[pos], b'>' | b'/') {
            return Err(invalid("missing whitespace between attributes"));
        }
    }
}

fn namespace(prefix: &str, frames: &[Frame], local: &[(String, String)]) -> Option<String> {
    if prefix == "xml" {
        return Some(XML_NS.to_owned());
    }
    local
        .iter()
        .rev()
        .chain(
            frames
                .iter()
                .rev()
                .flat_map(|f| f.declarations.iter().rev()),
        )
        .find(|(key, _)| key == prefix)
        .map(|(_, uri)| uri.clone())
}
fn expanded(
    name: &str,
    attribute: bool,
    frames: &[Frame],
    local: &[(String, String)],
) -> Result<(String, String), XmpError> {
    if !valid_name(name) {
        return Err(invalid("invalid XML qualified name"));
    }
    match name.split_once(':') {
        Some((prefix, value)) => {
            let uri = namespace(prefix, frames, local)
                .filter(|u| !u.is_empty())
                .ok_or_else(|| invalid("unbound namespace prefix"))?;
            Ok((uri, value.to_owned()))
        }
        None => Ok((
            if attribute {
                String::new()
            } else {
                namespace("", frames, local).unwrap_or_default()
            },
            name.to_owned(),
        )),
    }
}

fn field(
    value: &str,
    span: Range<usize>,
    property_span: Range<usize>,
    bytes: &[u8],
) -> Result<RatingField, XmpError> {
    let value = value.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r'));
    // Avoid accepting floating point, empty strings, or values outside Adobe's range.
    if !matches!(value, "-1" | "0" | "1" | "2" | "3" | "4" | "5") {
        return Err(invalid("unsupported or invalid rating value"));
    }
    let raw = &bytes[span.clone()];
    let lead = raw.iter().take_while(|b| whitespace(**b)).count();
    let tail = raw.iter().rev().take_while(|b| whitespace(**b)).count();
    Ok(RatingField {
        value: checked_rating(value.parse().map_err(xml_error)?)?,
        span: span.start + lead..span.end - tail,
        property_span,
    })
}

pub(super) fn parse(bytes: &[u8]) -> Result<Document, XmpError> {
    if bytes.len() as u64 > MAX_SIDECAR_BYTES {
        return Err(invalid("sidecar exceeds 8 MiB"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("only UTF-8 XMP is supported"))?;
    if !valid_xml_text(text) {
        return Err(invalid("invalid XML character"));
    }
    // Keep the original UTF-8 BOM byte positions explicit: quick-xml's source
    // reader skips a BOM internally and excludes it from buffer_position().
    let bom_length = if text.starts_with('\u{feff}') { 3 } else { 0 };
    let mut reader = Reader::from_str(&text[bom_length..]);
    reader.config_mut().check_comments = true;
    let mut frames: Vec<Frame> = Vec::new();
    let mut root_count = 0usize;
    let mut rdf_count = 0usize;
    let mut total_attributes = 0usize;
    let mut rating = None;
    let mut rating_text: Option<(String, Range<usize>)> = None;
    let mut insertion: Option<(usize, String)> = None;
    let mut declaration_seen = false;
    let mut prolog_content_seen = false;
    loop {
        let event = reader.read_event().map_err(xml_error)?;
        let end = reader.buffer_position() as usize + bom_length;
        match event {
            Event::Start(ref node) | Event::Empty(ref node) => {
                if frames.last().is_some_and(|f| f.rating) {
                    return Err(invalid("rating contains nested markup"));
                }
                let empty = matches!(event, Event::Empty(_));
                let start = end
                    .checked_sub(node.as_ref().len() + if empty { 3 } else { 2 })
                    .ok_or_else(|| invalid("invalid XML byte range"))?;
                if bytes.get(start) != Some(&b'<') {
                    return Err(invalid("invalid XML start tag"));
                }
                if frames.is_empty() {
                    root_count += 1;
                    if root_count > 1 {
                        return Err(invalid("multiple XML roots"));
                    }
                }
                if frames.len() >= 128 {
                    return Err(invalid("XML nesting limit exceeded"));
                }
                let spans = attribute_spans(bytes, start, end)?;
                let mut attributes = Vec::new();
                let mut declarations = Vec::new();
                for (index, attr) in node.attributes().enumerate() {
                    let attr = attr.map_err(xml_error)?;
                    total_attributes += 1;
                    if total_attributes > 65_536 {
                        return Err(invalid("XML attribute limit exceeded"));
                    }
                    let name = std::str::from_utf8(attr.key.as_ref())
                        .map_err(xml_error)?
                        .to_owned();
                    if name.len() > 1024 || !valid_name(&name) {
                        return Err(invalid("invalid attribute name"));
                    }
                    let value = attr
                        .decode_and_unescape_value(reader.decoder())
                        .map_err(xml_error)?
                        .into_owned();
                    if !valid_xml_text(&value) {
                        return Err(invalid("invalid attribute character"));
                    }
                    let span = spans
                        .get(index)
                        .ok_or_else(|| invalid("attribute span mismatch"))?
                        .clone();
                    if name == "xmlns" || name.starts_with("xmlns:") {
                        if value.len() > 1024 {
                            return Err(invalid("namespace URI exceeds safety limit"));
                        }
                        let prefix = name.strip_prefix("xmlns:").unwrap_or("");
                        if prefix == "xmlns"
                            || value == XMLNS_NS
                            || (prefix == "xml") != (value == XML_NS)
                            || (!prefix.is_empty() && value.is_empty())
                        {
                            return Err(invalid("invalid namespace declaration"));
                        }
                        declarations.push((prefix.to_owned(), value));
                    } else {
                        attributes.push(Attribute {
                            name,
                            value,
                            span: span.value,
                            property_span: span.property,
                        });
                    }
                }
                let name = std::str::from_utf8(node.name().as_ref())
                    .map_err(xml_error)?
                    .to_owned();
                let (uri, local) = expanded(&name, false, &frames, &declarations)?;
                let is_rdf = uri == RDF_NS && local == "RDF";
                if is_rdf {
                    rdf_count += 1;
                    if rdf_count > 1 {
                        return Err(invalid("multiple RDF documents are ambiguous"));
                    }
                }
                let is_description = uri == RDF_NS
                    && local == "Description"
                    && frames
                        .last()
                        .is_some_and(|f| f.namespace == RDF_NS && f.local == "RDF");
                let is_rating = uri == XMP_NS
                    && local == "Rating"
                    && frames.last().is_some_and(|f| f.description);
                let mut subject_ok = true;
                let mut seen = HashSet::new();
                for attr in &attributes {
                    let key = expanded(&attr.name, true, &frames, &declarations)?;
                    if !seen.insert(key.clone()) {
                        return Err(invalid("duplicate expanded attribute name"));
                    }
                    if is_description
                        && key.0 == RDF_NS
                        && key.1 == "about"
                        && !attr.value.is_empty()
                    {
                        subject_ok = false;
                    }
                    if is_description
                        && key.0 == RDF_NS
                        && matches!(key.1.as_str(), "nodeID" | "ID")
                    {
                        subject_ok = false;
                    }
                    if is_description && key.0 == XMP_NS && key.1 == "Rating" {
                        if rating.is_some() {
                            return Err(invalid("duplicate rating properties"));
                        }
                        rating = Some(field(
                            &attr.value,
                            attr.span.clone(),
                            attr.property_span.clone(),
                            bytes,
                        )?);
                    }
                }
                if is_description && !subject_ok {
                    return Err(invalid(
                        "nonempty RDF subject is ambiguous for photo ratings",
                    ));
                }
                if is_rating {
                    if rating.is_some() {
                        return Err(invalid("duplicate rating properties"));
                    }
                    if empty || !attributes.is_empty() {
                        return Err(invalid("unsupported rating element representation"));
                    }
                    rating_text = None;
                }
                if is_description && insertion.is_none() {
                    let mut prefix = "fastcullXmp".to_owned();
                    let mut suffix = 0;
                    while namespace(&prefix, &frames, &declarations).is_some() {
                        suffix += 1;
                        prefix = format!("fastcullXmp{suffix}");
                    }
                    insertion = Some((end - if empty { 2 } else { 1 }, prefix));
                }
                if !empty {
                    frames.push(Frame {
                        start,
                        name,
                        namespace: uri,
                        local,
                        declarations,
                        description: is_description,
                        rating: is_rating,
                    });
                }
            }
            Event::End(node) => {
                let frame = frames
                    .pop()
                    .ok_or_else(|| invalid("unexpected closing tag"))?;
                if frame.name.as_bytes() != node.name().as_ref() {
                    return Err(invalid("mismatched closing tag"));
                }
                if frame.rating {
                    let (value, span) = rating_text
                        .take()
                        .ok_or_else(|| invalid("rating has no value"))?;
                    rating = Some(field(&value, span, frame.start..end, bytes)?);
                }
            }
            Event::Text(node) => {
                if node.as_ref().windows(3).any(|part| part == b"]]>") {
                    return Err(invalid("CDATA terminator outside CDATA"));
                }
                let value = node.unescape().map_err(xml_error)?.into_owned();
                if !valid_xml_text(&value) {
                    return Err(invalid("invalid XML text character"));
                }
                if frames.is_empty() {
                    prolog_content_seen = true;
                    if !value.bytes().all(whitespace) {
                        return Err(invalid("text outside XML root"));
                    }
                }
                if frames.last().is_some_and(|f| f.rating) {
                    if rating_text.is_some() {
                        return Err(invalid("rating contains mixed content"));
                    }
                    rating_text = Some((value, end - node.as_ref().len()..end));
                }
            }
            Event::CData(node) => {
                if frames.is_empty() {
                    return Err(invalid("CDATA outside XML root"));
                }
                if frames.last().is_some_and(|f| f.rating) {
                    if rating_text.is_some() {
                        return Err(invalid("rating contains mixed content"));
                    }
                    let value = std::str::from_utf8(node.as_ref())
                        .map_err(xml_error)?
                        .to_owned();
                    rating_text = Some((value, end - 3 - node.as_ref().len()..end - 3));
                }
            }
            Event::Decl(node) => {
                if declaration_seen || prolog_content_seen || root_count != 0 || !frames.is_empty()
                {
                    return Err(invalid("misplaced XML declaration"));
                }
                declaration_seen = true;
                // quick-xml's declaration convenience getters do not validate
                // duplicate fields or grammar order, so check those explicitly.
                if node.len() > 4096 {
                    return Err(invalid("XML declaration exceeds safety limit"));
                }
                let content = std::str::from_utf8(&node).map_err(xml_error)?;
                let declaration = BytesStart::from_content(content, 3);
                let mut order = 0;
                for attr in declaration.attributes() {
                    let attr = attr.map_err(xml_error)?;
                    let next = match attr.key.as_ref() {
                        b"version" if attr.value.as_ref() == b"1.0" => 1,
                        b"encoding" if attr.value.eq_ignore_ascii_case(b"UTF-8") => 2,
                        b"standalone" if matches!(attr.value.as_ref(), b"yes" | b"no") => 3,
                        _ => return Err(invalid("unsupported or malformed XML declaration")),
                    };
                    if next <= order || (order == 0 && next != 1) {
                        return Err(invalid("invalid XML declaration field order"));
                    }
                    order = next;
                }
                if order == 0 {
                    return Err(invalid("XML declaration has no version"));
                }
            }
            Event::DocType(_) => return Err(invalid("DTD sidecars are unsupported")),
            Event::PI(node) => {
                let target = std::str::from_utf8(node.target()).map_err(xml_error)?;
                if target.eq_ignore_ascii_case("xml") || !valid_name(target) {
                    return Err(invalid("invalid XML processing-instruction target"));
                }
                if root_count == 0 {
                    prolog_content_seen = true;
                }
                if frames.last().is_some_and(|f| f.rating) {
                    return Err(invalid("rating contains mixed markup"));
                }
            }
            Event::Comment(_) => {
                if root_count == 0 {
                    prolog_content_seen = true;
                }
                if frames.last().is_some_and(|f| f.rating) {
                    return Err(invalid("rating contains mixed markup"));
                }
            }
            Event::Eof => break,
        }
    }
    if root_count != 1 || !frames.is_empty() || rdf_count != 1 {
        return Err(invalid("incomplete XML/RDF document"));
    }
    let (insertion, prefix) = insertion.ok_or_else(|| invalid("no unambiguous RDF Description"))?;
    Ok(Document {
        rating,
        insertion,
        prefix,
    })
}
