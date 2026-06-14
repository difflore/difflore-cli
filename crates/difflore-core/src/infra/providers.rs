//! Provider persistence and local provider auth probes.

use uuid::Uuid;

use crate::domain::models::{
    ProviderAddInput, ProviderRecord, ProviderRemoveInput, ProviderSetActiveInput,
    ProviderUpdateInput,
};
use crate::error::CoreError;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CheckAuthInput {
    pub engine: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckAuthResult {
    pub credential_detected: bool,
    pub verified: bool,
    pub method: String,
    pub detail: String,
}

#[derive(sqlx::FromRow)]
struct ProviderRow {
    id: String,
    name: String,
    base_url: String,
    api_key: String,
    model_mapping: String,
    is_active: i64,
    created_at: String,
    updated_at: String,
}

impl ProviderRow {
    fn decrypt_api_key(&self) -> String {
        match crate::infra::crypto::decrypt_secret(&self.api_key) {
            Ok(plaintext) => plaintext,
            Err(e) => {
                if crate::infra::env::debug_providers() {
                    eprintln!(
                        "[providers] failed to decrypt API key for provider {}: {e}",
                        self.id
                    );
                }
                String::new()
            }
        }
    }

    fn into_masked(self) -> ProviderRecord {
        let decrypted = self.decrypt_api_key();
        ProviderRecord {
            id: self.id,
            name: self.name,
            base_url: self.base_url,
            api_key: Some(mask_api_key(&decrypted)),
            model_mapping: serde_json::from_str(&self.model_mapping).unwrap_or_default(),
            is_active: self.is_active != 0,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }

    fn into_internal(self) -> ProviderRecord {
        let decrypted = self.decrypt_api_key();
        ProviderRecord {
            id: self.id,
            name: self.name,
            base_url: self.base_url,
            api_key: Some(decrypted),
            model_mapping: serde_json::from_str(&self.model_mapping).unwrap_or_default(),
            is_active: self.is_active != 0,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

fn mask_api_key(key: &str) -> String {
    // Count chars, not bytes. Real API keys are ASCII so byte vs char
    // length usually agrees, but a user-pasted key with stray emoji
    // or a non-ASCII paste artefact would otherwise panic on the
    // `&key[..3]` byte slice when 3 falls inside a multi-byte rune.
    let n = key.chars().count();
    if n <= 8 {
        return "****".to_owned();
    }
    let head: String = key.chars().take(3).collect();
    let tail: String = key.chars().skip(n - 3).collect();
    format!("{head}***{tail}")
}

pub async fn list(db: &sqlx::SqlitePool) -> crate::Result<Vec<ProviderRecord>> {
    let rows = sqlx::query_as!(
        ProviderRow,
        "SELECT id, name, base_url, api_key, model_mapping, is_active, created_at, updated_at FROM providers ORDER BY created_at DESC"
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(ProviderRow::into_masked).collect())
}

pub async fn get(
    db: &sqlx::SqlitePool,
    input: ProviderRemoveInput,
) -> crate::Result<Option<ProviderRecord>> {
    let row = sqlx::query_as!(
        ProviderRow,
        "SELECT id, name, base_url, api_key, model_mapping, is_active, created_at, updated_at FROM providers WHERE id = ?1",
        input.id
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map(ProviderRow::into_masked))
}

pub async fn add(db: &sqlx::SqlitePool, input: ProviderAddInput) -> crate::Result<ProviderRecord> {
    let id = format!("provider-{}", Uuid::new_v4());
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let mapping_json = serde_json::to_string(&input.model_mapping)?;
    // BYOK has been removed from the local CLI. Provider rows now only
    // describe an agent-cli sentinel (`agent-cli://<tool>`); the column
    // stays for back-compat with older DBs but is always written empty.
    let encrypted_key = crate::infra::crypto::encrypt_secret("")?;

    sqlx::query!(
        "INSERT INTO providers (id, name, base_url, api_key, model_mapping, is_active, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?6)",
        id,
        input.name,
        input.base_url,
        encrypted_key,
        mapping_json,
        now
    )
    .execute(db)
    .await?;

    Ok(ProviderRecord {
        id,
        name: input.name,
        base_url: input.base_url,
        api_key: None,
        model_mapping: input.model_mapping,
        is_active: false,
        created_at: now.clone(),
        updated_at: now,
    })
}

pub async fn update(
    db: &sqlx::SqlitePool,
    input: ProviderUpdateInput,
) -> crate::Result<ProviderRecord> {
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();

    let row = sqlx::query_as!(
        ProviderRow,
        "SELECT id, name, base_url, api_key, model_mapping, is_active, created_at, updated_at FROM providers WHERE id = ?1",
        input.id
    )
    .fetch_optional(db)
    .await?
    .ok_or_else(|| CoreError::NotFound(format!(
        "provider '{}' not found. List current providers with `difflore providers list`.",
        input.id
    )))?;

    let mut provider = row.into_internal();

    if let Some(name) = input.name {
        provider.name = name;
    }
    if let Some(base_url) = input.base_url {
        provider.base_url = base_url;
    }
    if let Some(mm) = input.model_mapping {
        provider.model_mapping = mm;
    }
    provider.updated_at = now;

    let mapping_json = serde_json::to_string(&provider.model_mapping)?;
    // BYOK has been removed; the api_key column is left in place for
    // older schemas but always overwritten with an encrypted empty string.
    let encrypted_secret = crate::infra::crypto::encrypt_secret("")?;

    let result = sqlx::query!(
        "UPDATE providers SET name=?1, base_url=?2, api_key=?3, model_mapping=?4, updated_at=?5 WHERE id=?6",
        provider.name,
        provider.base_url,
        encrypted_secret,
        mapping_json,
        provider.updated_at,
        provider.id
    )
    .execute(db)
    .await?;
    if result.rows_affected() == 0 {
        return Err(CoreError::NotFound(format!(
            "provider '{}' not found; cannot update. List current providers with `difflore providers list`.",
            provider.id
        )));
    }

    Ok(ProviderRecord {
        id: provider.id,
        name: provider.name,
        base_url: provider.base_url,
        api_key: None,
        model_mapping: provider.model_mapping,
        is_active: provider.is_active,
        created_at: provider.created_at,
        updated_at: provider.updated_at,
    })
}

pub async fn remove(db: &sqlx::SqlitePool, input: ProviderRemoveInput) -> crate::Result<()> {
    let result = sqlx::query!("DELETE FROM providers WHERE id = ?1", input.id)
        .execute(db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(CoreError::NotFound(format!(
            "provider '{}' not found. List current providers with `difflore providers list`.",
            input.id
        )));
    }
    Ok(())
}

pub async fn set_active(db: &sqlx::SqlitePool, input: ProviderSetActiveInput) -> crate::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query!("UPDATE providers SET is_active = 0")
        .execute(&mut *tx)
        .await?;
    if input.is_active {
        let result = sqlx::query!("UPDATE providers SET is_active = 1 WHERE id = ?1", input.id)
            .execute(&mut *tx)
            .await?;
        if result.rows_affected() == 0 {
            // Match the actionable message style used by `remove`: name
            // the bad id and tell the user where to look up the right
            // ones, instead of bare "provider".
            return Err(CoreError::NotFound(format!(
                "provider '{}' not found. List current providers with `difflore providers list`.",
                input.id
            )));
        }
    }
    tx.commit().await?;
    Ok(())
}

pub async fn check_auth(input: CheckAuthInput) -> crate::Result<CheckAuthResult> {
    let home = dirs::home_dir()
        .ok_or_else(|| CoreError::Internal("cannot resolve home directory".into()))?;

    let (detected, method, detail) = match input.engine.as_str() {
        "claude" => {
            let path = home.join(".claude").join(".credentials.json");
            let found = path.exists();
            (
                found,
                "config_file".to_owned(),
                if found {
                    "Credentials file detected".to_owned()
                } else {
                    "No credentials file detected".to_owned()
                },
            )
        }
        "codex" => {
            let found = crate::infra::env::var(crate::infra::env::OPENAI_API_KEY).is_some();
            (
                found,
                "env_var".to_owned(),
                if found {
                    "OPENAI_API_KEY environment variable detected".to_owned()
                } else {
                    "OPENAI_API_KEY not found in environment".to_owned()
                },
            )
        }
        "gemini" => {
            let path = home.join(".gemini").join("credentials.json");
            let found = path.exists();
            (
                found,
                "config_file".to_owned(),
                if found {
                    "Credentials file detected".to_owned()
                } else {
                    "No credentials file detected".to_owned()
                },
            )
        }
        "cursor" => {
            let path = home.join(".cursor");
            let found = path.exists();
            (
                found,
                "cli_config".to_owned(),
                if found {
                    "Config directory detected".to_owned()
                } else {
                    "Config directory not found".to_owned()
                },
            )
        }
        other => (
            false,
            "unknown".to_owned(),
            format!("unsupported engine: {other}"),
        ),
    };

    Ok(CheckAuthResult {
        credential_detected: detected,
        verified: false,
        method,
        detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_api_key_table() {
        let cases: &[(&str, &str)] = &[
            ("", "****"),
            ("short", "****"),
            ("12345678", "****"),
            ("sk-ant-1234567890abcd", "sk-***bcd"),
            ("abcdefghijk", "abc***ijk"),
        ];
        for (input, expected) in cases {
            assert_eq!(mask_api_key(input), *expected, "input: {input}");
        }
    }

    #[tokio::test]
    async fn check_auth_unknown_engine_reports_unsupported() {
        let res = check_auth(CheckAuthInput {
            engine: "bogus-engine".into(),
        })
        .await
        .expect("check_auth should not error for unknown engine");
        assert!(!res.credential_detected);
        assert!(!res.verified);
        assert_eq!(res.method, "unknown");
        assert!(res.detail.contains("unsupported"));
    }

    #[tokio::test]
    async fn check_auth_codex_method_is_env_var() {
        // Just validate the method/shape — don't mutate env to avoid races with
        // other parallel tests.
        let res = check_auth(CheckAuthInput {
            engine: "codex".into(),
        })
        .await
        .unwrap();
        assert_eq!(res.method, "env_var");
        assert!(!res.verified);
    }

    #[tokio::test]
    async fn check_auth_claude_uses_config_file_method() {
        let res = check_auth(CheckAuthInput {
            engine: "claude".into(),
        })
        .await
        .unwrap();
        assert_eq!(res.method, "config_file");
        assert!(!res.verified);
    }
}
