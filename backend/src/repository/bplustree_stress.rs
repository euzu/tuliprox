use super::bplustree::{BPlusTree, BPlusTreeUpdate, BPlusTreeQuery};
use rand::prelude::*;
use rand::distr::Alphanumeric;
use std::time::Instant;
use tempfile::NamedTempFile;

// Run with:  `cargo test --release --package tuliprox -- stress_test_bplustree -- --nocapture`

// Helper to generate random string
fn random_string(len: usize) -> String {
    rand::rng().sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

#[test]
fn stress_test_bplustree() {
    let temp_file = NamedTempFile::new().unwrap();
    let filepath = temp_file.path().to_path_buf();
    let log_path = std::path::Path::new("/projects/tuliprox/stress_results.txt");
    let mut log_file = std::fs::File::create(log_path).unwrap();
    use std::io::Write;

    // Config
    let num_items = 500_000;
    let query_count = 50_000;
    let small_val_len = 50;
    let large_val_len = 500; // Larger than packed limit (256)
    
    writeln!(log_file, "=== B+Tree Stress Test & Performance Analysis ===").unwrap();
    writeln!(log_file, "Dataset: {} items", num_items).unwrap();
    
    // ----------------------------------------------------------------
    // Phase 1: Batch Insert (Sequential Keys)
    // ----------------------------------------------------------------
    writeln!(log_file, "\n[Phase 1] Batch Insert (Sequential)...").unwrap();
    let mut tree = BPlusTree::<u32, String>::new();
    let start_gen = Instant::now();
    
    // Generate data
    let mut initial_data = Vec::with_capacity(num_items);
    for i in 0..num_items {
        initial_data.push((i as u32, random_string(small_val_len)));
    }
    writeln!(log_file, "Generation Time: {:.2?}", start_gen.elapsed()).unwrap();

    let start = Instant::now();
    for (k, v) in initial_data {
        tree.insert(k, v);
    }
    let insert_duration = start.elapsed();
    writeln!(log_file, "Insert Time: {:.2?}", insert_duration).unwrap();
    writeln!(log_file, "Throughput: {:.0} ops/sec", num_items as f64 / insert_duration.as_secs_f64()).unwrap();

    // Prepare query keys
    let mut query_keys: Vec<u32> = (0..num_items as u32).collect();
    query_keys.shuffle(&mut rand::rng());
    let query_subset_mem = &query_keys[0..query_count];

    // ----------------------------------------------------------------
    // Phase 1b: Memory-Only Random Query (Before storing)
    // ----------------------------------------------------------------
    writeln!(log_file, "\n[Phase 1b] Memory-Only Random Query ({} items)...", query_count).unwrap();
    let start = Instant::now();
    for k in query_subset_mem {
        let _ = tree.query(k);
    }
    let duration = start.elapsed();
    writeln!(log_file, "Time: {:.2?}", duration).unwrap();
    writeln!(log_file, "Throughput: {:.0} ops/sec", query_count as f64 / duration.as_secs_f64()).unwrap();

    // ----------------------------------------------------------------
    // Phase 1c: Store to disk
    // ----------------------------------------------------------------
    writeln!(log_file, "\n[Phase 1c] Store to disk...").unwrap();
    let start = Instant::now();
    tree.store(&filepath).unwrap();
    drop(tree);
    let duration = start.elapsed();
    writeln!(log_file, "Write Time: {:.2?}", duration).unwrap();
    let size_phase1 = std::fs::metadata(&filepath).unwrap().len();
    writeln!(log_file, "File Size: {:.2} MB", size_phase1 as f64 / 1024.0 / 1024.0).unwrap();

    // ----------------------------------------------------------------
    // Phase 2: Random Query (Disk-based)
    // ----------------------------------------------------------------
    writeln!(log_file, "\n[Phase 2] Random Query Disk-based ({} items)...", query_count).unwrap();
    let mut query = BPlusTreeQuery::<u32, String>::try_new(&filepath).unwrap();
    let start = Instant::now();
    for k in query_subset_mem {
        let _ = query.query_zero_copy(k).unwrap();
    }
    let duration = start.elapsed();
    writeln!(log_file, "Time: {:.2?}", duration).unwrap();
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
    let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&filepath).unwrap();
    let start = Instant::now();
    updater.update_batch(&update_refs).unwrap();
    let duration = start.elapsed();
    writeln!(log_file, "Time: {:.2?}", duration).unwrap();
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
    updater.update_batch(&update_refs_prom).unwrap();
    let duration = start.elapsed();
    writeln!(log_file, "Time: {:.2?}", duration).unwrap();
    writeln!(log_file, "Throughput: {:.0} ops/sec", update_count as f64 / duration.as_secs_f64()).unwrap();
    let size_phase4 = std::fs::metadata(&filepath).unwrap().len();
    writeln!(log_file, "File Size: {:.2} MB", size_phase4 as f64 / 1024.0 / 1024.0).unwrap();
    drop(updater);

    // ----------------------------------------------------------------
    // Phase 5: Compaction
    // ----------------------------------------------------------------
    writeln!(log_file, "\n[Phase 5] Compaction...").unwrap();
    let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&filepath).unwrap();
    let start = Instant::now();
    updater.compact(&filepath).unwrap();
    let duration = start.elapsed();
    writeln!(log_file, "Time: {:.2?}", duration).unwrap();
    let size_phase5 = std::fs::metadata(&filepath).unwrap().len();
    writeln!(log_file, "File Size: {:.2} MB (Reduction: {:.2} MB)", size_phase5 as f64 / 1024.0 / 1024.0, (size_phase4 as i64 - size_phase5 as i64) as f64 / 1024.0 / 1024.0).unwrap();
    drop(updater);

    // ----------------------------------------------------------------
    // Phase 6: Full Tree Load and In-Memory Query
    // ----------------------------------------------------------------
    writeln!(log_file, "\n[Phase 6] Full Tree Load (Memory-Only Read)...").unwrap();
    let start = Instant::now();
    let tree_mem = BPlusTree::<u32, String>::load(&filepath).unwrap();
    let load_duration = start.elapsed();
    writeln!(log_file, "Load Time: {:.2?}", load_duration).unwrap();

    let start = Instant::now();
    for k in query_subset_mem {
        let _ = tree_mem.query(k);
    }
    let query_duration = start.elapsed();
    writeln!(log_file, "In-Memory Query Time: {:.2?}", query_duration).unwrap();
    writeln!(log_file, "Throughput: {:.0} ops/sec", query_count as f64 / query_duration.as_secs_f64()).unwrap();
}
