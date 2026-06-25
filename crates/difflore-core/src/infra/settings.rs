use crate::domain::models::AppSettingsRecord;
use crate::error::CoreError;

pub async fn get() -> crate::Result<AppSettingsRecord> {
    let dir = crate::infra::paths::data_home()?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("settings.json");
    if !path.exists() {
        let defaults = AppSettingsRecord::default();
        let json = serde_json::to_string_pretty(&defaults)?;
        std::fs::write(&path, json)?;
        return Ok(defaults);
    }
    let content = std::fs::read_to_string(&path)?;
    let settings: AppSettingsRecord = serde_json::from_str(&content).map_err(|e| {
        CoreError::Internal(format!(
            "settings.json is corrupted and could not be parsed: {e}"
        ))
    })?;
    Ok(settings)
}

pub async fn update(input: AppSettingsRecord) -> crate::Result<AppSettingsRecord> {
    let dir = crate::infra::paths::data_home()?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("settings.json");

    let mut current: serde_json::Value = if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        serde_json::from_str(&content).map_err(|e| {
            CoreError::Internal(format!(
                "settings.json is corrupted and could not be parsed: {e}"
            ))
        })?
    } else {
        serde_json::json!({})
    };

    let patch: serde_json::Value = serde_json::to_value(&input)?;

    if let (Some(base), Some(overlay)) = (current.as_object_mut(), patch.as_object()) {
        for (k, v) in overlay {
            base.insert(k.clone(), v.clone());
        }
    }

    let merged: AppSettingsRecord = serde_json::from_value(current.clone())
        .map_err(|e| CoreError::Internal(format!("Failed to merge settings: {e}")))?;

    let json = serde_json::to_string_pretty(&merged)?;
    std::fs::write(&path, json)?;
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::{get, update};
    use crate::domain::models::AppSettingsRecord;

    #[test]
    fn get_creates_missing_data_dir_before_default_settings_write() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let home = tmp.path().join("nested").join("difflore-home");

        temp_env::with_var("DIFFLORE_HOME", Some(&home), || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let settings = rt.block_on(get()).expect("settings");

            assert_eq!(settings.theme, "dark");
            assert!(home.is_dir(), "settings::get should create DIFFLORE_HOME");
            assert!(
                home.join("settings.json").is_file(),
                "settings::get should write default settings"
            );
        });
    }

    #[test]
    fn update_rejects_corrupt_settings_without_overwriting() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let home = tmp.path().join("difflore-home");
        std::fs::create_dir_all(&home).expect("home dir");
        let path = home.join("settings.json");
        std::fs::write(&path, "{broken").expect("write corrupt settings");

        temp_env::with_var("DIFFLORE_HOME", Some(&home), || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let err = rt
                .block_on(update(AppSettingsRecord::default()))
                .expect_err("corrupt settings should fail");

            assert!(
                err.to_string().contains("settings.json is corrupted"),
                "unexpected error: {err}"
            );
            assert_eq!(
                std::fs::read_to_string(&path).expect("settings still exists"),
                "{broken"
            );
        });
    }
}
