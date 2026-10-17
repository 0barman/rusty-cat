use std::collections::BTreeSet;

use roxmltree::Node;

use crate::provider_xml::{
    child_elements, document_root, element_text, exactly_one_nonempty_text, exactly_one_text,
    parse_document, XmlResult,
};

pub(crate) const MAX_OSS_LIST_ENTRIES: usize = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OssPart {
    pub(crate) part_number: u64,
    pub(crate) size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OssUploadPage {
    pub(crate) upload_ids_for_key: Vec<String>,
    pub(crate) next_markers: Option<(String, String)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OssPartPage {
    pub(crate) parts: Vec<OssPart>,
    pub(crate) next_part_number_marker: Option<u64>,
}

fn listing_is_truncated(root: Node<'_, '_>) -> XmlResult<bool> {
    let elements = child_elements(root, "IsTruncated", 2)?;
    match elements.as_slice() {
        [] => Ok(false),
        [element] => match element_text(*element)?.to_ascii_lowercase().as_str() {
            "false" => Ok(false),
            "true" => Ok(true),
            _ => Err("invalid IsTruncated value"),
        },
        _ => Err("duplicate IsTruncated field"),
    }
}

fn listing_uses_url_encoding(root: Node<'_, '_>) -> XmlResult<bool> {
    let elements = child_elements(root, "EncodingType", 2)?;
    match elements.as_slice() {
        [] => Ok(false),
        [element] if element_text(*element)?.eq_ignore_ascii_case("url") => Ok(true),
        [_] => Err("unsupported EncodingType value"),
        _ => Err("duplicate EncodingType field"),
    }
}

fn decode_url_encoded_text(value: &str) -> XmlResult<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err("truncated URL escape in OSS XML");
            }
            let high = decode_hex(bytes[index + 1]).ok_or("invalid URL escape in OSS XML")?;
            let low = decode_hex(bytes[index + 2]).ok_or("invalid URL escape in OSS XML")?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            // EncodingType=url follows RFC 3986; `+` is a literal plus here.
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| "URL-encoded OSS XML field is not valid UTF-8")
}

fn decode_hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

/// Extracts exactly one non-empty critical XML field.
///
/// The legacy return type is retained because initiate maps both missing and
/// ambiguous fields to the same public response-parse error.
pub(crate) fn extract_xml_tag(body: &str, tag: &str) -> Option<String> {
    let document = parse_document(body).ok()?;
    let root = document_root(&document, "InitiateMultipartUploadResult").ok()?;
    exactly_one_nonempty_text(root, tag).ok()
}

pub(crate) fn parse_upload_page_for_key(xml: &str, key: &str) -> XmlResult<OssUploadPage> {
    let document = parse_document(xml)?;
    let root = document_root(&document, "ListMultipartUploadsResult")?;
    let url_encoded = listing_uses_url_encoding(root)?;
    let upload_blocks = child_elements(root, "Upload", MAX_OSS_LIST_ENTRIES)?;
    let mut ids = Vec::new();
    let mut seen = BTreeSet::new();
    for block in upload_blocks {
        let item_key = exactly_one_nonempty_text(block, "Key")?;
        let item_key = if url_encoded {
            decode_url_encoded_text(&item_key)?
        } else {
            item_key
        };
        let upload_id = exactly_one_nonempty_text(block, "UploadId")?;
        if !seen.insert((item_key.clone(), upload_id.clone())) {
            return Err("duplicate multipart upload entry");
        }
        if item_key == key {
            ids.push(upload_id);
        }
    }
    let next_markers = if listing_is_truncated(root)? {
        let key_marker = exactly_one_nonempty_text(root, "NextKeyMarker")?;
        let key_marker = if url_encoded {
            decode_url_encoded_text(&key_marker)?
        } else {
            key_marker
        };
        let upload_id_marker = exactly_one_text(root, "NextUploadIdMarker")?;
        Some((key_marker, upload_id_marker))
    } else {
        None
    };
    Ok(OssUploadPage {
        upload_ids_for_key: ids,
        next_markers,
    })
}

#[cfg(test)]
fn parse_upload_ids_for_key(xml: &str, key: &str) -> XmlResult<Vec<String>> {
    parse_upload_page_for_key(xml, key).map(|page| page.upload_ids_for_key)
}

pub(crate) fn parse_part_page(xml: &str) -> XmlResult<OssPartPage> {
    let document = parse_document(xml)?;
    let root = document_root(&document, "ListPartsResult")?;
    let part_blocks = child_elements(root, "Part", MAX_OSS_LIST_ENTRIES)?;
    let mut parts = Vec::with_capacity(part_blocks.len());
    let mut seen = BTreeSet::new();
    for block in part_blocks {
        let part_number = exactly_one_nonempty_text(block, "PartNumber")?
            .parse::<u64>()
            .map_err(|_| "invalid OSS PartNumber")?;
        let size = exactly_one_nonempty_text(block, "Size")?
            .parse::<u64>()
            .map_err(|_| "invalid OSS part Size")?;
        if part_number == 0 || size == 0 {
            return Err("OSS part number and size must be non-zero");
        }
        if !seen.insert(part_number) {
            return Err("duplicate OSS PartNumber");
        }
        parts.push(OssPart { part_number, size });
    }
    parts.sort_unstable_by_key(|part| part.part_number);
    let next_part_number_marker = if listing_is_truncated(root)? {
        let marker = exactly_one_nonempty_text(root, "NextPartNumberMarker")?
            .parse::<u64>()
            .map_err(|_| "invalid OSS NextPartNumberMarker")?;
        if marker == 0 {
            return Err("OSS NextPartNumberMarker must be non-zero");
        }
        Some(marker)
    } else {
        None
    };
    Ok(OssPartPage {
        parts,
        next_part_number_marker,
    })
}

#[cfg(test)]
fn parse_part_entries(xml: &str) -> XmlResult<Vec<OssPart>> {
    parse_part_page(xml).map(|page| page.parts)
}

#[cfg(test)]
fn extract_upload_ids_for_key(xml: &str, key: &str) -> Vec<String> {
    parse_upload_ids_for_key(xml, key).unwrap_or_default()
}

#[cfg(test)]
fn extract_part_numbers(xml: &str) -> Vec<u64> {
    parse_part_entries(xml)
        .unwrap_or_default()
        .into_iter()
        .map(|part| part.part_number)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        extract_part_numbers, extract_upload_ids_for_key, extract_xml_tag, parse_part_entries,
        parse_part_page, parse_upload_ids_for_key, parse_upload_page_for_key, MAX_OSS_LIST_ENTRIES,
    };
    use proptest::prelude::*;

    #[test]
    fn test_extract_xml_tag_trims_value() {
        let xml = "<InitiateMultipartUploadResult><UploadId>  abc-123  </UploadId></InitiateMultipartUploadResult>";
        assert_eq!(extract_xml_tag(xml, "UploadId").as_deref(), Some("abc-123"));
    }

    #[test]
    fn duplicate_critical_tag_is_ambiguous() {
        let xml = "<InitiateMultipartUploadResult><UploadId>first</UploadId><UploadId>second</UploadId></InitiateMultipartUploadResult>";
        assert_eq!(
            extract_xml_tag(xml, "UploadId"),
            None,
            "a duplicate critical field must not be accepted by taking the first value"
        );
    }

    #[test]
    fn test_extract_upload_ids_for_key_filters_target_key() {
        let xml = "<ListMultipartUploadsResult><Upload><Key>target/a.bin</Key><UploadId>id-1</UploadId></Upload><Upload><Key>other.bin</Key><UploadId>id-other</UploadId></Upload></ListMultipartUploadsResult>";
        assert_eq!(
            extract_upload_ids_for_key(xml, "target/a.bin"),
            vec!["id-1".to_string()]
        );
    }

    #[test]
    fn unknown_upload_fields_are_ignored() {
        let xml = "<ListMultipartUploadsResult><Upload><Unknown>x</Unknown><Key>target/a.bin</Key><UploadId>id-1</UploadId></Upload></ListMultipartUploadsResult>";
        assert_eq!(
            parse_upload_ids_for_key(xml, "target/a.bin").unwrap(),
            vec!["id-1".to_string()]
        );
    }

    #[test]
    fn upload_listing_decodes_url_encoded_keys_and_markers_exactly_once() {
        let xml = "<ListMultipartUploadsResult>\
                   <EncodingType>url</EncodingType>\
                   <Upload><Key>target%2F%E4%B8%AD%E6%96%87%20a%2Fb%26c%252Fd%2Be.bin</Key>\
                   <UploadId>id-encoded</UploadId></Upload>\
                   <IsTruncated>true</IsTruncated>\
                   <NextKeyMarker>target%2F%E4%B8%AD%E6%96%87%20a%2Fb%26c%252Fd%2Be.bin</NextKeyMarker>\
                   <NextUploadIdMarker>id-encoded</NextUploadIdMarker>\
                   </ListMultipartUploadsResult>";
        let page = parse_upload_page_for_key(xml, "target/中文 a/b&c%2Fd+e.bin").unwrap();
        assert_eq!(page.upload_ids_for_key, vec!["id-encoded"]);
        assert_eq!(
            page.next_markers,
            Some((
                "target/中文 a/b&c%2Fd+e.bin".to_owned(),
                "id-encoded".to_owned()
            ))
        );
    }

    #[test]
    fn upload_entry_requires_exactly_one_key_and_upload_id() {
        for xml in [
            "<ListMultipartUploadsResult><Upload><UploadId>id-1</UploadId></Upload></ListMultipartUploadsResult>",
            "<ListMultipartUploadsResult><Upload><Key>target/a.bin</Key></Upload></ListMultipartUploadsResult>",
            "<ListMultipartUploadsResult><Upload><Key>target/a.bin</Key><Key>target/a.bin</Key><UploadId>id-1</UploadId></Upload></ListMultipartUploadsResult>",
            "<ListMultipartUploadsResult><Upload><Key>target/a.bin</Key><UploadId>id-1</UploadId><UploadId>id-2</UploadId></Upload></ListMultipartUploadsResult>",
        ] {
            assert!(parse_upload_ids_for_key(xml, "target/a.bin").is_err());
        }
    }

    #[test]
    fn individually_valid_truncated_pages_remain_parseable_for_bounded_pagination() {
        let uploads = "<ListMultipartUploadsResult>\
                       <Upload><Key>target/a.bin</Key><UploadId>id-1</UploadId></Upload>\
                       <IsTruncated>true</IsTruncated>\
                       <NextKeyMarker>target/a.bin</NextKeyMarker>\
                       <NextUploadIdMarker>id-1</NextUploadIdMarker>\
                       </ListMultipartUploadsResult>";
        assert_eq!(
            parse_upload_ids_for_key(uploads, "target/a.bin").unwrap(),
            vec!["id-1".to_owned()]
        );

        let parts = "<ListPartsResult>\
                     <Part><PartNumber>1</PartNumber><Size>4</Size></Part>\
                     <IsTruncated>true</IsTruncated>\
                     <NextPartNumberMarker>1</NextPartNumberMarker>\
                     </ListPartsResult>";
        assert_eq!(
            parse_part_entries(parts).unwrap(),
            vec![super::OssPart {
                part_number: 1,
                size: 4,
            }]
        );
    }

    #[test]
    fn truncated_upload_page_requires_exactly_one_marker_pair() {
        for xml in [
            "<ListMultipartUploadsResult><IsTruncated>true</IsTruncated><NextUploadIdMarker>id-1</NextUploadIdMarker></ListMultipartUploadsResult>",
            "<ListMultipartUploadsResult><IsTruncated>true</IsTruncated><NextKeyMarker>target/a.bin</NextKeyMarker></ListMultipartUploadsResult>",
            "<ListMultipartUploadsResult><IsTruncated>true</IsTruncated><NextKeyMarker>target/a.bin</NextKeyMarker><NextKeyMarker>target/a.bin</NextKeyMarker><NextUploadIdMarker>id-1</NextUploadIdMarker></ListMultipartUploadsResult>",
            "<ListMultipartUploadsResult><IsTruncated>true</IsTruncated><NextKeyMarker>target/a.bin</NextKeyMarker><NextUploadIdMarker>id-1</NextUploadIdMarker><NextUploadIdMarker>id-2</NextUploadIdMarker></ListMultipartUploadsResult>",
        ] {
            assert!(parse_upload_page_for_key(xml, "target/a.bin").is_err());
        }
    }

    #[test]
    fn truncated_part_page_requires_exactly_one_positive_numeric_marker() {
        for xml in [
            "<ListPartsResult><Part><PartNumber>1</PartNumber><Size>4</Size></Part><IsTruncated>true</IsTruncated></ListPartsResult>",
            "<ListPartsResult><Part><PartNumber>1</PartNumber><Size>4</Size></Part><IsTruncated>true</IsTruncated><NextPartNumberMarker>1</NextPartNumberMarker><NextPartNumberMarker>2</NextPartNumberMarker></ListPartsResult>",
            "<ListPartsResult><Part><PartNumber>1</PartNumber><Size>4</Size></Part><IsTruncated>true</IsTruncated><NextPartNumberMarker>0</NextPartNumberMarker></ListPartsResult>",
            "<ListPartsResult><Part><PartNumber>1</PartNumber><Size>4</Size></Part><IsTruncated>true</IsTruncated><NextPartNumberMarker>not-a-number</NextPartNumberMarker></ListPartsResult>",
        ] {
            assert!(parse_part_page(xml).is_err());
        }
    }

    #[test]
    fn upload_entry_limit_is_bounded() {
        let mut xml = String::from("<ListMultipartUploadsResult>");
        for number in 1..=MAX_OSS_LIST_ENTRIES + 1 {
            xml.push_str(&format!(
                "<Upload><Key>target/a.bin</Key><UploadId>id-{number}</UploadId></Upload>"
            ));
        }
        xml.push_str("</ListMultipartUploadsResult>");
        assert!(parse_upload_page_for_key(&xml, "target/a.bin").is_err());
    }

    #[test]
    fn test_extract_part_numbers_sorts_valid_entries() {
        let xml = "<ListPartsResult><Part><PartNumber>3</PartNumber><Size>2</Size></Part><Part><PartNumber>1</PartNumber><Size>4</Size></Part></ListPartsResult>";
        assert_eq!(extract_part_numbers(xml), vec![1, 3]);
    }

    #[test]
    fn duplicate_part_entries_invalidate_the_whole_list() {
        let xml = "<ListPartsResult><Part><PartNumber>1</PartNumber><Size>4</Size></Part><Part><PartNumber>1</PartNumber><Size>4</Size></Part></ListPartsResult>";
        assert!(
            extract_part_numbers(xml).is_empty(),
            "duplicates must not be normalized into apparently trustworthy resume state"
        );
    }

    #[test]
    fn malformed_part_entry_invalidates_the_whole_list() {
        let xml = "<ListPartsResult><Part><PartNumber>1</PartNumber><Size>4</Size></Part><Part><PartNumber>bad</PartNumber><Size>4</Size></Part></ListPartsResult>";
        assert!(
            extract_part_numbers(xml).is_empty(),
            "a malformed entry must not be silently dropped beside a valid prefix"
        );
    }

    #[test]
    fn missing_or_duplicate_part_size_is_rejected() {
        for xml in [
            "<ListPartsResult><Part><PartNumber>1</PartNumber></Part></ListPartsResult>",
            "<ListPartsResult><Part><PartNumber>1</PartNumber><Size>4</Size><Size>4</Size></Part></ListPartsResult>",
        ] {
            assert!(parse_part_entries(xml).is_err());
        }
    }

    #[test]
    fn part_entry_limit_is_bounded() {
        let mut xml = String::from("<ListPartsResult>");
        for number in 1..=MAX_OSS_LIST_ENTRIES + 1 {
            xml.push_str(&format!(
                "<Part><PartNumber>{number}</PartNumber><Size>1</Size></Part>"
            ));
        }
        xml.push_str("</ListPartsResult>");
        assert!(parse_part_entries(&xml).is_err());
    }

    proptest! {
        #[test]
        fn any_duplicate_part_number_is_rejected(number in 1_u64..=10_000, size in 1_u64..=4_000_000) {
            let xml = format!(
                "<ListPartsResult><Part><PartNumber>{number}</PartNumber><Size>{size}</Size></Part><Part><PartNumber>{number}</PartNumber><Size>{size}</Size></Part></ListPartsResult>"
            );
            prop_assert!(parse_part_entries(&xml).is_err());
        }
    }
}

#[cfg(test)]
mod structure_regression_tests {
    use super::{extract_xml_tag, parse_part_page, parse_upload_page_for_key, OssPart};

    #[test]
    fn xml_structure_initiate_counts_empty_fields_and_ignores_comments() -> Result<(), &'static str>
    {
        for xml in [
            "<InitiateMultipartUploadResult><UploadId>id</UploadId><UploadId/></InitiateMultipartUploadResult>",
            "<InitiateMultipartUploadResult><!-- <UploadId>id</UploadId> --></InitiateMultipartUploadResult>",
            "<InitiateMultipartUploadResult><![CDATA[<UploadId>id</UploadId>]]></InitiateMultipartUploadResult>",
            "<InitiateMultipartUploadResult><Extension><UploadId>id</UploadId></Extension></InitiateMultipartUploadResult>",
            "<InitiateMultipartUploadResult><UploadId><Nested>id</Nested></UploadId></InitiateMultipartUploadResult>",
            "<InitiateMultipartUploadResult><UploadId>id</UploadId></BadRoot>",
        ] {
            if extract_xml_tag(xml, "UploadId").is_some() {
                return Err("invalid XML authorised an upload id");
            }
        }
        let xml = "<InitiateMultipartUploadResult><!-- <UploadId>fake</UploadId> --><UploadId>prefix<![CDATA[&suffix]]>&#33;</UploadId><Extension><UploadId>ignored</UploadId></Extension></InitiateMultipartUploadResult>";
        if extract_xml_tag(xml, "UploadId").as_deref() != Some("prefix&suffix!") {
            return Err("valid XML text or extension handling changed");
        }
        Ok(())
    }

    #[test]
    fn xml_structure_parts_reject_empty_duplicates_bad_root_and_markup() -> Result<(), &'static str>
    {
        for xml in [
            "<ListPartsResult><Part/></ListPartsResult>",
            "<ListPartsResult><Part><PartNumber>1</PartNumber><Size>4</Size><Size/></Part></ListPartsResult>",
            "<ListPartsResult><IsTruncated>false</IsTruncated><IsTruncated/></ListPartsResult>",
            "<ListPartsResult><Part><PartNumber>1</PartNumber><Size>4</Size></Part></Wrong>",
            "<Other><Part><PartNumber>1</PartNumber><Size>4</Size></Part></Other>",
            "<!DOCTYPE ListPartsResult [<!ENTITY a '4'>]><ListPartsResult><Part><PartNumber>1</PartNumber><Size>&a;</Size></Part></ListPartsResult>",
        ] {
            if parse_part_page(xml).is_ok() {
                return Err("invalid XML authorised resume parts");
            }
        }
        let page = parse_part_page("<ListPartsResult xmlns='urn:oss'><!-- <Part/> --><Unknown><Part/></Unknown><Part><PartNumber><![CDATA[1]]></PartNumber><Size>&#52;</Size></Part></ListPartsResult>")?;
        if page.parts
            != vec![OssPart {
                part_number: 1,
                size: 4,
            }]
        {
            return Err("valid XML part was not preserved");
        }
        Ok(())
    }

    #[test]
    fn xml_structure_uploads_count_empty_fields_and_decode_xml_before_url(
    ) -> Result<(), &'static str> {
        for xml in [
            "<ListMultipartUploadsResult><Upload/></ListMultipartUploadsResult>",
            "<ListMultipartUploadsResult><Upload><Key>key</Key><UploadId>id</UploadId><UploadId/></Upload></ListMultipartUploadsResult>",
            "<ListMultipartUploadsResult><EncodingType>url</EncodingType><EncodingType/></ListMultipartUploadsResult>",
            "<ListMultipartUploadsResult><IsTruncated>true</IsTruncated><NextKeyMarker>key</NextKeyMarker><NextUploadIdMarker/><NextUploadIdMarker>id</NextUploadIdMarker></ListMultipartUploadsResult>",
        ] {
            if parse_upload_page_for_key(xml, "key").is_ok() {
                return Err("invalid XML authorised resume uploads");
            }
        }
        let xml = "<ListMultipartUploadsResult><EncodingType>url</EncodingType><Upload><Key>a%26b&#37;2Fc</Key><UploadId><![CDATA[id&value]]></UploadId></Upload><IsTruncated>true</IsTruncated><NextKeyMarker>a%26b%2Fc</NextKeyMarker><NextUploadIdMarker/></ListMultipartUploadsResult>";
        let page = parse_upload_page_for_key(xml, "a&b/c")?;
        if page.upload_ids_for_key != vec!["id&value"]
            || page.next_markers != Some(("a&b/c".to_owned(), String::new()))
        {
            return Err("valid XML/url encoding or empty pagination marker changed");
        }
        Ok(())
    }
}
