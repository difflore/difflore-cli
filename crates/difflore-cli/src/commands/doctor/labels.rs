use crate::installer;
use crate::style;

pub(crate) const fn doctor_install_state_label(
    detected: bool,
    state: installer::InstallState,
) -> &'static str {
    match (detected, state) {
        (false, _) => "not detected",
        (true, installer::InstallState::Installed) => "installed",
        (true, installer::InstallState::NotInstalled) => "detected, not installed",
        (true, installer::InstallState::Conflict) => "detected, conflict",
        (true, installer::InstallState::Unknown) => "detected, install state unknown",
    }
}

pub(crate) const fn doctor_canonical_record_state_label(
    state: installer::CanonicalRecordState,
) -> &'static str {
    match state {
        installer::CanonicalRecordState::Missing => "missing",
        installer::CanonicalRecordState::Present => "present",
        installer::CanonicalRecordState::Stale => "stale",
        installer::CanonicalRecordState::Conflict => "conflict",
    }
}

pub(crate) const fn doctor_install_mark(
    detected: bool,
    state: installer::InstallState,
) -> &'static str {
    match (detected, state) {
        (true, installer::InstallState::Installed) => style::sym::OK,
        (true, installer::InstallState::Conflict) => style::sym::ERR,
        (true, installer::InstallState::Unknown | installer::InstallState::NotInstalled)
        | (false, _) => style::sym::WARN,
    }
}

pub(crate) const fn doctor_canonical_mark(state: installer::CanonicalRecordState) -> &'static str {
    match state {
        installer::CanonicalRecordState::Present => style::sym::OK,
        installer::CanonicalRecordState::Missing | installer::CanonicalRecordState::Stale => {
            style::sym::WARN
        }
        installer::CanonicalRecordState::Conflict => style::sym::ERR,
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
