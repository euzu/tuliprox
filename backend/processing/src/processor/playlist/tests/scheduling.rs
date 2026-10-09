use super::{
    collect_target_task_result, stalker_checkpoint_message, target_mutated_resources, target_waiting_message,
    wait_for_target_finalizer_slot, with_sequential_group, TargetJobResult, MAX_CONCURRENT_TARGET_FINALIZERS,
};
use shared::model::{ConfigTargetDto, PipelineStats};
use std::{sync::Arc, time::Duration};
use tokio::task::JoinSet;
use tuliprox_core::model::{Config, ConfigTarget};

#[tokio::test]
async fn parallel_input_scheduler_serializes_equal_groups_and_overlaps_distinct_groups() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn observe(active: &AtomicUsize, maximum: &AtomicUsize) {
        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
        maximum.fetch_max(current, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        active.fetch_sub(1, Ordering::SeqCst);
    }

    let locks = tuliprox_core::utils::FileLockManager::default();
    let active = AtomicUsize::new(0);
    let maximum = AtomicUsize::new(0);
    tokio::join!(
        with_sequential_group(&locks, Some(7), true, observe(&active, &maximum)),
        with_sequential_group(&locks, Some(7), true, observe(&active, &maximum)),
    );
    assert_eq!(maximum.load(Ordering::SeqCst), 1);

    maximum.store(0, Ordering::SeqCst);
    tokio::join!(
        with_sequential_group(&locks, Some(7), true, observe(&active, &maximum)),
        with_sequential_group(&locks, Some(8), true, observe(&active, &maximum)),
    );
    assert_eq!(maximum.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn parallel_input_scheduler_releases_group_after_abort() {
    let locks = Arc::new(tuliprox_core::utils::FileLockManager::default());
    let task_locks = Arc::clone(&locks);
    let task = tokio::spawn(async move {
        with_sequential_group(&task_locks, Some(7), true, std::future::pending::<()>()).await;
    });
    tokio::task::yield_now().await;
    task.abort();
    let _ = task.await;

    tokio::time::timeout(Duration::from_secs(1), with_sequential_group(&locks, Some(7), true, std::future::ready(())))
        .await
        .expect("aborting an input job must release its sequential group");
}

#[test]
fn input_progress_message_contains_each_target_and_blocking_input() {
    let targets = ["target-a", "target-b"];
    let inputs = ["input-a", "input-b"];
    let messages: Vec<_> = targets
        .iter()
        .flat_map(|target| inputs.iter().map(move |input| target_waiting_message(target, input)))
        .collect();

    assert_eq!(messages.len(), 4);
    for target in targets {
        for input in inputs {
            assert!(messages.contains(&format!("Target '{target}' is waiting for input '{input}'")));
        }
    }
    assert!(stalker_checkpoint_message("portal-a").contains("portal-a"));
}

#[tokio::test]
async fn parallel_target_pipeline_bounds_active_finalizers() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let mut tasks = JoinSet::new();
    let mut results = Vec::new();
    let mut errors = Vec::new();

    for index in 0..6 {
        wait_for_target_finalizer_slot(&mut tasks, &mut results, &mut errors).await;
        let active = Arc::clone(&active);
        let maximum = Arc::clone(&maximum);
        tasks.spawn(async move {
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            maximum.fetch_max(current, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(10)).await;
            active.fetch_sub(1, Ordering::SeqCst);
            TargetJobResult {
                index,
                name: format!("target-{index}"),
                result: Ok(()),
                errors: Vec::new(),
                processing: PipelineStats::default(),
            }
        });
    }
    while let Some(result) = tasks.join_next().await {
        collect_target_task_result(result, &mut results, &mut errors);
    }

    assert!(maximum.load(Ordering::SeqCst) <= MAX_CONCURRENT_TARGET_FINALIZERS);
    assert_eq!(results.len(), 6);
    assert!(errors.is_empty());
}

#[test]
fn parallel_target_pipeline_normalizes_conflicting_output_resources() {
    let config = Config { storage_dir: "/tmp/tuliprox-target-resources".to_string(), ..Config::default() };

    let mut spaced = ConfigTarget::from(&ConfigTargetDto::default());
    spaced.name = "A B".to_string();
    let mut underscored = ConfigTarget::from(&ConfigTargetDto::default());
    underscored.name = "A_B".to_string();
    assert!(!target_mutated_resources(&config, &spaced).is_disjoint(&target_mutated_resources(&config, &underscored)));

    spaced.name = "one".to_string();
    spaced.output = vec![tuliprox_core::model::TargetOutput::M3u(tuliprox_core::model::M3uTargetOutput {
        filename: Some("out/../x.m3u".to_string()),
        include_type_in_url: false,
        mask_redirect_url: false,
        filter: None,
    })];
    underscored.name = "two".to_string();
    underscored.output = vec![tuliprox_core::model::TargetOutput::M3u(tuliprox_core::model::M3uTargetOutput {
        filename: Some("x.m3u".to_string()),
        include_type_in_url: false,
        mask_redirect_url: false,
        filter: None,
    })];
    assert!(!target_mutated_resources(&config, &spaced).is_disjoint(&target_mutated_resources(&config, &underscored)));
}
