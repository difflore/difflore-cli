use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::cloud::client::CloudClient;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RuleTrustEvidence {
    pub(crate) cited_count: i64,
    pub(crate) trust_rate: Option<f64>,
}

pub(crate) type RuleTrustMap = HashMap<String, RuleTrustEvidence>;

const TRUST_EVIDENCE_TTL: Duration = Duration::from_secs(60);
const TRUST_EVIDENCE_TIMEOUT: Duration = Duration::from_secs(3);
const HOOK_TRUST_EVIDENCE_TIMEOUT: Duration = Duration::from_millis(2500);

#[derive(Debug, Default)]
struct TrustEvidenceCache {
    fetched_at: Option<Instant>,
    map: RuleTrustMap,
}

fn cache() -> &'static std::sync::Mutex<TrustEvidenceCache> {
    static CACHE: OnceLock<std::sync::Mutex<TrustEvidenceCache>> = OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(TrustEvidenceCache::default()))
}

pub(crate) async fn fetch_cloud_top_rule_trust_evidence(cloud: &CloudClient) -> RuleTrustMap {
    fetch_cloud_top_rule_trust_evidence_with_timeout(cloud, TRUST_EVIDENCE_TIMEOUT).await
}

async fn fetch_cloud_top_rule_trust_evidence_with_timeout(
    cloud: &CloudClient,
    timeout: Duration,
) -> RuleTrustMap {
    if !cloud.is_logged_in() {
        return RuleTrustMap::new();
    }

    let stale = {
        let guard = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(fetched_at) = guard.fetched_at
            && fetched_at.elapsed() < TRUST_EVIDENCE_TTL
        {
            return guard.map.clone();
        }
        (!guard.map.is_empty()).then(|| guard.map.clone())
    };

    let Ok(Ok(top_rules)) = tokio::time::timeout(timeout, cloud.get_impact_top_rules()).await
    else {
        return stale.unwrap_or_default();
    };

    let map: RuleTrustMap = top_rules
        .rules
        .into_iter()
        .filter(|rule| rule.cited_count > 0 || rule.trust_rate.is_some())
        .map(|rule| {
            (
                rule.id,
                RuleTrustEvidence {
                    cited_count: rule.cited_count,
                    trust_rate: rule.trust_rate,
                },
            )
        })
        .collect();

    {
        let mut guard = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.fetched_at = Some(Instant::now());
        guard.map.clone_from(&map);
    }

    map
}

pub(crate) async fn fetch_default_cloud_top_rule_trust_evidence_for_hook() -> RuleTrustMap {
    let cloud = CloudClient::create().await;
    fetch_cloud_top_rule_trust_evidence_with_timeout(&cloud, HOOK_TRUST_EVIDENCE_TIMEOUT).await
}

pub(crate) fn format_trust_evidence(proof: &RuleTrustEvidence) -> Option<String> {
    let rate = proof.trust_rate?;
    let pct = (rate * 100.0).round() as i64;
    if proof.cited_count > 0 {
        Some(format!("trust {pct}% ({} cited)", proof.cited_count))
    } else {
        Some(format!("trust {pct}%"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_trust_evidence_labels_cited_trust_rate() {
        let proof = RuleTrustEvidence {
            cited_count: 2,
            trust_rate: Some(1.0),
        };

        assert_eq!(
            format_trust_evidence(&proof).as_deref(),
            Some("trust 100% (2 cited)")
        );
    }

    #[test]
    fn format_trust_evidence_hides_missing_rate() {
        let proof = RuleTrustEvidence {
            cited_count: 2,
            trust_rate: None,
        };

        assert_eq!(format_trust_evidence(&proof), None);
    }

    #[test]
    fn hook_trust_evidence_uses_shorter_hot_path_timeout() {
        assert!(HOOK_TRUST_EVIDENCE_TIMEOUT < TRUST_EVIDENCE_TIMEOUT);
    }
}
