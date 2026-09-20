use sha2::{Digest, Sha256};
use std::fmt::Write;

const RESOURCE_SCHEME_PREFIX: &str = "aifuel-resource+";

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

pub(crate) fn cursor(server_id: &str, snapshot_id: u64, offset: usize) -> String {
    let input = format!("{server_id}\0{snapshot_id}\0{offset}");
    let digest = Sha256::digest(input.as_bytes());
    let mut cursor = String::with_capacity(67);
    cursor.push_str("g1-");
    for byte in digest {
        write!(cursor, "{byte:02x}").expect("writing to a string cannot fail");
    }
    cursor
}

pub(crate) fn snapshot_cursor(
    kind: &str,
    server_id: &str,
    snapshot_id: u64,
    offset: usize,
) -> String {
    let input = format!("{kind}\0{server_id}\0{snapshot_id}\0{offset}");
    let digest = Sha256::digest(input.as_bytes());
    let mut cursor = String::with_capacity(68);
    cursor.push_str("r1-");
    for byte in digest {
        write!(cursor, "{byte:02x}").expect("writing to a string cannot fail");
    }
    cursor
}

/// Replace only the scheme of an absolute upstream URI with a reversible,
/// server-scoped scheme. The remainder is deliberately copied byte-for-byte;
/// parsing and reserializing it could change escaping or an IPv6 authority.
pub(crate) fn resource_uri(server_id: &str, upstream_uri: &str) -> Result<String, String> {
    let (scheme, remainder) = split_absolute_scheme(upstream_uri)?;
    let mut result = String::with_capacity(
        RESOURCE_SCHEME_PREFIX.len()
            + server_id.len().saturating_mul(2)
            + scheme.len().saturating_mul(2)
            + remainder.len(),
    );
    result.push_str(RESOURCE_SCHEME_PREFIX);
    push_lower_hex(&mut result, server_id.as_bytes());
    result.push('+');
    push_lower_hex(&mut result, scheme.as_bytes());
    result.push_str(remainder);
    Ok(result)
}

/// Decode a gateway resource URI without normalizing its upstream remainder.
pub(crate) fn decode_resource_uri(uri: &str) -> Result<(String, String), String> {
    let Some(encoded) = uri.strip_prefix(RESOURCE_SCHEME_PREFIX) else {
        return Err("resource URI does not use the AI Fuel gateway scheme".to_owned());
    };
    let Some((server_hex, remainder)) = encoded.split_once('+') else {
        return Err("gateway resource URI is missing its server component".to_owned());
    };
    let Some((scheme_hex, upstream_remainder)) = remainder.split_once(':') else {
        return Err("gateway resource URI is missing its original scheme".to_owned());
    };
    let server_id = decode_utf8_hex(server_hex, "server id")?;
    let scheme = decode_utf8_hex(scheme_hex, "scheme")?;
    let reconstructed = format!("{scheme}:{upstream_remainder}");
    let (validated_scheme, _) = split_absolute_scheme(&reconstructed)?;
    if validated_scheme != scheme {
        return Err("gateway resource URI contains an invalid original scheme".to_owned());
    }
    Ok((server_id, format!("{scheme}:{upstream_remainder}")))
}

/// Rewrite a URI template whose scheme is literal. Expressions after the
/// colon remain unchanged, so reserved expansions have identical semantics
/// before and after gateway routing.
pub(crate) fn resource_template(
    server_id: &str,
    upstream_template: &str,
) -> Result<String, String> {
    resource_uri(server_id, upstream_template)
}

pub(crate) fn is_direct_http_uri(uri: &str) -> bool {
    split_absolute_scheme(uri)
        .map(|(scheme, _)| {
            scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
        })
        .unwrap_or(false)
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

fn split_absolute_scheme(uri: &str) -> Result<(&str, &str), String> {
    let Some(colon) = uri.find(':') else {
        return Err("resource URI must have an absolute literal scheme".to_owned());
    };
    let scheme = &uri[..colon];
    let mut chars = scheme.chars();
    if !chars
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic())
        || !chars.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        })
    {
        return Err("resource URI must have a valid literal scheme".to_owned());
    }
    if scheme.contains('{') || scheme.contains('}') {
        return Err("variable-scheme resource URI templates are unsupported".to_owned());
    }
    Ok((scheme, &uri[colon..]))
}

fn push_lower_hex(output: &mut String, bytes: &[u8]) {
    for byte in bytes {
        write!(output, "{byte:02x}").expect("writing to a string cannot fail");
    }
}

fn decode_utf8_hex(value: &str, label: &str) -> Result<String, String> {
    if value.is_empty() || !value.len().is_multiple_of(2) {
        return Err(format!(
            "gateway resource URI has an invalid {label} encoding"
        ));
    }
    let bytes = (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| format!("gateway resource URI has an invalid {label} encoding"))?;
    String::from_utf8(bytes)
        .map_err(|_| format!("gateway resource URI has an invalid UTF-8 {label}"))
}

#[cfg(test)]
mod tests {
    use super::{decode_resource_uri, resource_template, resource_uri, tool_name};

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
    fn resource_identity_preserves_ipv6_escaping_and_scheme_case() {
        let gateway = resource_uri("docs/東京", "HtTp://[::1]/a%2Fb?q=%2F#frag").unwrap();
        assert_eq!(
            gateway,
            "aifuel-resource+646f63732fe69d b1e4baac+48745470://[::1]/a%2Fb?q=%2F#frag"
                .replace(' ', "")
        );
        assert_eq!(
            decode_resource_uri(&gateway).unwrap(),
            (
                "docs/東京".to_owned(),
                "HtTp://[::1]/a%2Fb?q=%2F#frag".to_owned()
            )
        );
    }

    #[test]
    fn resource_template_keeps_reserved_expansion_after_literal_scheme() {
        let template = resource_template("docs", "custom+scheme:///{+path}").unwrap();
        assert_eq!(
            template,
            "aifuel-resource+646f6373+637573746f6d2b736368656d65:///{+path}"
        );
    }

    #[test]
    fn variable_scheme_templates_are_rejected() {
        assert!(resource_template("docs", "{scheme}:///{path}").is_err());
        assert!(resource_template("docs", "/relative/{path}").is_err());
    }
}
