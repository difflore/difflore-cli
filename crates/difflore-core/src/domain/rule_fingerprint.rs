const FINGERPRINT_LEN: usize = 4;

pub fn rule_fingerprint(rule_id: &str) -> String {
    let digest = crate::infra::crypto::sha256_block_hex(rule_id.as_bytes());
    digest
        .strip_prefix("sha256:")
        .unwrap_or(&digest)
        .chars()
        .take(FINGERPRINT_LEN)
        .collect()
}

pub fn memory_citation_token(position: usize, rule_id: &str) -> String {
    format!("df:{position}-{}", rule_fingerprint(rule_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_citation_token_is_stable_and_positioned() {
        assert_eq!(
            memory_citation_token(2, "rule-alpha"),
            memory_citation_token(2, "rule-alpha")
        );
        assert!(memory_citation_token(2, "rule-alpha").starts_with("df:2-"));
        assert_ne!(
            memory_citation_token(2, "rule-alpha"),
            memory_citation_token(2, "rule-beta")
        );
    }
}
