//! `IMDb` defaults: dataset URL, minimum votes.

pub const DEFAULT_IMDB_URL: &str = "https://datasets.imdbws.com/title.ratings.tsv.gz";
pub const DEFAULT_IMDB_MIN_VOTES: u32 = 1000;

pub fn default_imdb_url() -> String { DEFAULT_IMDB_URL.to_string() }

pub fn is_default_imdb_url(v: &String) -> bool { v == DEFAULT_IMDB_URL }

pub const fn default_imdb_min_votes() -> u32 { DEFAULT_IMDB_MIN_VOTES }

pub const fn is_default_imdb_min_votes(v: &u32) -> bool { *v == DEFAULT_IMDB_MIN_VOTES }
