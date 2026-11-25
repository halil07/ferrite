#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::pixelmatch::{DiffOptions, diff};
use crate::png_codec::{VisualError, decode_png, encode_png};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselinePolicy {
    pub update_baselines: bool,
    pub max_diff_ratio: f64,
    pub diff_options: DiffOptions,
}

impl Default for BaselinePolicy {
    fn default() -> Self {
        Self {
            update_baselines: false,
            max_diff_ratio: 0.001,
            diff_options: DiffOptions::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum VisualAssertionResult {
    Passed {
        diff_pixels: u64,
        aa_pixels: u64,
        diff_ratio: f64,
    },
    BaselineCreated {
        path: PathBuf,
    },
    BaselineUpdated {
        path: PathBuf,
    },
    Failed {
        diff_pixels: u64,
        aa_pixels: u64,
        diff_ratio: f64,
        diff_path: PathBuf,
        actual_path: PathBuf,
    },
}

/// File-system baseline snapshot store with strict path-traversal rejection.
#[derive(Debug, Clone)]
pub struct SnapshotStore {
    root: PathBuf,
}

impl SnapshotStore {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn validate_name(name: &str) -> Result<&str, VisualError> {
        if name.is_empty()
            || name.len() > 128
            || name.contains("..")
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(VisualError::InvalidSnapshotName(name.to_string()));
        }
        Ok(name)
    }

    pub fn baseline_path(&self, name: &str) -> Result<PathBuf, VisualError> {
        let valid = Self::validate_name(name)?;
        Ok(self.root.join("baselines").join(format!("{valid}.png")))
    }

    pub fn assert_matches(
        &self,
        name: &str,
        actual_png: &[u8],
        policy: &BaselinePolicy,
    ) -> Result<VisualAssertionResult, VisualError> {
        let valid = Self::validate_name(name)?;
        let actual_img = decode_png(actual_png)?;

        let baseline_path = self.baseline_path(valid)?;
        if let Some(parent) = baseline_path.parent() {
            fs::create_dir_all(parent)?;
        }

        if !baseline_path.exists() {
            fs::write(&baseline_path, actual_png)?;
            return Ok(VisualAssertionResult::BaselineCreated {
                path: baseline_path,
            });
        }

        if policy.update_baselines {
            fs::write(&baseline_path, actual_png)?;
            return Ok(VisualAssertionResult::BaselineUpdated {
                path: baseline_path,
            });
        }

        let baseline_bytes = fs::read(&baseline_path)?;
        let baseline_img = decode_png(&baseline_bytes)?;
        let report = diff(&baseline_img, &actual_img, &policy.diff_options)?;

        if report.diff_ratio <= policy.max_diff_ratio {
            return Ok(VisualAssertionResult::Passed {
                diff_pixels: report.diff_pixels,
                aa_pixels: report.aa_pixels,
                diff_ratio: report.diff_ratio,
            });
        }

        let diffs_dir = self.root.join("diffs");
        let actual_dir = self.root.join("actual");
        fs::create_dir_all(&diffs_dir)?;
        fs::create_dir_all(&actual_dir)?;

        let diff_path = diffs_dir.join(format!("{valid}.diff.png"));
        let actual_path = actual_dir.join(format!("{valid}.actual.png"));
        fs::write(&diff_path, encode_png(&report.diff_image)?)?;
        fs::write(&actual_path, actual_png)?;

        Ok(VisualAssertionResult::Failed {
            diff_pixels: report.diff_pixels,
            aa_pixels: report.aa_pixels,
            diff_ratio: report.diff_ratio,
            diff_path,
            actual_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::png_codec::{RgbaImage, encode_png};

    fn make_png(color: [u8; 4]) -> Vec<u8> {
        let img = RgbaImage {
            width: 8,
            height: 8,
            data: color.iter().copied().cycle().take(8 * 8 * 4).collect(),
        };
        encode_png(&img).unwrap()
    }

    #[test]
    fn rejects_path_traversal_and_writes_diff_artifacts_on_regression() {
        let dir = std::env::temp_dir().join(format!(
            "ferrite-visual-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = SnapshotStore::new(&dir);

        assert!(matches!(
            store.assert_matches("../escape", &make_png([0, 0, 0, 255]), &BaselinePolicy::default()),
            Err(VisualError::InvalidSnapshotName(_))
        ));

        let baseline = make_png([255, 255, 255, 255]);
        let created = store
            .assert_matches("login-screen", &baseline, &BaselinePolicy::default())
            .unwrap();
        assert!(matches!(created, VisualAssertionResult::BaselineCreated { .. }));

        let passed = store
            .assert_matches("login-screen", &baseline, &BaselinePolicy::default())
            .unwrap();
        assert!(matches!(
            passed,
            VisualAssertionResult::Passed { diff_pixels: 0, .. }
        ));

        let regression = make_png([0, 0, 0, 255]);
        let failed = store
            .assert_matches("login-screen", &regression, &BaselinePolicy::default())
            .unwrap();
        match failed {
            VisualAssertionResult::Failed {
                diff_pixels,
                diff_path,
                actual_path,
                ..
            } => {
                assert_eq!(diff_pixels, 64);
                assert!(diff_path.exists());
                assert!(actual_path.exists());
            }
            other => panic!("expected Failed, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&dir);
    }
}
