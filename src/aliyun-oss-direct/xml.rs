pub(crate) fn extract_xml_tag(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let (_, tail) = body.split_once(open.as_str())?;
    let (value, _) = tail.split_once(close.as_str())?;
    Some(value.trim().to_string())
}

pub(crate) fn extract_upload_ids_for_key(xml: &str, key: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for seg in xml.split("<Upload>").skip(1) {
        if let Some((upload_block, _)) = seg.split_once("</Upload>") {
            let item_key = extract_xml_tag(upload_block, "Key");
            let upload_id = extract_xml_tag(upload_block, "UploadId");
            if item_key.as_deref() == Some(key) {
                if let Some(id) = upload_id {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

pub(crate) fn extract_part_numbers(xml: &str) -> Vec<u64> {
    let mut out = Vec::new();
    for seg in xml.split("<PartNumber>").skip(1) {
        if let Some((value, _)) = seg.split_once("</PartNumber>") {
            if let Ok(part_number) = value.trim().parse::<u64>() {
                out.push(part_number);
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::{extract_part_numbers, extract_upload_ids_for_key, extract_xml_tag};

    #[test]
    fn test_extract_xml_tag_trims_value() {
        let xml = "<Root><UploadId>  abc-123  </UploadId></Root>";
        assert_eq!(extract_xml_tag(xml, "UploadId").as_deref(), Some("abc-123"));
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
    fn test_extract_part_numbers_sorts_and_dedups() {
        let xml = "<ListPartsResult><Part><PartNumber>3</PartNumber></Part><Part><PartNumber>1</PartNumber></Part><Part><PartNumber>3</PartNumber></Part></ListPartsResult>";
        assert_eq!(extract_part_numbers(xml), vec![1, 3]);
    }
}
