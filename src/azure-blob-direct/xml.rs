use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;

pub fn block_list_xml<'a>(block_ids: impl IntoIterator<Item = &'a str>) -> Vec<u8> {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>");
    for id in block_ids {
        xml.push_str("<Latest>");
        xml.push_str(id);
        xml.push_str("</Latest>");
    }
    xml.push_str("</BlockList>");
    xml.into_bytes()
}

pub(crate) fn parse_block_indices_from_block_list(xml: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for seg in xml.split("<Name>").skip(1) {
        if let Some((v, _)) = seg.split_once("</Name>") {
            if let Ok(raw) = BASE64_STANDARD.decode(v.trim()) {
                if let Ok(s) = String::from_utf8(raw) {
                    if let Ok(i) = s.parse::<usize>() {
                        out.push(i);
                    }
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::parse_block_indices_from_block_list;

    #[test]
    fn test_parse_block_indices_from_block_list_sorts_and_filters_invalid() {
        let xml = "<BlockList><UncommittedBlocks><Block><Name>MDAwMDAwMDM=</Name></Block><Block><Name>MDAwMDAwMDE=</Name></Block><Block><Name>not-base64</Name></Block></UncommittedBlocks></BlockList>";
        assert_eq!(parse_block_indices_from_block_list(xml), vec![1, 3]);
    }
}
