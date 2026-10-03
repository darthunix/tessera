//! A cut that should only move lines: every non-blank line of the base's
//! files is in one of the candidate's files, as many times as it was.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use tessera_pgtool::snapshot::output;

/// The files before and after the cut.
#[derive(clap::Args, Debug)]
pub struct Options {
    /// The revision of the files before the cut.
    #[arg(long, value_name = "REF")]
    pub base: String,
    /// The revision of the files after it: a Git revision or WORKTREE.
    #[arg(long, value_name = "REF", default_value = "WORKTREE")]
    pub candidate: String,
    /// A file of the base, such as nodes/agg.c; the option repeats.
    #[arg(long, value_name = "PATH", required = true)]
    pub from: Vec<PathBuf>,
    /// A file of the candidate, such as nodes/agg_group.c; the option repeats.
    #[arg(long, value_name = "PATH", required = true)]
    pub to: Vec<PathBuf>,
}

/// A file's name and its lines.
pub struct File {
    pub name: String,
    pub text: String,
}

/// What did not move as it was.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Moves {
    /// Non-blank lines of the base's files, and of those found again.
    pub lines: usize,
    pub moved: usize,
    /// Lines found again without their leading `static`: a function one
    /// file now calls in another, as a cut makes it.
    pub unstatic: Vec<Place>,
    /// Lines of the base not found again, and lines new in the candidate.
    pub missing: Vec<Place>,
    pub added: Vec<Place>,
}

/// A line and where it first stood: the file and the line number.
#[derive(Debug, PartialEq, Eq)]
pub struct Place {
    pub text: String,
    pub file: String,
    pub line: usize,
}

/// The non-blank lines of a file with their numbers, a function's type
/// line joined to the line of its name that follows it, as PostgreSQL's
/// style writes a definition: `static void` alone stands for many
/// functions, `static void` with `agg_end(CustomScanState *css)` for one.
fn lines(text: &str) -> Vec<(String, usize)> {
    let all: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut at = 0;
    while at < all.len() {
        let line = all[at];
        at += 1;
        if line.trim().is_empty() {
            continue;
        }
        let name = all.get(at).is_some_and(|next| {
            let identifier = next
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
                .count();
            identifier > 0 && next[identifier..].starts_with('(')
        });
        let type_line = !line.starts_with([' ', '\t', '#', '/', '*', '}'])
            && !line.ends_with([';', ',', '{', '}', ')']);
        if name && type_line {
            out.push((format!("{line}\n{}", all[at]), at));
            at += 1;
        } else {
            out.push((line.to_owned(), at));
        }
    }
    out
}

/// Compare the base's files with the candidate's, line by line as a
/// multiset: order and file do not matter, a line's text and its count do.
pub fn compare(before: &[File], after: &[File]) -> Moves {
    let mut available: BTreeMap<String, Vec<(&str, usize)>> = BTreeMap::new();
    for file in after {
        for (line, number) in lines(&file.text) {
            available
                .entry(line)
                .or_default()
                .push((file.name.as_str(), number));
        }
    }
    for places in available.values_mut() {
        places.reverse();
    }
    let mut moves = Moves::default();
    let mut unmatched = Vec::new();
    for file in before {
        for (line, number) in lines(&file.text) {
            moves.lines += 1;
            if available.get_mut(&line).and_then(Vec::pop).is_some() {
                moves.moved += 1;
            } else {
                unmatched.push((line, file.name.as_str(), number));
            }
        }
    }
    for (line, name, number) in unmatched {
        let without = line.strip_prefix("static ").map(str::to_owned);
        let place = Place {
            text: line,
            file: name.to_owned(),
            line: number,
        };
        if let Some(rest) = without
            && available.get_mut(&rest).and_then(Vec::pop).is_some()
        {
            moves.unstatic.push(place);
        } else {
            moves.missing.push(place);
        }
    }
    for (line, places) in available {
        for (name, number) in places.into_iter().rev() {
            moves.added.push(Place {
                text: line.clone(),
                file: name.to_owned(),
                line: number,
            });
        }
    }
    moves
        .added
        .sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
    moves
}

/// The comparison as text; exit 1 when a line of the base is missing.
pub fn report(moves: &Moves) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} lines of the base (a definition's type and name one): {} moved as they were, \
         {} without `static`, {} missing; {} new lines",
        moves.lines,
        moves.moved,
        moves.unstatic.len(),
        moves.missing.len(),
        moves.added.len()
    );
    for (title, places) in [
        ("Without `static` now", &moves.unstatic),
        ("Missing from the candidate", &moves.missing),
        ("New in the candidate", &moves.added),
    ] {
        if places.is_empty() {
            continue;
        }
        let _ = writeln!(out, "\n{title}:");
        for place in places {
            let text = place.text.replace('\n', " ");
            let _ = writeln!(out, "  {}:{}: {text}", place.file, place.line);
        }
    }
    out
}

pub fn run(repo: &Path, options: &Options) -> Result<u8> {
    let read = |revision: &str, paths: &[PathBuf]| -> Result<Vec<File>> {
        paths
            .iter()
            .map(|path| {
                let name = path.to_string_lossy().into_owned();
                let text = if revision == "WORKTREE" {
                    fs::read_to_string(repo.join(path))
                        .with_context(|| format!("cannot read {name}"))?
                } else {
                    String::from_utf8(output(
                        Command::new("git")
                            .current_dir(repo)
                            .arg("show")
                            .arg(format!("{revision}:{name}")),
                    )?)
                    .with_context(|| format!("{revision}:{name} is not UTF-8"))?
                };
                Ok(File { name, text })
            })
            .collect()
    };
    let moves = compare(
        &read(&options.base, &options.from)?,
        &read(&options.candidate, &options.to)?,
    );
    print!("{}", report(&moves));
    Ok(u8::from(!moves.missing.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, text: &str) -> File {
        File {
            name: name.to_owned(),
            text: text.to_owned(),
        }
    }

    #[test]
    fn a_cut_moves_lines_and_unstatics_functions() {
        let before = [file(
            "agg.c",
            "static void\nhelper(void)\n{\n}\n\nstatic void helper(void);\nint\nmain(void)\n{\n}\n",
        )];
        let after = [
            file("agg.c", "int\nmain(void)\n{\n}\n"),
            file(
                "agg_group.c",
                "#include \"agg_node.h\"\n\nvoid\nhelper(void)\n{\n}\n",
            ),
        ];
        let moves = compare(&before, &after);
        assert_eq!((moves.lines, moves.moved), (7, 5));
        assert_eq!(moves.unstatic.len(), 1);
        assert_eq!(moves.unstatic[0].text, "static void\nhelper(void)");
        assert_eq!(moves.unstatic[0].line, 1);
        assert_eq!(moves.missing.len(), 1);
        assert_eq!(
            (moves.missing[0].text.as_str(), moves.missing[0].line),
            ("static void helper(void);", 6)
        );
        assert_eq!(moves.added.len(), 1);
        assert_eq!(
            (moves.added[0].file.as_str(), moves.added[0].line),
            ("agg_group.c", 1)
        );
        let text = report(&moves);
        assert!(text.contains("7 lines of the base (a definition's type and name one): 5 moved"));
        assert!(text.contains("agg.c:1: static void helper(void)\n"));
        assert!(text.contains("agg.c:6: static void helper(void);"));
    }

    #[test]
    fn a_line_counts_as_many_times_as_it_stood() {
        let before = [file("a.c", "}\n}\n}\n")];
        let after = [file("b.c", "}\n}\n")];
        let moves = compare(&before, &after);
        assert_eq!((moves.moved, moves.missing.len()), (2, 1));
        let changed = [file("b.c", "}\n}\n};\n")];
        let moves = compare(&before, &changed);
        assert_eq!((moves.missing.len(), moves.added.len()), (1, 1));
    }
}
