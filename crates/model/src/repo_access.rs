use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{ApiError, Repo, Visibility};

/// Which repositories of an organization an organization secret, variable or setting reaches,
/// as GitHub writes it in `visibility` and `selected_repository_ids`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepoAccess {
    All,
    /// Private and internal repositories.
    Private,
    Selected(BTreeSet<i64>),
}

impl RepoAccess {
    /// The access a write asks for. `selected` is read only for the `selected` visibility, so a
    /// client that sends an empty list beside `all` is answered as GitHub answers it.
    ///
    /// # Errors
    ///
    /// `422` for a `visibility` outside `all`, `private` and `selected`.
    pub fn parse(visibility: &str, selected: Option<&[i64]>) -> Result<RepoAccess, ApiError> {
        match visibility {
            "all" => Ok(RepoAccess::All),
            "private" => Ok(RepoAccess::Private),
            "selected" => Ok(RepoAccess::Selected(
                selected.unwrap_or_default().iter().copied().collect(),
            )),
            other => Err(ApiError::unprocessable(format!(
                "Invalid request.\n\nInvalid property /visibility: `{other}` is not a possible value."
            ))),
        }
    }

    /// The `visibility` GitHub answers.
    #[must_use]
    pub fn visibility(&self) -> &'static str {
        match self {
            RepoAccess::All => "all",
            RepoAccess::Private => "private",
            RepoAccess::Selected(_) => "selected",
        }
    }

    /// Whether `repo`, a repository of the organization, is reached.
    #[must_use]
    pub fn reaches(&self, repo: &Repo) -> bool {
        match self {
            RepoAccess::All => true,
            RepoAccess::Private => repo.visibility != Visibility::Public,
            RepoAccess::Selected(ids) => ids.contains(&repo.id),
        }
    }

    /// The selected repository IDs, `None` unless the visibility is `selected`.
    #[must_use]
    pub fn selected(&self) -> Option<&BTreeSet<i64>> {
        match self {
            RepoAccess::Selected(ids) => Some(ids),
            RepoAccess::All | RepoAccess::Private => None,
        }
    }

    /// Replaces the selected repositories, as `PUT .../repositories`.
    ///
    /// # Errors
    ///
    /// `409` unless the visibility is `selected`.
    pub fn set(&mut self, ids: impl IntoIterator<Item = i64>) -> Result<(), ApiError> {
        *self.selected_mut()? = ids.into_iter().collect();
        Ok(())
    }

    /// Adds one repository, as `PUT .../repositories/{repository_id}`; adding it again is a no-op.
    ///
    /// # Errors
    ///
    /// `409` unless the visibility is `selected`.
    pub fn add(&mut self, id: i64) -> Result<(), ApiError> {
        self.selected_mut()?.insert(id);
        Ok(())
    }

    /// Removes one repository, as `DELETE .../repositories/{repository_id}`; removing one that
    /// is not selected is a no-op.
    ///
    /// # Errors
    ///
    /// `409` unless the visibility is `selected`.
    pub fn remove(&mut self, id: i64) -> Result<(), ApiError> {
        self.selected_mut()?.remove(&id);
        Ok(())
    }

    fn selected_mut(&mut self) -> Result<&mut BTreeSet<i64>, ApiError> {
        match self {
            RepoAccess::Selected(ids) => Ok(ids),
            RepoAccess::All | RepoAccess::Private => Err(ApiError::conflict(
                "The visibility of the repository access is not set to selected.",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RepoAccess;

    #[test]
    fn a_selected_list_changes_only_under_the_selected_visibility() {
        // Given one access of each visibility
        let mut all = RepoAccess::parse("all", Some(&[1])).unwrap();
        let mut private = RepoAccess::parse("private", None).unwrap();
        let mut selected = RepoAccess::parse("selected", Some(&[3, 1, 3])).unwrap();

        // When each is set, added to and removed from
        let changes = |a: &mut RepoAccess| {
            [a.set([5, 1]), a.add(7), a.add(7), a.remove(5), a.remove(9)]
                .map(|r| r.map_err(|e| e.status))
        };

        // Then only the selected one changes, and the others answer 409 to each change
        assert_eq!(changes(&mut all), [Err(409); 5]);
        assert_eq!(changes(&mut private), [Err(409); 5]);
        assert_eq!(changes(&mut selected), [Ok(()); 5]);
        assert_eq!(
            selected
                .selected()
                .map(|s| s.iter().copied().collect::<Vec<_>>()),
            Some(vec![1, 7])
        );
        assert_eq!((all.visibility(), all.selected()), ("all", None));
        assert_eq!(private.visibility(), "private");
    }

    #[test]
    fn an_unknown_visibility_is_422() {
        assert_eq!(
            RepoAccess::parse("public", None).map_err(|e| e.status),
            Err(422)
        );
    }
}
