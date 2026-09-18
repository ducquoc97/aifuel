use sha2::{Digest, Sha256};
use std::fmt::Write;

const MAX_TOOL_NAME_BYTES: usize = 128;
const SHORTENED_PREFIX_BYTES: usize = 62;

pub(crate) fn tool_name(server_id: &str, upstream_name: &str) -> String {
    let full = format!(
        "{}__{}",
        encode_component(server_id),
        encode_component(upstream_name)
    );
    if full.len() <= MAX_TOOL_NAME_BYTES {
        return full;
    }

    let digest = Sha256::digest(full.as_bytes());
    let mut shortened = String::with_capacity(MAX_TOOL_NAME_BYTES);
    shortened.push_str(&full[..SHORTENED_PREFIX_BYTES]);
    shortened.push_str("__");
    for byte in digest {
        write!(shortened, "{byte:02x}").expect("writing to a string cannot fail");
    }
    shortened
}

pub(crate) fn cursor(
    listing_kind: &str,
    gateway_scope: &str,
    snapshot_id: u64,
    offset: usize,
) -> String {
    let input = format!("{listing_kind}\0{gateway_scope}\0{snapshot_id}\0{offset}");
    let digest = Sha256::digest(input.as_bytes());
    let mut cursor = String::with_capacity(67);
    cursor.push_str("g1-");
    for byte in digest {
        write!(cursor, "{byte:02x}").expect("writing to a string cannot fail");
    }
    cursor
}

fn encode_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.') {
            encoded.push(*byte as char);
        } else {
            write!(encoded, "_{byte:02X}").expect("writing to a string cannot fail");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::{cursor, tool_name};

    #[test]
    fn tool_name_keeps_the_server_and_tool_pair_unambiguous() {
        assert_eq!(tool_name("docs", "search"), "docs__search");
        assert_eq!(tool_name("docs_under", "search"), "docs_5Funder__search");
        assert_eq!(tool_name("docs", "find__page"), "docs__find_5F_5Fpage");
    }

    #[test]
    fn long_tool_names_use_the_approved_fixed_length_digest_form() {
        let original = format!("server__{}", "a".repeat(150));
        let name = tool_name("server", &"a".repeat(150));

        assert_eq!(name.len(), 128);
        assert_eq!(&name[..62], &original[..62]);
        assert_eq!(&name[62..64], "__");
        assert!(name[64..].bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn cursors_are_scoped_to_the_listing_and_gateway_session() {
        let tools = cursor("tools", "gateway-one", 3, 4);

        assert_ne!(tools, cursor("resources", "gateway-one", 3, 4));
        assert_ne!(tools, cursor("tools", "gateway-two", 3, 4));
        assert_ne!(tools, cursor("tools", "gateway-one", 3, 5));
    }
}
