use rand::distr::Alphanumeric;
use rand::Rng;
use regex::Regex;
use std::fs;
use walkdir::{DirEntry, WalkDir};

pub fn generate_random_string(length: usize) -> String {
    let rng = rand::thread_rng();
    let random_string: String = rng
        .sample_iter(&Alphanumeric)
        .take(length)
        .map(char::from)
        .collect();
    random_string
}

pub fn get_files(local_dir: &str, filter_pattern: &str) -> Vec<DirEntry> {
    WalkDir::new(local_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|entry| {
            let source_path = entry.path();
            if source_path.to_str().unwrap().contains("/build/") {
                return None;
            }

            if !source_path.is_file() {
                return None;
            }
            let relative_path = source_path.strip_prefix(local_dir).unwrap();
            let re = Regex::new(filter_pattern).unwrap();
            if re.is_match(relative_path.to_str().unwrap()) {
                return None;
            }

            let metadata = fs::metadata(source_path).unwrap();
            let file_size = metadata.len();
            let file_size_mb = file_size as f64 / (1024.0 * 1024.0);
            if file_size_mb > 1_f64 {
                return None;
            }
            Some(entry)
        })
        .collect()
}