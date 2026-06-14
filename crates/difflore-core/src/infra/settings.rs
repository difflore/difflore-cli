use crate::domain::models::AppSettingsRecord;
use crate::error::CoreError;

pub async fn get() -> crate::Result<AppSettingsRecord> {
    let path = crate::infra::paths::data_home()?.join("settings.json");
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
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
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
