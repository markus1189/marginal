//! Which backend owns a file, and the one place that decides.
//!
//! Everything downstream of a parse consumes the same four artifacts — a flat
//! unit list, a containment tree, per-line syntax marks, and question
//! positions — so a *format* is exactly the code that produces them and nothing
//! else. `blocks::questions` is not among them on purpose: it reads the tree, so
//! it is written once and works for any backend that fills one in.
//!
//! Two backends today. `blocks` is markdown, through comrak, and is the only
//! thing in the crate that knows comrak exists. `plain` is the fallback, and
//! knows nothing at all.

use crate::blocks::{Block, TreeNode};
use crate::highlight::LineMarks;
use crate::{blocks, highlight, plain};

/// Every format name `--format` accepts, in the order the usage text lists
/// them. One list, so a name the parser takes and the message a typo gets
/// cannot drift apart.
pub const NAMES: [&str; 2] = ["markdown", "plain"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    #[default]
    Markdown,
    Plain,
}

impl Format {
    /// The format a path's extension names — and `Plain` for every extension no
    /// backend claims, including none at all.
    ///
    /// Markdown is the special case here, not the default, and that is a change
    /// from opening every file through comrak. A `.tex` file parsed as markdown
    /// is not a degraded parse, it is a wrong one: `#` starts no heading in
    /// LaTeX, `_` marks no emphasis, and a unit list built from those puts
    /// comments on lines the reviewer never selected. Plain is the honest floor.
    ///
    /// Extension, never content sniffing. A `.tex` file whose first paragraph
    /// happens to look like a markdown list is still LaTeX, and a heuristic that
    /// disagreed would be unpredictable in exactly the case where it mattered.
    /// `--format` is the override, and it is the answer for a file with no
    /// extension — including the temp files a launcher opens, though both
    /// launchers in this tree name theirs `.md` today.
    pub fn of_path(path: &str) -> Self {
        let ext = std::path::Path::new(path)
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        match ext.as_str() {
            "md" | "markdown" => Self::Markdown,
            _ => Self::Plain,
        }
    }

    /// The `--format` spelling, or `None` for a name no backend answers to.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "markdown" => Some(Self::Markdown),
            "plain" => Some(Self::Plain),
            _ => None,
        }
    }

    /// The flat navigation units.
    pub fn parse(self, src: &str) -> Vec<Block> {
        match self {
            Self::Markdown => blocks::parse(src),
            Self::Plain => plain::parse(src),
        }
    }

    /// The containment hierarchy.
    pub fn parse_tree(self, src: &str) -> TreeNode {
        match self {
            Self::Markdown => blocks::parse_tree(src),
            Self::Plain => plain::parse_tree(src),
        }
    }

    /// Per-line syntax marks. Takes the tree the caller already built rather
    /// than parsing a second time — the markdown backend derives its marks from
    /// exactly that tree, and a backend that re-parsed could disagree with the
    /// spans everything else is using.
    pub fn marks(self, tree: &TreeNode, src: &str) -> Vec<LineMarks> {
        match self {
            Self::Markdown => highlight::marks(tree, src),
            Self::Plain => plain::marks(src),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_is_claimed_by_extension_and_nothing_else_is() {
        for path in ["PLAN.md", "a/b/PLAN.md", "notes.markdown", "SHOUTING.MD"] {
            assert_eq!(Format::of_path(path), Format::Markdown, "{path}");
        }
        for path in [
            "paper.tex",
            "README",
            "notes.txt",
            "doc.rst",
            "notes.org",
            ".md",           // an extensionless dotfile, not a markdown file
            "archive.md.gz", // the last extension is the one that counts
            "",
        ] {
            assert_eq!(Format::of_path(path), Format::Plain, "{path}");
        }
    }

    /// `NAMES` is what a typo is offered, so a name listed there and not
    /// accepted would advertise a format that does not exist.
    #[test]
    fn every_advertised_name_parses_and_an_unknown_one_is_rejected() {
        for name in NAMES {
            assert!(Format::from_name(name).is_some(), "{name}");
        }
        for bad in ["md", "latex", "Markdown", "", "tex"] {
            assert_eq!(Format::from_name(bad), None, "{bad}");
        }
    }

    /// The dispatch is the whole module, so the test that matters is that the
    /// arms differ: both backends answer, and they answer differently on input
    /// where markdown structure is real.
    #[test]
    fn the_two_backends_are_actually_different_parsers() {
        let src = "# Heading\n- item one\n- item two\n";
        let md = Format::Markdown.parse(src);
        let plain = Format::Plain.parse(src);
        assert_eq!(
            md.iter().map(|b| b.kind).collect::<Vec<_>>(),
            ["heading", "list-item", "list-item"]
        );
        assert_eq!(
            plain.iter().map(|b| b.kind).collect::<Vec<_>>(),
            ["paragraph"]
        );
        assert!(!Format::Markdown.marks(&Format::Markdown.parse_tree(src), src)[0].is_empty());
        assert!(Format::Plain.marks(&Format::Plain.parse_tree(src), src)[0].is_empty());
    }
}
