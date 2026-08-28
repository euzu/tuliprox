use shared::model::recording_rule::{RecordingRule, RecordingRulesFile};
use std::path::{Path, PathBuf};
use tokio::{fs, sync::Mutex};

static MUTATION_GUARD: Mutex<()> = Mutex::const_new(());

#[derive(Debug, Clone)]
pub struct RecordingRuleRepository {
    path: PathBuf,
}

impl RecordingRuleRepository {
    pub fn new(storage_dir: impl AsRef<Path>) -> Self {
        Self { path: storage_dir.as_ref().join("recording_rules.json") }
    }

    pub async fn load(&self) -> std::io::Result<RecordingRulesFile> {
        match fs::read(&self.path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(invalid_data),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(RecordingRulesFile::default()),
            Err(err) => Err(err),
        }
    }

    pub async fn save(&self, file: &RecordingRulesFile) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let bytes = serde_json::to_vec_pretty(file).map_err(invalid_data)?;
        tuliprox_core::utils::atomic_json_store::write_json_atomic(&self.path, &bytes).await
    }

    pub async fn list(&self) -> std::io::Result<Vec<RecordingRule>> {
        Ok(self.load().await?.rules)
    }

    pub async fn create(&self, rule: RecordingRule) -> std::io::Result<RecordingRule> {
        let _guard = MUTATION_GUARD.lock().await;
        let mut file = self.load().await?;
        file.rules.push(rule.clone());
        self.save(&file).await?;
        Ok(rule)
    }

    pub async fn update(&self, rule: RecordingRule) -> std::io::Result<Option<RecordingRule>> {
        let _guard = MUTATION_GUARD.lock().await;
        let mut file = self.load().await?;
        let Some(existing) = file.rules.iter_mut().find(|existing| existing.id == rule.id) else {
            return Ok(None);
        };
        *existing = rule.clone();
        self.save(&file).await?;
        Ok(Some(rule))
    }

    pub async fn delete(&self, id: &str) -> std::io::Result<bool> {
        let _guard = MUTATION_GUARD.lock().await;
        let mut file = self.load().await?;
        let before = file.rules.len();
        file.rules.retain(|rule| rule.id != id);
        if file.rules.len() == before {
            return Ok(false);
        }
        self.save(&file).await?;
        Ok(true)
    }
}

fn invalid_data(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{
        recording_rule::{RuleBody, RuleSource, RuleVisibility},
        UserId,
    };

    fn rule(id: &str) -> RecordingRule {
        RecordingRule {
            id: id.to_string(),
            owner_id: UserId::from("web:alice"),
            visibility: RuleVisibility::Private,
            enabled: true,
            source: RuleSource::new("1", "2", "input"),
            channel_id: None,
            body: RuleBody::WeeklyTimeslot {
                weekday: 1,
                local_start_time: "20:00".to_string(),
                duration_secs: 1800,
                timezone: "UTC".to_string(),
            },
            pre_roll_secs: 0,
            post_roll_secs: 0,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[tokio::test]
    async fn create_update_delete_round_trips_atomic_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = RecordingRuleRepository::new(dir.path());
        let created = repo.create(rule("r1")).await.expect("create");
        assert_eq!(created.id, "r1");
        assert_eq!(repo.list().await.expect("list").len(), 1);

        let mut changed = created;
        changed.enabled = false;
        assert!(!repo.update(changed).await.expect("update").expect("found").enabled);
        assert!(repo.delete("r1").await.expect("delete"));
        assert!(repo.list().await.expect("list").is_empty());
    }
}
