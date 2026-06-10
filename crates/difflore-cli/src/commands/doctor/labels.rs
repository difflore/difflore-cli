use crate::mcp_install;
use crate::style;

pub(crate) const fn doctor_install_state_label(
    detected: bool,
    state: mcp_install::InstallState,
) -> &'static str {
    match (detected, state) {
        (false, _) => "not detected",
        (true, mcp_install::InstallState::Installed) => "installed",
        (true, mcp_install::InstallState::NotInstalled) => "detected, not installed",
        (true, mcp_install::InstallState::Conflict) => "detected, conflict",
        (true, mcp_install::InstallState::Unknown) => "detected, install state unknown",
    }
}

pub(crate) const fn doctor_canonical_record_state_label(
    state: mcp_install::CanonicalRecordState,
) -> &'static str {
    match state {
        mcp_install::CanonicalRecordState::Missing => "missing",
        mcp_install::CanonicalRecordState::Present => "present",
        mcp_install::CanonicalRecordState::Stale => "stale",
        mcp_install::CanonicalRecordState::Conflict => "conflict",
    }
}

pub(crate) const fn doctor_install_mark(
    detected: bool,
    state: mcp_install::InstallState,
) -> &'static str {
    match (detected, state) {
        (true, mcp_install::InstallState::Installed) => style::sym::OK,
        (true, mcp_install::InstallState::Conflict) => style::sym::ERR,
        (true, mcp_install::InstallState::Unknown | mcp_install::InstallState::NotInstalled)
        | (false, _) => style::sym::WARN,
    }
}

pub(crate) const fn doctor_canonical_mark(
    state: mcp_install::CanonicalRecordState,
) -> &'static str {
    match state {
        mcp_install::CanonicalRecordState::Present => style::sym::OK,
        mcp_install::CanonicalRecordState::Missing | mcp_install::CanonicalRecordState::Stale => {
            style::sym::WARN
        }
        mcp_install::CanonicalRecordState::Conflict => style::sym::ERR,
    }
}

pub(crate) fn doctor_probe_freshness(
    timestamp: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
) -> &'static str {
    let ttl = chrono::Duration::minutes(difflore_core::infra::startup::STARTUP_TTL_MINUTES);
    match timestamp {
        Some(ts) if (now - ts) < ttl => "fresh",
        Some(_) => "stale",
        None => "missing",
    }
}

#[cfg(test)]
mod tests {
    use super::doctor_probe_freshness;

    #[test]
    fn doctor_probe_freshness_uses_startup_ttl() {
        let now = chrono::Utc::now();
        assert_eq!(doctor_probe_freshness(Some(now), now), "fresh");
        assert_eq!(
            doctor_probe_freshness(
                Some(
                    now - chrono::Duration::minutes(
                        difflore_core::infra::startup::STARTUP_TTL_MINUTES + 1,
                    ),
                ),
                now,
            ),
            "stale"
        );
        assert_eq!(doctor_probe_freshness(None, now), "missing");
    }
}
