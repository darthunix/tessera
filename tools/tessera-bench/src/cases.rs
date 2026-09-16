//! Group selected operations into cases: one process per side per case.

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// Run name and source index (0 = before, 1 = after).
pub const ORDER: [(&str, usize); 2] = [("before", 0), ("after", 1)];

#[derive(Serialize)]
pub struct Case {
    pub bench: String,
    pub name: String,
    pub directory: String,
    pub paths: BTreeSet<String>,
}

/// Keep operations whose id contains any filter; no filters keep everything.
pub fn select(listed: BTreeSet<String>, filters: &[String]) -> BTreeSet<String> {
    listed
        .into_iter()
        .filter(|id| filters.is_empty() || filters.iter().any(|filter| id.contains(filter)))
        .collect()
}

pub fn group(bench: &str, paths: BTreeSet<String>) -> Result<Vec<Case>> {
    ensure!(!paths.is_empty(), "no benchmark cases matched");
    let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for path in paths {
        let (name, _) = path.rsplit_once('/').context("invalid benchmark name")?;
        groups.entry(name.to_owned()).or_default().insert(path);
    }
    Ok(groups
        .into_iter()
        .enumerate()
        .map(|(index, (name, paths))| Case {
            bench: bench.to_owned(),
            name,
            directory: format!("{bench}-case-{:04}", index + 1),
            paths,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed() -> BTreeSet<String> {
        ["dense/a", "datum/a"]
            .iter()
            .flat_map(|name| ["fold", "reference"].map(|path| format!("reader/{name}/{path}")))
            .collect()
    }

    #[test]
    fn grouping_preserves_the_selection_and_separates_representations() -> Result<()> {
        let cases = group("reader", listed())?;
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].name, "reader/datum/a");
        assert_eq!(cases[0].directory, "reader-case-0001");
        assert_eq!(cases[1].name, "reader/dense/a");
        assert_eq!(
            cases[1].paths,
            BTreeSet::from([
                "reader/dense/a/fold".to_owned(),
                "reader/dense/a/reference".to_owned()
            ])
        );
        assert!(group("reader", BTreeSet::new()).is_err());
        Ok(())
    }

    #[test]
    fn filters_are_substrings_and_any_match_keeps_an_operation() {
        assert_eq!(select(listed(), &[]), listed());
        let dense = select(listed(), &["/dense/".to_owned()]);
        assert_eq!(dense.len(), 2);
        assert!(dense.iter().all(|id| id.contains("/dense/")));
        let both = select(listed(), &["/dense/".to_owned(), "/datum/".to_owned()]);
        assert_eq!(both, listed());
        assert!(select(listed(), &["missing".to_owned()]).is_empty());
    }
}
