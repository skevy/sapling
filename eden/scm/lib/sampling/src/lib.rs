/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::fs::File;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::Arc;

use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use parking_lot::MutexGuard;

pub static CONFIG: OnceCell<Option<Arc<SamplingConfig>>> = OnceCell::new();

pub fn init(config: &dyn configmodel::Config) {
    CONFIG.get_or_init(|| SamplingConfig::new(config).map(Arc::new));
}

#[derive(Debug)]
pub struct SamplingConfig {
    keys: HashMap<String, String>,
    file: Mutex<File>,
}

impl SamplingConfig {
    pub fn new(config: &dyn configmodel::Config) -> Option<Self> {
        let sample_categories: HashMap<String, String> = config
            .keys("sampling")
            .into_iter()
            .filter_map(|name| {
                if let Some(key) = name.strip_prefix("key.") {
                    if let Some(val) = config.get("sampling", &name) {
                        return Some((key.to_string(), val.to_string()));
                    }
                }
                None
            })
            .collect();
        if sample_categories.is_empty() {
            return None;
        }

        if let Some((output_file, okay_exists)) = sampling_output_file(config) {
            match OpenOptions::new()
                .create(okay_exists)
                .create_new(!okay_exists)
                .append(true)
                .open(&output_file)
            {
                Ok(file) => {
                    return Some(Self {
                        keys: sample_categories,
                        file: Mutex::new(file),
                    });
                }
                Err(err) => {
                    // This is expected for child commands that skirt the telemetry wrapper.
                    tracing::warn!(
                        ?err,
                        ?output_file,
                        "error opening sampling file (expected for child commands)"
                    );
                }
            }
        }

        None
    }

    pub fn category(&self, key: &str) -> Option<&str> {
        self.keys.get(key).map(|c| &**c)
    }

    pub fn file(&self) -> MutexGuard<File> {
        self.file.lock()
    }
}

// Returns tuple of output path and whether it's okay if the path already exists.
fn sampling_output_file(config: &dyn configmodel::Config) -> Option<(PathBuf, bool)> {
    let mut candidates: Vec<(PathBuf, bool)> = Vec::with_capacity(2);

    if let Ok(path) = std::env::var("SCM_SAMPLING_FILEPATH") {
        // Env var is not-okay-exists (i.e. only one process should respect this).
        candidates.push((path.into(), false));
    }

    if let Some(path) = config.get("sampling", "filepath") {
        // Config setting is okay to be shared across multiple commands (mainly
        // for test compat).
        candidates.push((path.to_string().into(), true));
    }

    candidates
        .into_iter()
        .find(|(path, _okay_exists)| path.parent().map_or(false, |d| d.exists()))
}
