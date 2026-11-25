#![forbid(unsafe_code)]

//! Deterministic Accessibility-Tree Locator Engine (`e1`, `e2`, ...), CSS-independent
//! fingerprinting, self-healing DOM box resolution, and CDP input dispatch.

pub mod actions;
pub mod ax_tree;
pub mod fingerprint;
pub mod registry;

pub use actions::LocatorEngine;
pub use ax_tree::{AxNode, AxStates, is_interactive_role, is_landmark_role, is_semantic_role, parse_full_ax_tree};
pub use fingerprint::{ElementFingerprint, compute_fingerprint, normalize_name};
pub use registry::{RefEntry, RefRegistry, Snapshot, SnapshotItem};
