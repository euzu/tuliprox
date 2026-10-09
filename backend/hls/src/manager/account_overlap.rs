use super::HlsProxyManager;
use log::debug;
use shared::utils::sanitize_sensitive_info;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct HlsAccountOverlapCooldownKey {
    pub(super) input_name: Arc<str>,
    pub(super) account_name: Arc<str>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct HlsAccountOverlapCooldown {
    pub(super) until_ms: u64,
}

#[derive(Debug, Clone, Copy)]
enum HlsAccountOverlapCooldownReason {
    ReclaimedByOriginalOwner,
    SpeculativePromoted,
}

impl HlsAccountOverlapCooldownReason {
    fn as_log_reason(self) -> &'static str {
        match self {
            Self::ReclaimedByOriginalOwner => "reclaimed-by-original-owner",
            Self::SpeculativePromoted => "speculative-promoted",
        }
    }
}

impl HlsProxyManager {
    pub async fn is_account_overlap_cooling_down(
        &self,
        input_name: &Arc<str>,
        account_name: &Arc<str>,
        now_ms: u64,
    ) -> bool {
        let key =
            HlsAccountOverlapCooldownKey { input_name: Arc::clone(input_name), account_name: Arc::clone(account_name) };
        let mut cooldowns = self.account_overlap_cooldowns.write().await;
        let Some(cooldown) = cooldowns.get(&key).copied() else {
            return false;
        };
        if now_ms >= cooldown.until_ms {
            cooldowns.remove(&key);
            return false;
        }
        true
    }

    pub async fn mark_account_overlap_reclaimed_cooldown(
        &self,
        input_name: Arc<str>,
        account_name: Arc<str>,
        now_ms: u64,
        hard_active_window_ms: u64,
    ) {
        self.mark_account_overlap_cooldown(
            input_name,
            account_name,
            now_ms,
            hard_active_window_ms,
            HlsAccountOverlapCooldownReason::ReclaimedByOriginalOwner,
        )
        .await;
    }

    pub async fn mark_account_overlap_promoted_cooldown(
        &self,
        input_name: Arc<str>,
        account_name: Arc<str>,
        now_ms: u64,
        hard_active_window_ms: u64,
    ) {
        self.mark_account_overlap_cooldown(
            input_name,
            account_name,
            now_ms,
            hard_active_window_ms,
            HlsAccountOverlapCooldownReason::SpeculativePromoted,
        )
        .await;
    }

    async fn mark_account_overlap_cooldown(
        &self,
        input_name: Arc<str>,
        account_name: Arc<str>,
        now_ms: u64,
        hard_active_window_ms: u64,
        reason: HlsAccountOverlapCooldownReason,
    ) {
        let until_ms = now_ms.saturating_add(hard_active_window_ms);
        if until_ms <= now_ms {
            return;
        }
        let key = HlsAccountOverlapCooldownKey { input_name, account_name };
        self.account_overlap_cooldowns.write().await.insert(key.clone(), HlsAccountOverlapCooldown { until_ms });
        debug!(
            "HLS account overlap cooldown set for input {} account {} until {} ms after {}",
            sanitize_sensitive_info(key.input_name.as_ref()),
            sanitize_sensitive_info(key.account_name.as_ref()),
            until_ms,
            reason.as_log_reason()
        );
    }
}
