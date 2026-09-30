//! Presence checks for the paths an agent lifecycle operation inspects before
//! it moves or opens anything.
//!
//! A lifecycle operation must tell "nothing is there" apart from "whether
//! anything is there cannot be established": the first lets it skip a store
//! or move a directory into place, the second must stop it. A path under an
//! ancestor that is a file, or that cannot be read, is the second case on
//! every platform, even where the platform reports it as simply missing.

use std::path::Path;

/// Whether something exists at a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathPresence {
    /// Something exists at the path.
    Present,
    /// Nothing exists at the path, and its nearest existing ancestor is a
    /// directory, so the path could be created.
    Absent,
    /// Whether anything exists at the path could not be established. Carries
    /// the reason, naming the path or ancestor that could not be inspected.
    Uninspectable(String),
}

impl PathPresence {
    #[must_use]
    pub fn is_uninspectable(&self) -> bool {
        matches!(self, Self::Uninspectable(_))
    }

    /// The reason an uninspectable path could not be inspected.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Uninspectable(reason) => Some(reason),
            Self::Present | Self::Absent => None,
        }
    }
}

/// Turn one `try_exists` result for `path` into a presence, identically on
/// every platform.
///
/// `Ok(true)` is present and an error is uninspectable. `Ok(false)` is absent
/// only when the nearest existing ancestor is a directory: an ancestor that
/// exists but is not a directory, or that cannot be inspected, makes the path
/// uninspectable. Unix reports a file standing where a directory must be as
/// an error, while Windows reports it as `Ok(false)`, which is why a missing
/// path re-walks its ancestors.
pub async fn classify_presence(path: &Path, probe: std::io::Result<bool>) -> PathPresence {
    match probe {
        Ok(true) => PathPresence::Present,
        Err(e) => PathPresence::Uninspectable(format!("cannot inspect {}: {e}", path.display())),
        Ok(false) => {
            for ancestor in path.ancestors().skip(1) {
                if ancestor.as_os_str().is_empty() {
                    continue;
                }
                match tokio::fs::metadata(ancestor).await {
                    Ok(meta) if meta.is_dir() => return PathPresence::Absent,
                    Ok(_) => {
                        return PathPresence::Uninspectable(format!(
                            "{} is not a directory",
                            ancestor.display()
                        ));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        return PathPresence::Uninspectable(format!(
                            "cannot inspect {}: {e}",
                            ancestor.display()
                        ));
                    }
                }
            }
            // A relative path none of whose ancestors exist hangs off the
            // working directory.
            PathPresence::Absent
        }
    }
}

/// Inspect `path` with [`classify_presence`].
pub async fn inspect_lifecycle_path(path: &Path) -> PathPresence {
    classify_presence(path, tokio::fs::try_exists(path).await).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn a_file_standing_where_a_directory_must_be_is_uninspectable() {
        let tmp = TempDir::new().unwrap();
        let blocker = tmp.path().join("agents");
        std::fs::write(&blocker, "not a directory").unwrap();
        let path = blocker.join("scout").join("workspace");

        // Whatever the platform reports for the path itself...
        let presence = inspect_lifecycle_path(&path).await;
        assert!(presence.is_uninspectable(), "{presence:?}");
        assert!(presence.reason().is_some());

        // ...both shapes it can take are classified the same way: Unix
        // reports an error, Windows reports the path as missing.
        let error = std::io::Error::new(std::io::ErrorKind::NotADirectory, "not a directory");
        let from_error = classify_presence(&path, Err(error)).await;
        assert!(from_error.is_uninspectable(), "{from_error:?}");
        assert!(
            from_error
                .reason()
                .is_some_and(|reason| reason.contains(&path.display().to_string())),
            "{from_error:?}"
        );

        let from_missing = classify_presence(&path, Ok(false)).await;
        assert!(from_missing.is_uninspectable(), "{from_missing:?}");
        assert!(
            from_missing
                .reason()
                .is_some_and(|reason| reason.contains(&blocker.display().to_string())),
            "the reason names the blocking ancestor: {from_missing:?}"
        );
    }

    #[tokio::test]
    async fn a_missing_path_under_a_real_directory_is_absent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("agents").join("scout").join("workspace");

        assert_eq!(inspect_lifecycle_path(&path).await, PathPresence::Absent);
        assert_eq!(
            classify_presence(&path, Ok(false)).await,
            PathPresence::Absent
        );
        assert_eq!(PathPresence::Absent.reason(), None);
        assert!(!path.exists(), "inspecting a path creates nothing");
        assert!(!tmp.path().join("agents").exists());
    }

    #[tokio::test]
    async fn an_existing_path_is_present() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("workspace");
        std::fs::create_dir(&dir).unwrap();
        let file = tmp.path().join("jobs.db");
        std::fs::write(&file, "").unwrap();

        assert_eq!(inspect_lifecycle_path(&dir).await, PathPresence::Present);
        assert_eq!(inspect_lifecycle_path(&file).await, PathPresence::Present);
        assert!(!PathPresence::Present.is_uninspectable());
    }
}
