use std::collections::BTreeSet;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;

use crate::provider_xml::{
    child_elements, document_root, exactly_one_nonempty_text, parse_document, XmlResult,
};

pub(crate) const MAX_AZURE_BLOCK_LIST_ENTRIES: usize = 50_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AzureBlock {
    pub(crate) index: usize,
    pub(crate) size: u64,
}

pub fn block_list_xml<'a>(block_ids: impl IntoIterator<Item = &'a str>) -> Vec<u8> {
    let mut xml = String::from(r#"<?xml version="1.0" encoding="utf-8"?><BlockList>"#);
    for id in block_ids {
        xml.push_str("<Latest>");
        xml.push_str(id);
        xml.push_str("</Latest>");
    }
    xml.push_str("</BlockList>");
    xml.into_bytes()
}

fn parse_canonical_block_index(encoded: &str) -> XmlResult<usize> {
    let raw = BASE64_STANDARD
        .decode(encoded)
        .map_err(|_| "Azure block id is not valid base64")?;
    if BASE64_STANDARD.encode(&raw) != encoded {
        return Err("Azure block id is not canonical base64");
    }
    let decoded = std::str::from_utf8(&raw).map_err(|_| "Azure block id is not UTF-8")?;
    if decoded.len() != 8 || !decoded.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("Azure block id is not an eight-digit decimal index");
    }
    let index = decoded
        .parse::<usize>()
        .map_err(|_| "Azure block index overflow")?;
    if format!("{index:08}") != decoded {
        return Err("Azure block id is not canonical");
    }
    Ok(index)
}

pub(crate) fn parse_block_entries_from_block_list(xml: &str) -> XmlResult<Vec<AzureBlock>> {
    let document = parse_document(xml)?;
    let root = document_root(&document, "BlockList")?;
    let sections = child_elements(root, "UncommittedBlocks", 2)?;
    let section = match sections.as_slice() {
        [] => return Ok(Vec::new()),
        [section] => *section,
        _ => return Err("duplicate UncommittedBlocks section"),
    };
    let blocks = child_elements(section, "Block", MAX_AZURE_BLOCK_LIST_ENTRIES)?;
    let mut parsed = Vec::with_capacity(blocks.len());
    let mut seen = BTreeSet::new();
    for block in blocks {
        let encoded = exactly_one_nonempty_text(block, "Name")?;
        let index = parse_canonical_block_index(&encoded)?;
        let size = exactly_one_nonempty_text(block, "Size")?
            .parse::<u64>()
            .map_err(|_| "invalid Azure block Size")?;
        if size == 0 {
            return Err("Azure block size must be non-zero");
        }
        if !seen.insert(index) {
            return Err("duplicate Azure block id");
        }
        parsed.push(AzureBlock { index, size });
    }
    parsed.sort_unstable_by_key(|block| block.index);
    Ok(parsed)
}

#[cfg(test)]
fn parse_block_indices_from_block_list(xml: &str) -> Vec<usize> {
    parse_block_entries_from_block_list(xml)
        .unwrap_or_default()
        .into_iter()
        .map(|block| block.index)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        parse_block_entries_from_block_list, parse_block_indices_from_block_list,
        MAX_AZURE_BLOCK_LIST_ENTRIES,
    };
    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
    use base64::Engine as _;
    use proptest::prelude::*;

    #[test]
    fn test_parse_block_indices_from_block_list_sorts_valid_entries() {
        let xml = "<BlockList><UncommittedBlocks><Block><Name>MDAwMDAwMDM=</Name><Size>2</Size></Block><Block><Name>MDAwMDAwMDE=</Name><Size>4</Size></Block></UncommittedBlocks></BlockList>";
        assert_eq!(parse_block_indices_from_block_list(xml), vec![1, 3]);
    }

    #[test]
    fn duplicate_block_ids_invalidate_the_whole_list() {
        let xml = "<BlockList><UncommittedBlocks><Block><Name>MDAwMDAwMDA=</Name><Size>4</Size></Block><Block><Name>MDAwMDAwMDA=</Name><Size>4</Size></Block></UncommittedBlocks></BlockList>";
        assert!(
            parse_block_indices_from_block_list(xml).is_empty(),
            "duplicate ids must not be silently deduplicated"
        );
    }

    #[test]
    fn malformed_block_id_invalidates_the_whole_list() {
        let xml = "<BlockList><UncommittedBlocks><Block><Name>MDAwMDAwMDA=</Name><Size>4</Size></Block><Block><Name>not-base64</Name><Size>4</Size></Block></UncommittedBlocks></BlockList>";
        assert!(
            parse_block_indices_from_block_list(xml).is_empty(),
            "a malformed id must not be silently dropped beside a valid prefix"
        );
    }

    #[test]
    fn block_entry_requires_exactly_one_name() {
        for xml in [
            "<BlockList><UncommittedBlocks><Block><Size>4</Size></Block></UncommittedBlocks></BlockList>",
            "<BlockList><UncommittedBlocks><Block><Name>MDAwMDAwMDA=</Name><Name>MDAwMDAwMDE=</Name><Size>4</Size></Block></UncommittedBlocks></BlockList>",
        ] {
            assert!(parse_block_entries_from_block_list(xml).is_err());
        }
    }

    #[test]
    fn duplicate_uncommitted_sections_are_rejected() {
        let xml = "<BlockList><UncommittedBlocks></UncommittedBlocks><UncommittedBlocks></UncommittedBlocks></BlockList>";
        assert!(parse_block_entries_from_block_list(xml).is_err());
    }

    #[test]
    fn missing_or_duplicate_block_size_is_rejected() {
        for xml in [
            "<BlockList><UncommittedBlocks><Block><Name>MDAwMDAwMDA=</Name></Block></UncommittedBlocks></BlockList>",
            "<BlockList><UncommittedBlocks><Block><Name>MDAwMDAwMDA=</Name><Size>4</Size><Size>4</Size></Block></UncommittedBlocks></BlockList>",
        ] {
            assert!(parse_block_entries_from_block_list(xml).is_err());
        }
    }

    #[test]
    fn noncanonical_block_id_is_rejected() {
        // Base64 for "0": it decodes to an integer but is not the provider's
        // fixed-width deterministic block-id format.
        let xml = "<BlockList><UncommittedBlocks><Block><Name>MA==</Name><Size>4</Size></Block></UncommittedBlocks></BlockList>";
        assert!(parse_block_entries_from_block_list(xml).is_err());
    }

    #[test]
    fn unknown_fields_are_ignored_and_valid_unordered_entries_are_sorted() {
        let xml = "<BlockList><Unknown>x</Unknown><UncommittedBlocks><Block><Name>MDAwMDAwMDE=</Name><Size>2</Size><Unknown>x</Unknown></Block><Block><Name>MDAwMDAwMDA=</Name><Size>4</Size></Block></UncommittedBlocks></BlockList>";
        let parsed = parse_block_entries_from_block_list(xml).unwrap();
        assert_eq!(
            parsed.iter().map(|block| block.index).collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(
            parsed.iter().map(|block| block.size).collect::<Vec<_>>(),
            vec![4, 2]
        );
    }

    #[test]
    fn block_entry_limit_is_bounded() {
        let mut xml = String::from("<BlockList><UncommittedBlocks>");
        for index in 0..=MAX_AZURE_BLOCK_LIST_ENTRIES {
            let id = BASE64_STANDARD.encode(format!("{index:08}"));
            xml.push_str(&format!("<Block><Name>{id}</Name><Size>1</Size></Block>"));
        }
        xml.push_str("</UncommittedBlocks></BlockList>");
        assert!(parse_block_entries_from_block_list(&xml).is_err());
    }

    proptest! {
        #[test]
        fn any_duplicate_canonical_block_id_is_rejected(index in 0_usize..50_000, size in 1_u64..=4_000_000) {
            let id = BASE64_STANDARD.encode(format!("{index:08}"));
            let xml = format!(
                "<BlockList><UncommittedBlocks><Block><Name>{id}</Name><Size>{size}</Size></Block><Block><Name>{id}</Name><Size>{size}</Size></Block></UncommittedBlocks></BlockList>"
            );
            prop_assert!(parse_block_entries_from_block_list(&xml).is_err());
        }
    }
}

#[cfg(test)]
mod structure_regression_tests {
    use super::{parse_block_entries_from_block_list, AzureBlock};

    #[test]
    fn xml_structure_blocks_reject_empty_entries_duplicates_and_bad_structure(
    ) -> Result<(), &'static str> {
        for xml in [
            "<BlockList><UncommittedBlocks><Block/></UncommittedBlocks></BlockList>",
            "<BlockList><UncommittedBlocks><Block><Name>MDAwMDAwMDA=</Name><Name/><Size>4</Size></Block></UncommittedBlocks></BlockList>",
            "<BlockList><UncommittedBlocks/><UncommittedBlocks/></BlockList>",
            "<BlockList><UncommittedBlocks></Wrong></BlockList>",
            "<Other/>",
            "<!DOCTYPE BlockList [<!ENTITY x '4'>]><BlockList/>",
        ] {
            if parse_block_entries_from_block_list(xml).is_ok() {
                return Err("invalid XML authorised resume blocks");
            }
        }
        Ok(())
    }

    #[test]
    fn xml_structure_blocks_allow_empty_containers_comments_cdata_and_entities(
    ) -> Result<(), &'static str> {
        for xml in [
            "<BlockList/>",
            "<BlockList><CommittedBlocks/><UncommittedBlocks/></BlockList>",
        ] {
            if !parse_block_entries_from_block_list(xml)?.is_empty() {
                return Err("empty block list changed");
            }
        }
        let xml = "<BlockList xmlns='urn:azure'><CommittedBlocks/><UncommittedBlocks><!-- <Block/> --><Extension><Block/></Extension><Block><Name><![CDATA[MDAwMDAwMDA=]]></Name><Size>&#x34;</Size><Extension/></Block></UncommittedBlocks></BlockList>";
        if parse_block_entries_from_block_list(xml)? != vec![AzureBlock { index: 0, size: 4 }] {
            return Err("valid XML block was not preserved");
        }
        Ok(())
    }
}

#[cfg(test)]
mod full_list_resource_tests {
    use super::{parse_block_entries_from_block_list, MAX_AZURE_BLOCK_LIST_ENTRIES};
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;

    #[test]
    fn xml_depth_preflight_accepts_fifty_thousand_pretty_printed_azure_blocks(
    ) -> Result<(), &'static str> {
        let mut xml = String::from("<?xml version='1.0' encoding='utf-8'?>\n<BlockList>\n  <CommittedBlocks/>\n  <UncommittedBlocks>\n");
        for index in 0..MAX_AZURE_BLOCK_LIST_ENTRIES {
            let id = STANDARD.encode(format!("{index:08}"));
            xml.push_str(&format!(
                "    <Block>\n      <Name>{id}</Name>\n      <Size>4</Size>\n    </Block>\n"
            ));
        }
        xml.push_str("  </UncommittedBlocks>\n</BlockList>");
        let blocks = parse_block_entries_from_block_list(&xml)?;
        if blocks.len() != MAX_AZURE_BLOCK_LIST_ENTRIES
            || blocks.first().map(|block| (block.index, block.size)) != Some((0, 4))
            || blocks.last().map(|block| (block.index, block.size)) != Some((49_999, 4))
        {
            return Err("the resource guard changed the provider's full valid block-list limit");
        }
        Ok(())
    }
}
