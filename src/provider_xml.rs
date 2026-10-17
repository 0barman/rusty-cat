//! Bounded XML parsing shared by the direct provider control-plane responses.

use roxmltree::{Document, Node, ParsingOptions};

pub(crate) type XmlResult<T> = Result<T, &'static str>;

// Leaves room for a fully populated, pretty-printed Azure block list (50,000
// entries) while bounding allocations for adversarial tiny XML elements.
const MAX_PROVIDER_XML_NODES: u32 = 1_000_000;

pub(crate) fn parse_document(xml: &str) -> XmlResult<Document<'_>> {
    preflight_xml(xml.as_bytes())?;
    Document::parse_with_options(
        xml,
        ParsingOptions {
            allow_dtd: false,
            nodes_limit: MAX_PROVIDER_XML_NODES,
            entity_resolver: None,
        },
    )
    .map_err(|_| "provider response contains invalid or excessively complex XML")
}

// roxmltree's tokenizer recursively descends into elements, and reserves its
// initial vectors by counting raw '<' and '=' bytes, including those in text.
// Guard those resources before the parser allocates or enters recursive code.
// Ordinary provider responses use at most four element levels; 64 leaves room
// for extensions. A 50,000-entry Azure block list uses about 300,000 '<' bytes
// and 50,000 '=' bytes, comfortably below these reservation budgets.
const MAX_XML_DEPTH: usize = 64;
const MAX_XML_ATTRIBUTES_PER_ELEMENT: usize = 64;
const MAX_XML_RAW_MARKUP_TOKENS: usize = 1_000_000;
const MAX_XML_RAW_ATTRIBUTE_TOKENS: usize = 250_000;

fn preflight_xml(input: &[u8]) -> XmlResult<()> {
    if input.iter().filter(|byte| **byte == b'<').count() > MAX_XML_RAW_MARKUP_TOKENS
        || input.iter().filter(|byte| **byte == b'=').count() > MAX_XML_RAW_ATTRIBUTE_TOKENS
    {
        return Err("XML parser reservation budget exceeded");
    }

    let mut remaining = input;
    let mut depth = 0_usize;
    while let Some(open) = remaining.iter().position(|byte| *byte == b'<') {
        remaining = remaining.get(open..).ok_or("invalid XML scan offset")?;
        if remaining.starts_with(b"<!--") {
            remaining = skip_xml_markup(remaining, 4, b"-->")?;
        } else if remaining.starts_with(b"<![CDATA[") {
            remaining = skip_xml_markup(remaining, 9, b"]]>")?;
        } else if remaining.starts_with(b"<?") {
            remaining = skip_xml_markup(remaining, 2, b"?>")?;
        } else if remaining.starts_with(b"<!") {
            // No DTD/entity expansion may introduce markup after this scan.
            return Err("XML declarations other than comments and CDATA are unsupported");
        } else {
            let closing = remaining.starts_with(b"</");
            let tag_start = if closing { 2 } else { 1 };
            let tag = remaining.get(tag_start..).ok_or("missing XML tag")?;
            let (after_tag, empty) = scan_xml_tag(tag)?;
            if closing {
                depth = depth.checked_sub(1).ok_or("unmatched XML closing tag")?;
            } else {
                let element_depth = depth.checked_add(1).ok_or("XML element depth overflow")?;
                // A self-closing element also enters the tokenizer's recursive
                // element function, so count its temporary depth as well.
                if element_depth > MAX_XML_DEPTH {
                    return Err("XML element depth exceeds the parser limit");
                }
                if !empty {
                    depth = element_depth;
                }
            }
            remaining = after_tag;
        }
    }
    if depth != 0 {
        return Err("XML contains unclosed elements");
    }
    Ok(())
}

fn skip_xml_markup<'a>(
    input: &'a [u8],
    prefix_len: usize,
    terminator: &[u8],
) -> XmlResult<&'a [u8]> {
    if terminator.is_empty() {
        return Err("XML markup terminator is empty");
    }
    let contents = input.get(prefix_len..).ok_or("incomplete XML markup")?;
    let end = contents
        .windows(terminator.len())
        .position(|window| window == terminator)
        .and_then(|offset| offset.checked_add(terminator.len()))
        .ok_or("unterminated XML markup")?;
    contents.get(end..).ok_or("invalid XML markup offset")
}

/// A non-recursive resource scan, not a replacement XML syntax parser.
/// Quotes protect attribute contents; roxmltree still validates names, field
/// syntax, namespaces, entities, and matching opening/closing elements.
fn scan_xml_tag(mut remaining: &[u8]) -> XmlResult<(&[u8], bool)> {
    let mut quote = None;
    let mut previous = 0_u8;
    let mut attributes = 0_usize;
    while let Some(byte) = remaining.first().copied() {
        remaining = remaining.get(1..).ok_or("invalid XML tag offset")?;
        if let Some(delimiter) = quote {
            if byte == delimiter {
                quote = None;
            }
        } else {
            match byte {
                b'\'' | b'"' => quote = Some(byte),
                b'>' => return Ok((remaining, previous == b'/')),
                b'<' => return Err("unexpected markup inside an XML tag"),
                b'=' => {
                    attributes = attributes
                        .checked_add(1)
                        .ok_or("XML attribute count overflow")?;
                    if attributes > MAX_XML_ATTRIBUTES_PER_ELEMENT {
                        return Err("XML attributes exceed the per-element limit");
                    }
                }
                _ => {}
            }
        }
        previous = byte;
    }
    Err("unterminated XML tag or attribute")
}

pub(crate) fn document_root<'a, 'input>(
    document: &'a Document<'input>,
    expected: &str,
) -> XmlResult<Node<'a, 'input>> {
    let root = document
        .root()
        .first_element_child()
        .ok_or("XML document has no root element")?;
    if root.tag_name().name() != expected {
        return Err("XML document has an unexpected root element");
    }
    Ok(root)
}

pub(crate) fn child_elements<'a, 'input>(
    parent: Node<'a, 'input>,
    tag: &str,
    max_entries: usize,
) -> XmlResult<Vec<Node<'a, 'input>>> {
    let mut elements = Vec::new();
    for child in parent.children().filter(|child| child.is_element()) {
        // Provider fields are direct children in the parent's namespace.
        // Names appearing inside comments, text, or extension subtrees do not
        // provide or duplicate control fields.
        if child.tag_name().name() != tag
            || child.tag_name().namespace() != parent.tag_name().namespace()
        {
            continue;
        }
        if elements.len() >= max_entries {
            return Err("XML entry count exceeds the provider limit");
        }
        elements.push(child);
    }
    Ok(elements)
}

pub(crate) fn element_text(element: Node<'_, '_>) -> XmlResult<String> {
    let mut value = String::new();
    for child in element.children() {
        if child.is_element() {
            return Err("XML text field contains nested markup");
        }
        if child.is_text() {
            if let Some(text) = child.text() {
                value.push_str(text);
            }
        }
    }
    Ok(value.trim().to_owned())
}

pub(crate) fn exactly_one_text(parent: Node<'_, '_>, tag: &str) -> XmlResult<String> {
    let elements = child_elements(parent, tag, 2)?;
    match elements.as_slice() {
        [element] => element_text(*element),
        [] => Err("XML critical field is missing"),
        _ => Err("XML critical field is duplicated"),
    }
}

pub(crate) fn exactly_one_nonempty_text(parent: Node<'_, '_>, tag: &str) -> XmlResult<String> {
    let value = exactly_one_text(parent, tag)?;
    if value.is_empty() {
        return Err("XML critical field is empty");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::{parse_document, MAX_PROVIDER_XML_NODES};

    #[test]
    fn xml_structure_node_limit_rejects_large_collections_without_poisoning_parser(
    ) -> Result<(), &'static str> {
        let xml = format!(
            "<Root>{}</Root>",
            "<X/>text".repeat(MAX_PROVIDER_XML_NODES as usize / 2 + 1)
        );
        if parse_document(&xml).is_ok() {
            return Err("XML node allocation limit was not enforced");
        }
        parse_document("<Root><Value>healthy</Value></Root>")?;
        Ok(())
    }
}

#[cfg(test)]
mod preflight_regression_tests {
    use super::parse_document;

    #[test]
    fn xml_depth_preflight_rejects_excessive_nesting() -> Result<(), &'static str> {
        let xml = format!("{}leaf{}", "<X>".repeat(65), "</X>".repeat(65));
        if parse_document(&xml).is_ok() {
            return Err("XML nesting exceeded the safe parser depth");
        }
        let empty_leaf = format!("{}<Leaf/>{}", "<X>".repeat(64), "</X>".repeat(64));
        if parse_document(&empty_leaf).is_ok() {
            return Err("a self-closing leaf bypassed the XML nesting limit");
        }
        Ok(())
    }

    #[test]
    fn xml_depth_preflight_bounds_attributes_before_quadratic_duplicate_checks(
    ) -> Result<(), &'static str> {
        let attributes = (0..65)
            .map(|index| format!(" a{index}='value'"))
            .collect::<String>();
        if parse_document(&format!("<Root{attributes}/>")).is_ok() {
            return Err("per-element XML attribute limit was not enforced");
        }
        Ok(())
    }

    #[test]
    fn xml_depth_preflight_bounds_raw_parser_reservations_even_inside_text(
    ) -> Result<(), &'static str> {
        let eq = format!("<Root><![CDATA[{}]]></Root>", "=".repeat(250_001));
        let lt = format!("<Root><![CDATA[{}]]></Root>", "<".repeat(1_000_001));
        for xml in [eq, lt] {
            if parse_document(&xml).is_ok() {
                return Err("raw XML reservation token limit was not enforced");
            }
        }
        Ok(())
    }

    #[test]
    fn xml_depth_preflight_accepts_bounded_nesting_attributes_and_special_markup(
    ) -> Result<(), &'static str> {
        let attributes = (0..64)
            .map(|index| format!(" a{index}='=&gt;/>'"))
            .collect::<String>();
        let xml = format!(
            "{}<Leaf{attributes}/>{}",
            "<X>".repeat(63),
            "</X>".repeat(63)
        );
        parse_document(&xml)?;
        let fake_nesting = "<X>".repeat(100);
        let xml = format!("<?probe depth='<X><Y>'?><Root quote='> / ='><![CDATA[{fake_nesting}]]><!-- {fake_nesting} --><Leaf quote=\"= />\"/></Root>");
        parse_document(&xml)?;
        Ok(())
    }

    #[test]
    fn xml_depth_preflight_rejects_empty_dtd_without_rejecting_dtd_text() -> Result<(), &'static str>
    {
        if parse_document("<!DOCTYPE Root><Root/>").is_ok() {
            return Err("DOCTYPE bypassed XML preflight");
        }
        parse_document("<Root><![CDATA[<!DOCTYPE Root>]]><!-- <!DOCTYPE Root> --></Root>")?;
        Ok(())
    }

    #[test]
    fn xml_depth_preflight_accepts_exact_raw_reservation_budgets() -> Result<(), &'static str> {
        let xml = format!(
            "<Root><![CDATA[{}{}]]></Root>",
            "<".repeat(super::MAX_XML_RAW_MARKUP_TOKENS - 3),
            "=".repeat(super::MAX_XML_RAW_ATTRIBUTE_TOKENS),
        );
        super::preflight_xml(xml.as_bytes())?;
        Ok(())
    }

    #[test]
    fn xml_depth_preflight_rejects_incomplete_markup_without_panicking() -> Result<(), &'static str>
    {
        for xml in [
            "<Root><!-- comment",
            "<Root><![CDATA[unfinished",
            "<?unfinished",
            "<Root attribute='unfinished> <Nested/>",
            "<Root <Nested/>>",
            "</Root>",
        ] {
            if super::preflight_xml(xml.as_bytes()).is_ok() {
                return Err("incomplete markup passed XML resource preflight");
            }
        }
        if super::skip_xml_markup(b"text", 0, b"").is_ok() {
            return Err("empty markup terminator was accepted");
        }
        Ok(())
    }
}
