use shared::model::{
    recording_rule::{RecordingRule, RecordingRulesFile},
    UserId,
};
use std::path::{Path, PathBuf};
use tokio::{fs, sync::Mutex};

static MUTATION_GUARD: Mutex<()> = Mutex::const_new(());

#[derive(Debug, thiserror::Error)]
pub enum CreateRuleError {
    #[error("recording_rule_limit_exceeded")]
    LimitExceeded,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

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

    pub async fn list(&self) -> std::io::Result<Vec<RecordingRule>> { Ok(self.load().await?.rules) }

    pub async fn create(&self, rule: RecordingRule) -> std::io::Result<RecordingRule> {
        match self.create_with_owner_limit(rule, None).await {
            Ok(rule) => Ok(rule),
            Err(CreateRuleError::Io(err)) => Err(err),
            Err(CreateRuleError::LimitExceeded) => Err(std::io::Error::other("recording_rule_limit_exceeded")),
        }
    }

    pub async fn create_with_owner_limit(
        &self,
        rule: RecordingRule,
        max_rules: Option<u32>,
    ) -> Result<RecordingRule, CreateRuleError> {
        let _guard = MUTATION_GUARD.lock().await;
        let mut file = self.load().await?;
        if let Some(limit) = max_rules {
            let count = file.rules.iter().filter(|r| r.owner_id == rule.owner_id).count();
            if count >= limit as usize {
                return Err(CreateRuleError::LimitExceeded);
            }
        }
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

    pub async fn delete_by_owner(&self, owner_id: &UserId) -> std::io::Result<usize> {
        let _guard = MUTATION_GUARD.lock().await;
        let mut file = self.load().await?;
        let before = file.rules.len();
        file.rules.retain(|rule| &rule.owner_id != owner_id);
        let removed = before - file.rules.len();
        if removed > 0 {
            self.save(&file).await?;
        }
        Ok(removed)
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
        assert_eq!(repo.list().await.expect("list"), [] as [shared::model::recording_rule::RecordingRule; 0]);
    }

    #[tokio::test]
    async fn create_with_owner_limit_enforces_limit_per_owner() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = RecordingRuleRepository::new(dir.path());

        let mut r1 = rule("r1");
        r1.owner_id = UserId::from("api:alice");
        let mut r2 = rule("r2");
        r2.owner_id = UserId::from("api:alice");
        let mut r3 = rule("r3");
        r3.owner_id = UserId::from("api:alice");

        let mut other_owner_rule = rule("r_other");
        other_owner_rule.owner_id = UserId::from("api:bob");

        // Alice creates 2 rules with max limit 2
        assert!(repo.create_with_owner_limit(r1, Some(2)).await.is_ok());
        assert!(repo.create_with_owner_limit(r2, Some(2)).await.is_ok());

        // Alice's 3rd rule is rejected due to limit
        let err = repo.create_with_owner_limit(r3, Some(2)).await;
        assert!(matches!(err, Err(CreateRuleError::LimitExceeded)));

        // Bob's rule is accepted because limit is per owner
        assert!(repo.create_with_owner_limit(other_owner_rule, Some(2)).await.is_ok());
        assert_eq!(repo.list().await.expect("list").len(), 3);
    }

    #[tokio::test]
    async fn delete_by_owner_removes_matching_rules_and_leaves_others() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = RecordingRuleRepository::new(dir.path());

        let alice_rule = rule("r1");
        let mut bob_rule = rule("r2");
        bob_rule.owner_id = UserId::api("bob");

        repo.create(alice_rule).await.unwrap();
        repo.create(bob_rule).await.unwrap();

        let removed = repo.delete_by_owner(&UserId::api("bob")).await.unwrap();
        assert_eq!(removed, 1);

        let remaining = repo.list().await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "r1");
        assert_eq!(remaining[0].owner_id, UserId::from("web:alice"));

        let removed_again = repo.delete_by_owner(&UserId::api("bob")).await.unwrap();
        assert_eq!(removed_again, 0);
    }
}
