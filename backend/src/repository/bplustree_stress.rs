use super::bplustree::{BPlusTree, BPlusTreeQuery, BPlusTreeSerialWriter, BPlusTreeUpdate, FlushPolicy};
use rand::prelude::*;
use rand::distr::Alphanumeric;
use std::io::Write;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;

// Run with:  `cargo test --release --package tuliprox -- stress_test_bplustree -- --nocapture`

// Helper to generate random string
fn random_string(len: usize) -> String {
    rand::rng().sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

#[inline]
fn lcg_next(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state
}

fn percentile_nearest_rank(sorted_values: &[f64], percentile: f64) -> f64 {
    if sorted_values.is_empty() {
        return 0.0;
    }
    let p = percentile.clamp(0.0, 1.0);
    let n = sorted_values.len();
    let rank = ((p * n as f64).ceil() as usize).saturating_sub(1).min(n - 1);
    sorted_values[rank]
}

#[test]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::too_many_lines
)]
fn stress_test_bplustree() {
    let temp_file = NamedTempFile::new().unwrap();
    let filepath = temp_file.path().to_path_buf();
    let log_path = std::path::Path::new("/tmp/stress_results.txt");
    let mut log_file = std::fs::File::create(log_path).unwrap();

    // Config
    let num_items = 1_000_000usize;
    let query_count = 50_000;
    let insert_runs = 7usize;
    let value_pool_size = 2_048usize;
    let small_val_len = 50;
    let large_val_len = 500; // Larger than packed limit (256)
    
    writeln!(log_file, "=== B+Tree Stress Test & Performance Analysis ===").unwrap();
    writeln!(log_file, "Dataset: {num_items} items").unwrap();
    writeln!(log_file, "Insert benchmark runs: {insert_runs}").unwrap();
    
    // ----------------------------------------------------------------
    // Phase 1: Batch Insert (Sequential Keys)
    // ----------------------------------------------------------------
    writeln!(log_file, "\n[Phase 1] Batch Insert (Sequential, Multi-Run)...").unwrap();
    let start_gen = Instant::now();

    let mut value_pool = Vec::with_capacity(value_pool_size);
    for _ in 0..value_pool_size {
        value_pool.push(random_string(small_val_len));
    }
    writeln!(
        log_file,
        "Value pool generation time ({} templates): {:.2?}",
        value_pool_size,
        start_gen.elapsed()
    )
    .unwrap();

    let mut insert_throughputs = Vec::with_capacity(insert_runs);
    let mut final_tree: Option<BPlusTree<u32, String>> = None;
    for run_idx in 0..insert_runs {
        let mut run_tree = BPlusTree::<u32, String>::new();
        let start = Instant::now();
        for i in 0..num_items {
            let value = value_pool[i % value_pool_size].clone();
            run_tree.insert(i as u32, value);
        }
        let run_duration = start.elapsed();
        let run_throughput = num_items as f64 / run_duration.as_secs_f64();
        insert_throughputs.push(run_throughput);
        writeln!(
            log_file,
            "Run {}: {:.2?} ({:.0} ops/sec)",
            run_idx + 1,
            run_duration,
            run_throughput
        )
        .unwrap();
        final_tree = Some(run_tree);
    }
    let mut sorted_insert_throughputs = insert_throughputs.clone();
    sorted_insert_throughputs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let insert_mean = insert_throughputs.iter().sum::<f64>() / insert_throughputs.len() as f64;
    let insert_median = percentile_nearest_rank(&sorted_insert_throughputs, 0.5);
    let insert_p95 = percentile_nearest_rank(&sorted_insert_throughputs, 0.95);
    let insert_min = *sorted_insert_throughputs.first().unwrap_or(&0.0);
    let insert_max = *sorted_insert_throughputs.last().unwrap_or(&0.0);
    writeln!(
        log_file,
        "Insert Throughput Summary (ops/sec): mean={:.0}, median={:.0}, p95={:.0}, min={:.0}, max={:.0}",
        insert_mean,
        insert_median,
        insert_p95,
        insert_min,
        insert_max
    )
    .unwrap();
    let mut tree = final_tree.expect("final insert run should produce a tree");

    // Prepare query keys
    let mut query_keys: Vec<u32> = (0..num_items as u32).collect();
    query_keys.shuffle(&mut rand::rng());
    let query_subset_mem = &query_keys[0..query_count];

    // ----------------------------------------------------------------
    // Phase 1b: Memory-Only Random Query (Before storing)
    // ----------------------------------------------------------------
    writeln!(log_file, "\n[Phase 1b] Memory-Only Random Query ({query_count} items)...").unwrap();
    let start = Instant::now();
    for k in query_subset_mem {
        let _ = tree.query(k);
    }
    let duration = start.elapsed();
    writeln!(log_file, "Time: {duration:.2?}").unwrap();
    writeln!(log_file, "Throughput: {:.0} ops/sec", query_count as f64 / duration.as_secs_f64()).unwrap();

    // ----------------------------------------------------------------
    // Phase 1c: Store to disk
    // ----------------------------------------------------------------
    writeln!(log_file, "\n[Phase 1c] Store to disk...").unwrap();
    let start = Instant::now();
    tree.store(&filepath).unwrap();
    drop(tree);
    let duration = start.elapsed();
    writeln!(log_file, "Write Time: {duration:.2?}").unwrap();
    let size_phase1 = std::fs::metadata(&filepath).unwrap().len();
    writeln!(log_file, "File Size: {:.2} MB", size_phase1 as f64 / 1024.0 / 1024.0).unwrap();

    // ----------------------------------------------------------------
    // Phase 2: Random Query (Disk-based)
    // ----------------------------------------------------------------
    writeln!(log_file, "\n[Phase 2] Random Query Disk-based ({query_count} items)...").unwrap();
    let mut query = BPlusTreeQuery::<u32, String>::try_new(&filepath).unwrap();
    let start = Instant::now();
    for k in query_subset_mem {
        let _ = query.query_zero_copy(k).unwrap();
    }
    let duration = start.elapsed();
    writeln!(log_file, "Time: {duration:.2?}").unwrap();
    writeln!(log_file, "Throughput: {:.0} ops/sec", query_count as f64 / duration.as_secs_f64()).unwrap();
    
    // ----------------------------------------------------------------
    // Phase 3: Batch Update (In-Place Packed)
    // ----------------------------------------------------------------
    writeln!(log_file, "\n[Phase 3] Batch Update (In-Place Packed)...").unwrap();
    let update_count = 5000;
    let update_subset = &query_keys[0..update_count];
    let updates: Vec<(u32, String)> = update_subset.iter()
        .map(|&k| (k, random_string(small_val_len)))
        .collect();
    let update_refs: Vec<(&u32, &String)> = updates.iter().map(|(k,v)| (k,v)).collect();
    let mut tree_updater = BPlusTreeUpdate::<u32, String>::try_new(&filepath).unwrap();
    let start = Instant::now();
    tree_updater.update_batch(&update_refs).unwrap();
    let duration = start.elapsed();
    writeln!(log_file, "Time: {duration:.2?}").unwrap();
    writeln!(log_file, "Throughput: {:.0} ops/sec", update_count as f64 / duration.as_secs_f64()).unwrap();
    let size_phase3 = std::fs::metadata(&filepath).unwrap().len();
    writeln!(log_file, "File Size: {:.2} MB", size_phase3 as f64 / 1024.0 / 1024.0).unwrap();
    
    // ----------------------------------------------------------------
    // Phase 4: Batch Update (Promoting Packed -> Single)
    // ----------------------------------------------------------------
    writeln!(log_file, "\n[Phase 4] Batch Update (Promotion to Single)...").unwrap();
    let updates_prom: Vec<(u32, String)> = update_subset.iter()
        .map(|&k| (k, random_string(large_val_len)))
        .collect();
    let update_refs_prom: Vec<(&u32, &String)> = updates_prom.iter().map(|(k,v)| (k,v)).collect();
    let start = Instant::now();
    tree_updater.update_batch(&update_refs_prom).unwrap();
    let duration = start.elapsed();
    writeln!(log_file, "Time: {duration:.2?}").unwrap();
    writeln!(log_file, "Throughput: {:.0} ops/sec", update_count as f64 / duration.as_secs_f64()).unwrap();
    let size_phase4 = std::fs::metadata(&filepath).unwrap().len();
    writeln!(log_file, "File Size: {:.2} MB", size_phase4 as f64 / 1024.0 / 1024.0).unwrap();
    drop(tree_updater);

    // ----------------------------------------------------------------
    // Phase 5: Compaction
    // ----------------------------------------------------------------
    writeln!(log_file, "\n[Phase 5] Compaction...").unwrap();
    let mut tree_updater = BPlusTreeUpdate::<u32, String>::try_new(&filepath).unwrap();
    let start = Instant::now();
    tree_updater.compact(&filepath).unwrap();
    let duration = start.elapsed();
    writeln!(log_file, "Time: {duration:.2?}").unwrap();
    let size_phase5 = std::fs::metadata(&filepath).unwrap().len();
    writeln!(
        log_file,
        "File Size: {:.2} MB (Reduction: {:.2} MB)",
        size_phase5 as f64 / 1024.0 / 1024.0,
        (size_phase4 as i64 - size_phase5 as i64) as f64 / 1024.0 / 1024.0
    )
    .unwrap();
    drop(tree_updater);

    // ----------------------------------------------------------------
    // Phase 6: Full Tree Load and In-Memory Query
    // ----------------------------------------------------------------
    writeln!(log_file, "\n[Phase 6] Full Tree Load (Memory-Only Read)...").unwrap();
    let start = Instant::now();
    let tree_mem = BPlusTree::<u32, String>::load(&filepath).unwrap();
    let load_duration = start.elapsed();
    writeln!(log_file, "Load Time: {load_duration:.2?}").unwrap();

    let start = Instant::now();
    for k in query_subset_mem {
        let _ = tree_mem.query(k);
    }
    let query_duration = start.elapsed();
    writeln!(log_file, "In-Memory Query Time: {query_duration:.2?}").unwrap();
    writeln!(log_file, "Throughput: {:.0} ops/sec", query_count as f64 / query_duration.as_secs_f64()).unwrap();
    drop(tree_mem);

    // ----------------------------------------------------------------
    // Phase 7: Concurrent Readers + Writers (Disk-based)
    // ----------------------------------------------------------------
    writeln!(log_file, "\n[Phase 7] Concurrent Readers + Writers (Disk-based)...").unwrap();
    let reader_threads = 8usize;
    let writer_threads = 4usize;
    let reader_ops_per_thread = 40_000u64;
    let writer_batches_per_thread = 48u64;
    let writer_batch_size = 128usize;
    let num_items_u32 = num_items as u32;
    let expected_writer_batches = writer_batches_per_thread * writer_threads as u64;
    let expected_writer_updates = expected_writer_batches * writer_batch_size as u64;

    writeln!(
        log_file,
        "Readers: {reader_threads}, Writers: {writer_threads}, Reader Ops/Thread: {reader_ops_per_thread}, Writer Batches/Thread: {writer_batches_per_thread}, Batch Size: {writer_batch_size}"
    )
    .unwrap();

    let start_barrier = Arc::new(Barrier::new(reader_threads + writer_threads + 1));
    let shared_path = Arc::new(filepath.clone());
    let wal_writer = Arc::new(
        BPlusTreeSerialWriter::<u32, String>::new(filepath.as_path(), FlushPolicy::Batch).unwrap(),
    );
    let small_payload = "s".repeat(small_val_len.saturating_sub(24));
    let large_payload = "L".repeat(large_val_len.saturating_sub(24));

    let mut reader_handles = Vec::with_capacity(reader_threads);
    for reader_id in 0..reader_threads {
        let barrier = Arc::clone(&start_barrier);
        let path = Arc::clone(&shared_path);
        reader_handles.push(thread::spawn(move || -> (u64, u64, Duration) {
            let mut query = BPlusTreeQuery::<u32, String>::try_new(path.as_path()).unwrap();
            let mut prng_state = 0x9E37_79B9_7F4A_7C15u64 ^ ((reader_id as u64 + 1).wrapping_mul(0xBF58_476D_1CE4_E5B9));
            barrier.wait();
            let started = Instant::now();
            let mut ops = 0u64;
            let mut misses = 0u64;

            for _ in 0..reader_ops_per_thread {
                let key = (lcg_next(&mut prng_state) as u32) % num_items_u32;
                if query.query_zero_copy(&key).unwrap().is_none() {
                    misses += 1;
                }
                ops += 1;
            }

            (ops, misses, started.elapsed())
        }));
    }

    let mut writer_handles = Vec::with_capacity(writer_threads);
    for writer_id in 0..writer_threads {
        let barrier = Arc::clone(&start_barrier);
        let wal_writer = Arc::clone(&wal_writer);
        let small_payload_local = small_payload.clone();
        let large_payload_local = large_payload.clone();
        writer_handles.push(thread::spawn(move || -> (u64, u64, u64, Duration) {
            let mut prng_state = 0xD6E8_FEB8_6659_FD93u64 ^ ((writer_id as u64 + 1).wrapping_mul(0x94D0_49BB_1331_11EB));
            barrier.wait();
            let started = Instant::now();

            let retries = 0u64;
            let mut applied_batches = 0u64;
            let mut applied_updates = 0u64;

            for batch_idx in 0..writer_batches_per_thread {
                let mut owned_batch = Vec::with_capacity(writer_batch_size);
                for _ in 0..writer_batch_size {
                    let key = (lcg_next(&mut prng_state) as u32) % num_items_u32;
                    let variant = lcg_next(&mut prng_state);
                    let payload = if (variant & 0b1111) == 0 {
                        &large_payload_local
                    } else {
                        &small_payload_local
                    };
                    owned_batch.push((key, format!("w{writer_id}_b{batch_idx}_k{key}_{payload}")));
                }
                let batch_refs: Vec<(&u32, &String)> = owned_batch.iter().map(|(k, v)| (k, v)).collect();
                // Pre-serialize + compress outside the file lock.
                let prepared = BPlusTreeUpdate::<u32, String>::prepare_upsert_batch(&batch_refs).unwrap();
                wal_writer.upsert_prepared(prepared).unwrap();
                applied_batches += 1;
                applied_updates += writer_batch_size as u64;
            }

            (applied_updates, applied_batches, retries, started.elapsed())
        }));
    }

    let concurrent_start = Instant::now();
    start_barrier.wait();

    let mut total_reader_ops = 0u64;
    let mut total_reader_misses = 0u64;
    let mut total_reader_time = Duration::ZERO;
    let mut max_reader_duration = Duration::ZERO;
    for handle in reader_handles {
        let (ops, misses, elapsed) = handle.join().unwrap();
        total_reader_ops += ops;
        total_reader_misses += misses;
        total_reader_time += elapsed;
        if elapsed > max_reader_duration {
            max_reader_duration = elapsed;
        }
    }

    let mut total_writer_updates = 0u64;
    let mut total_writer_batches = 0u64;
    let mut total_writer_retries = 0u64;
    let mut total_writer_time = Duration::ZERO;
    let mut max_writer_duration = Duration::ZERO;
    for handle in writer_handles {
        let (updates, batches, retries, elapsed) = handle.join().unwrap();
        total_writer_updates += updates;
        total_writer_batches += batches;
        total_writer_retries += retries;
        total_writer_time += elapsed;
        if elapsed > max_writer_duration {
            max_writer_duration = elapsed;
        }
    }
    wal_writer.commit().unwrap();
    wal_writer.shutdown().unwrap();

    let concurrent_duration = concurrent_start.elapsed();
    let reader_avg_latency_us = total_reader_time.as_secs_f64() * 1_000_000.0 / total_reader_ops as f64;
    let writer_avg_latency_us = total_writer_time.as_secs_f64() * 1_000_000.0 / total_writer_updates as f64;

    writeln!(log_file, "Wall Time: {concurrent_duration:.2?}").unwrap();
    writeln!(
        log_file,
        "Reader Ops: {total_reader_ops} (misses: {total_reader_misses}), Throughput: {:.0} ops/sec, Avg Latency: {:.2}us/op, Slowest Reader: {max_reader_duration:.2?}",
        total_reader_ops as f64 / concurrent_duration.as_secs_f64(),
        reader_avg_latency_us
    )
    .unwrap();
    writeln!(
        log_file,
        "Writer Updates: {total_writer_updates} (batches: {total_writer_batches}), Throughput: {:.0} updates/sec, Avg Latency: {:.2}us/update, Lock Retries: {total_writer_retries} (single-writer queue), Slowest Writer: {max_writer_duration:.2?}",
        total_writer_updates as f64 / concurrent_duration.as_secs_f64(),
        writer_avg_latency_us
    )
    .unwrap();

    assert_eq!(total_writer_batches, expected_writer_batches, "all writer batches must be committed");
    assert_eq!(total_writer_updates, expected_writer_updates, "all writer updates must be committed");
    assert_eq!(total_reader_misses, 0, "all reader queries should resolve during concurrent load");

    // ----------------------------------------------------------------
    // Phase 7b: Post-Concurrency Verification
    // ----------------------------------------------------------------
    writeln!(log_file, "\n[Phase 7b] Post-Concurrency Verification...").unwrap();
    let mut verify_query = BPlusTreeQuery::<u32, String>::try_new(&filepath).unwrap();
    let verification_samples = 20_000u64;
    let mut verification_misses = 0u64;
    let mut verify_state = 0x243F_6A88_85A3_08D3u64;
    let verify_start = Instant::now();
    for _ in 0..verification_samples {
        let key = (lcg_next(&mut verify_state) as u32) % num_items_u32;
        if verify_query.query(&key).unwrap().is_none() {
            verification_misses += 1;
        }
    }
    let verify_duration = verify_start.elapsed();
    writeln!(
        log_file,
        "Verification Time: {verify_duration:.2?}, misses: {verification_misses}/{verification_samples}"
    )
    .unwrap();
    assert_eq!(verification_misses, 0, "post-concurrency verification failed");
}
