use serde::{Deserialize, Serialize};
use std::{
    io::{self, BufRead},
    path::Path,
};
use tuliprox_btree::{BPlusTree, BPlusTreeQuery};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct ImdbRow {
    pub id: u32,
    pub votes: u32,
    pub rating_x10: u16,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImdbRating {
    pub rating: f32,
    pub votes: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParseStats {
    pub total_rows: usize,
    pub valid_rows: usize,
    pub skipped_rows: usize,
    pub duplicate_rows: usize,
}

/// The B+Tree value: one `IMDb` rating, packed to keep the on-disk entry small.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImdbEntry {
    pub rating_x10: u16,
    pub votes: u32,
}

impl From<&ImdbRow> for ImdbEntry {
    fn from(row: &ImdbRow) -> Self { Self { rating_x10: row.rating_x10, votes: row.votes } }
}

impl ImdbEntry {
    fn rating(self) -> ImdbRating { ImdbRating { rating: f32::from(self.rating_x10) / 10.0, votes: self.votes } }
}

/// The in-memory tree a download builds and stores. Also the value a caller
/// seeds for tests.
pub type ImdbTree = BPlusTree<u32, ImdbEntry>;

/// Publishes `tree` as the ratings database at `path`.
///
/// `BPlusTree::store` writes a temporary file, verifies it and renames it into
/// place, so a reader never sees a half-written table.
pub fn store_imdb_tree(tree: &mut ImdbTree, path: &Path) -> io::Result<()> { tree.store(path).map(|_| ()) }

/// Number of stored ratings without loading the table, used by the download's
/// shrink guard.
pub fn imdb_ratings_row_count(path: &Path) -> io::Result<usize> {
    let mut query = BPlusTreeQuery::<u32, ImdbEntry>::try_new(path)?;
    query.len().map_err(io::Error::other)
}

/// One run's view of the ratings table.
///
/// `Memory` holds the rows read from disk once for the run; `Disk` queries the
/// B+Tree for every lookup and never loads the table. Both are dropped when the
/// run ends, so no table memory outlives an update.
pub enum ImdbLookup {
    Memory(Vec<ImdbRow>),
    Disk { tree: BPlusTreeQuery<u32, ImdbEntry>, error_logged: bool },
}

impl ImdbLookup {
    /// Opens the ratings table for one run; `Ok(None)` when nothing is stored.
    ///
    /// The caller holds the file read lock for this call, because the open may
    /// read pages (and, in memory mode, the whole tree).
    pub fn open(path: &Path, disk_based: bool) -> io::Result<Option<Self>> {
        if !path.try_exists()? {
            return Ok(None);
        }
        if disk_based {
            return Ok(Some(Self::Disk {
                tree: BPlusTreeQuery::<u32, ImdbEntry>::try_new(path)?,
                error_logged: false,
            }));
        }
        let query = BPlusTreeQuery::<u32, ImdbEntry>::try_new(path)?;
        let mut rows = Vec::new();
        for entry in query.disk_iter() {
            let (id, value) = entry?;
            rows.push(ImdbRow { id, votes: value.votes, rating_x10: value.rating_x10 });
        }
        // The `Vec` doubles on growth and would otherwise keep up to twice the
        // table's size for the whole run.
        rows.shrink_to_fit();
        Ok(Some(Self::Memory(rows)))
    }

    /// Memory-only constructor for tests and callers that already hold the rows.
    pub fn from_rows(rows: Vec<ImdbRow>) -> Self { Self::Memory(rows) }

    pub fn lookup(&mut self, id: u32) -> Option<ImdbRating> {
        match self {
            Self::Memory(rows) => {
                let idx = rows.binary_search_by_key(&id, |r| r.id).ok()?;
                Some(ImdbRating { rating: f32::from(rows[idx].rating_x10) / 10.0, votes: rows[idx].votes })
            }
            Self::Disk { tree, error_logged } => match tree.query(&id) {
                Ok(found) => found.map(ImdbEntry::rating),
                Err(err) => {
                    // One warning per run: a broken table must not spam a line
                    // for every playlist item.
                    if !*error_logged {
                        *error_logged = true;
                        log::warn!("IMDb ratings disk lookup failed ({err}); treating the table as unrated");
                    }
                    None
                }
            },
        }
    }
}

#[derive(Debug, Default)]
pub struct ImdbIndexBuilder {
    tree: ImdbTree,
    stats: ParseStats,
    first_line: bool,
}

impl ImdbIndexBuilder {
    pub fn new() -> Self { Self { tree: ImdbTree::new(), stats: ParseStats::default(), first_line: true } }

    pub fn push_line(&mut self, line: &str) {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if self.first_line {
            self.first_line = false;
            if trimmed.starts_with("tconst") {
                return;
            }
        }

        if trimmed.is_empty() {
            return;
        }

        self.stats.total_rows += 1;
        if let Some(row) = parse_row(trimmed) {
            // Inserting an existing id replaces the value, which is the
            // "duplicate id: keep the last" rule. The tree's own `insert`
            // returns nothing, so the cheaper `query` tells us a row was replaced.
            if self.tree.query(&row.id).is_some() {
                self.stats.duplicate_rows += 1;
            }
            self.tree.insert(row.id, ImdbEntry::from(&row));
        } else {
            self.stats.skipped_rows += 1;
        }
    }

    /// The parsed tree plus the parse statistics. Rows are inserted as they are
    /// parsed, so no row vector exists next to the tree.
    pub fn finish(mut self) -> (ImdbTree, ParseStats) {
        self.stats.valid_rows = self.tree.len();
        (self.tree, self.stats)
    }

    pub fn stats(&self) -> &ParseStats { &self.stats }
}

pub fn parse_tsv<R: BufRead>(mut reader: R) -> io::Result<(ImdbTree, ParseStats)> {
    let mut builder = ImdbIndexBuilder::new();
    let mut line = String::new();

    while reader.read_line(&mut line)? > 0 {
        builder.push_line(&line);
        line.clear();
    }

    Ok(builder.finish())
}

fn parse_row(line: &str) -> Option<ImdbRow> {
    let mut cols = line.split('\t');
    let tconst = cols.next()?;
    let rating_str = cols.next()?;
    let votes_str = cols.next()?;
    if cols.next().is_some() {
        return None;
    }

    if !tconst.starts_with("tt") {
        return None;
    }
    let id_str = &tconst[2..];
    if id_str.is_empty() {
        return None;
    }
    let id = id_str.parse::<u32>().ok()?;

    let rating_x10 = parse_rating_x10(rating_str)?;
    let votes = votes_str.parse::<u32>().ok()?;

    Some(ImdbRow { id, votes, rating_x10 })
}

fn parse_rating_x10(s: &str) -> Option<u16> {
    let s = s.trim();
    if s.is_empty() || s == r"\N" {
        return None;
    }

    if let Some((integer_part, frac_part)) = s.split_once('.') {
        if integer_part.is_empty() || frac_part.is_empty() {
            return None;
        }
        if !frac_part.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let int_val: u16 = integer_part.parse().ok()?;
        if int_val > 10 {
            return None;
        }

        let first_char = frac_part.chars().next()?;
        let frac_digit = u16::from(first_char as u8 - b'0');

        if int_val == 10 && (frac_digit != 0 || frac_part.len() > 1 && frac_part.chars().any(|c| c != '0')) {
            return None;
        }

        let val = int_val * 10 + frac_digit;
        if val > 100 {
            return None;
        }
        Some(val)
    } else {
        let int_val: u16 = s.parse().ok()?;
        if int_val > 10 {
            return None;
        }
        Some(int_val * 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn tree_lookup(tree: &ImdbTree, id: u32) -> Option<ImdbRating> { tree.query(&id).copied().map(ImdbEntry::rating) }

    fn tree_of(rows: Vec<ImdbRow>) -> ImdbTree {
        let mut tree = ImdbTree::new();
        for row in rows {
            tree.insert(row.id, ImdbEntry::from(&row));
        }
        tree
    }

    fn store_tree(tree: &mut ImdbTree, path: &Path) { store_imdb_tree(tree, path).expect("store should succeed") }

    #[test]
    fn test_parse_tsv_header_and_valid_rows() {
        let tsv = "tconst\taverageRating\tnumVotes\ntt0000001\t5.7\t2112\ntt0000002\t8.1\t1500\n";
        let (tree, stats) = parse_tsv(Cursor::new(tsv)).expect("parse_tsv should succeed");
        assert_eq!(stats.total_rows, 2);
        assert_eq!(stats.valid_rows, 2);
        assert_eq!(stats.skipped_rows, 0);
        assert_eq!(stats.duplicate_rows, 0);
        assert_eq!(tree.len(), 2);

        let r1 = tree_lookup(&tree, 1).expect("id 1 found");
        assert_eq!(r1.rating, 5.7);
        assert_eq!(r1.votes, 2112);

        let r2 = tree_lookup(&tree, 2).expect("id 2 found");
        assert_eq!(r2.rating, 8.1);
        assert_eq!(r2.votes, 1500);
    }

    #[test]
    fn test_parse_tsv_skips_malformed_rows() {
        let tsv = "\
tconst\taverageRating\tnumVotes
tt0000001\t\\N\t100
tt0000002\t7.5\t\\N
tt0000003\tbad\t100
tt0000004\t7.5\tbad
0000005\t7.5\t100
nm0000006\t7.5\t100
tt0000007\t11.0\t100
tt0000008\t-1.0\t100
tt0000009\t7.5
tt0000010\t7.5\t100\textra
tt0000011\t8.0\t500
";
        let (tree, stats) = parse_tsv(Cursor::new(tsv)).expect("parse_tsv should succeed");
        assert_eq!(stats.total_rows, 11);
        assert_eq!(stats.skipped_rows, 10);
        assert_eq!(stats.valid_rows, 1);
        assert_eq!(tree.len(), 1);
        assert!(tree_lookup(&tree, 11).is_some());
    }

    #[test]
    fn test_parse_tsv_duplicates_keep_last() {
        let tsv = "\
tconst\taverageRating\tnumVotes
tt0000001\t5.0\t100
tt0000002\t6.0\t200
tt0000001\t7.5\t300
tt0000001\t8.2\t400
";
        let (tree, stats) = parse_tsv(Cursor::new(tsv)).expect("parse_tsv should succeed");
        assert_eq!(stats.total_rows, 4);
        assert_eq!(stats.duplicate_rows, 2);
        assert_eq!(stats.valid_rows, 2);
        assert_eq!(tree.len(), 2);

        let r1 = tree_lookup(&tree, 1).expect("id 1 found");
        assert_eq!(r1.rating, 8.2);
        assert_eq!(r1.votes, 400);

        let r2 = tree_lookup(&tree, 2).expect("id 2 found");
        assert_eq!(r2.rating, 6.0);
        assert_eq!(r2.votes, 200);
    }

    #[test]
    fn test_parse_tsv_ratings_boundary_values() {
        let tsv = "\
tconst\taverageRating\tnumVotes
tt0000001\t10.0\t100
tt0000002\t0.0\t50
tt0000003\t10\t200
tt0000004\t0\t30
";
        let (tree, stats) = parse_tsv(Cursor::new(tsv)).expect("parse_tsv should succeed");
        assert_eq!(stats.valid_rows, 4);
        assert_eq!(tree_lookup(&tree, 1).map(|r| r.rating), Some(10.0));
        assert_eq!(tree_lookup(&tree, 2).map(|r| r.rating), Some(0.0));
        assert_eq!(tree_lookup(&tree, 3).map(|r| r.rating), Some(10.0));
        assert_eq!(tree_lookup(&tree, 4).map(|r| r.rating), Some(0.0));
    }

    #[test]
    fn test_parse_tsv_crlf_line_endings() {
        let tsv = "tconst\taverageRating\tnumVotes\r\ntt0000001\t5.7\t2112\r\ntt0000002\t8.1\t1500\r\n";
        let (tree, stats) = parse_tsv(Cursor::new(tsv)).expect("parse_tsv should succeed");
        assert_eq!(stats.valid_rows, 2);
        assert_eq!(tree_lookup(&tree, 1).map(|r| r.rating), Some(5.7));
        assert_eq!(tree_lookup(&tree, 2).map(|r| r.rating), Some(8.1));
    }

    #[test]
    fn test_lookup_hit_miss_first_last() {
        let tsv = "\
tconst\taverageRating\tnumVotes
tt0000100\t5.0\t10
tt0000010\t6.0\t20
tt0001000\t7.0\t30
tt0000001\t8.0\t40
tt0000500\t9.0\t50
";
        let (tree, _) = parse_tsv(Cursor::new(tsv)).expect("parse_tsv should succeed");

        // First
        let first = tree_lookup(&tree, 1).expect("first element");
        assert_eq!(first.rating, 8.0);
        assert_eq!(first.votes, 40);

        // Last
        let last = tree_lookup(&tree, 1000).expect("last element");
        assert_eq!(last.rating, 7.0);
        assert_eq!(last.votes, 30);

        // Hit
        let mid = tree_lookup(&tree, 100).expect("middle element");
        assert_eq!(mid.rating, 5.0);
        assert_eq!(mid.votes, 10);

        // Miss
        assert!(tree_lookup(&tree, 0).is_none());
        assert!(tree_lookup(&tree, 99).is_none());
        assert!(tree_lookup(&tree, 9999).is_none());
    }

    fn rows() -> Vec<ImdbRow> {
        vec![
            ImdbRow { id: 1, votes: 100, rating_x10: 75 },
            ImdbRow { id: 50, votes: 200, rating_x10: 82 },
            ImdbRow { id: 1000, votes: 300, rating_x10: 91 },
        ]
    }

    fn assert_lookups(lookup: &mut ImdbLookup) {
        assert_eq!(lookup.lookup(1), Some(ImdbRating { rating: 7.5, votes: 100 }));
        assert_eq!(lookup.lookup(50), Some(ImdbRating { rating: 8.2, votes: 200 }));
        assert_eq!(lookup.lookup(1000), Some(ImdbRating { rating: 9.1, votes: 300 }));
        assert_eq!(lookup.lookup(0), None);
        assert_eq!(lookup.lookup(2), None);
        assert_eq!(lookup.lookup(9999), None);
    }

    #[test]
    fn test_btree_round_trip_memory_and_disk() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("imdb_ratings.db");

        store_tree(&mut tree_of(rows()), &db_path);

        let mut memory = ImdbLookup::open(&db_path, false).expect("open memory").expect("table is Some");
        assert!(matches!(memory, ImdbLookup::Memory(_)));
        assert_lookups(&mut memory);

        let mut disk = ImdbLookup::open(&db_path, true).expect("open disk").expect("table is Some");
        assert!(matches!(disk, ImdbLookup::Disk { .. }));
        assert_lookups(&mut disk);
    }

    #[test]
    fn test_open_missing_file_is_ok_none() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("non_existent.db");
        assert!(ImdbLookup::open(&db_path, false).expect("missing file is Ok(None)").is_none());
        assert!(ImdbLookup::open(&db_path, true).expect("missing file is Ok(None)").is_none());
    }

    #[test]
    fn test_store_replaces_previous_table_atomically() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("imdb_ratings.db");

        store_tree(&mut tree_of(rows()), &db_path);
        store_tree(&mut tree_of(vec![ImdbRow { id: 7, votes: 10, rating_x10: 42 }]), &db_path);

        let mut lookup = ImdbLookup::open(&db_path, false).expect("open").expect("table is Some");
        assert_eq!(lookup.lookup(7), Some(ImdbRating { rating: 4.2, votes: 10 }));
        assert_eq!(lookup.lookup(1), None);
    }

    #[test]
    fn test_memory_lookup_matches_index() {
        let mut lookup = ImdbLookup::from_rows(rows());
        assert_lookups(&mut lookup);
    }
}
