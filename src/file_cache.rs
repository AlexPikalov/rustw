// Copyright 2016 The Rustw Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::str;

use analysis::{AnalysisHost, Target, Id};
use rustdoc::html::markdown;
use rustdoc::html::markdown::RenderType;
use span;
use vfs::{Vfs, FileContents};

use super::highlight;

// TODO maximum size and evication policy
// TODO keep timestamps and check on every read. Then don't empty on build.

pub struct Cache {
    files: Vfs<VfsUserData>,
    summaries: HashMap<Id, DefSummary>,
    analysis: AnalysisHost,
    project_dir: PathBuf,
}

type Span = span::Span<span::ZeroIndexed>;

#[derive(Serialize, Debug, Clone)]
pub struct SearchResult {
    pub defs: Vec<FileResult>,
    pub refs: Vec<FileResult>,
}

#[derive(Serialize, Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct FileResult {
    pub file_name: String,
    pub lines: Vec<LineResult>,
}

#[derive(Serialize, Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct LineResult {
    pub line_start: u32,
    pub column_start: u32,
    pub column_end: u32,
    pub line: String,
}

impl LineResult {
    fn new(span: &Span, line: String) -> LineResult {
        LineResult {
            line_start: span.range.row_start.one_indexed().0,
            column_start: span.range.col_start.one_indexed().0,
            column_end: span.range.col_end.one_indexed().0,
            line: line,
        }
    }
}

#[derive(Serialize, Debug, Clone)]
pub struct FindResult {
    pub results: Vec<FileResult>,
}

#[derive(Serialize, Debug, Clone)]
pub struct DefSummary {
    id: u32,
    bread_crumbs: Vec<String>,
    signature: String,
    doc_summary: String,
    doc_rest: String,
    parent: u32,
    children: Vec<DefChild>,
}

#[derive(Serialize, Debug, Clone)]
pub struct DefChild {
    id: u32,
    signature: String,
    doc_summary: String,
}

// Our data which we attach to files in the VFS.
struct VfsUserData {
    highlighted_lines: Vec<String>,
}

impl VfsUserData {
    fn new() -> VfsUserData {
        VfsUserData {
            highlighted_lines: vec![],
        }
    }
}

macro_rules! vfs_err {
    ($e: expr) => {
        {
            let r: Result<_, String> = $e.map_err(|e| e.into());
            r
        }
    }
}

impl Cache {
    pub fn new() -> Cache {
        Cache {
            files: Vfs::new(),
            summaries: HashMap::new(),
            analysis: AnalysisHost::new(Target::Debug),
            project_dir: env::current_dir().unwrap(),
        }
    }

    pub fn reset(&mut self) {
        self.files.clear();
        self.summaries = HashMap::new();
    }

    pub fn get_text(&self, path: &Path) -> Result<String, String> {
        match self.files.load_file(path) {
            Ok(FileContents::Text(s)) => Ok(s),
            Ok(FileContents::Binary(_)) => Err(::vfs::Error::BadFileKind.into()),
            Err(e) => Err(e.into())
        }
    }

    pub fn get_bytes(&self, path: &Path) -> Result<Vec<u8>, String> {
        match self.files.load_file(path) {
            Ok(FileContents::Text(s)) => Ok(s.into_bytes()),
            Ok(FileContents::Binary(b)) => Ok(b),
            Err(e) => Err(e.into())
        }
    }

    pub fn get_lines(&self, path: &Path, line_start: span::Row<span::ZeroIndexed>, line_end: span::Row<span::ZeroIndexed>) -> Result<String, String> {
        vfs_err!(self.files.load_file(path))?;
        vfs_err!(self.files.load_lines(path, line_start, line_end))
    }

    pub fn summary(&mut self, id: Id) -> Result<&DefSummary, String> {
        if !self.summaries.contains_key(&id) {
            // TODO catch this error and make a "no summary available" page
            let summary = self.make_summary(id)?;
            self.summaries.insert(id, summary);
        }
        Ok(&self.summaries[&id])
    }

    // TODO handle non-rs files by returning plain text lines
    pub fn get_highlighted(&self, path: &Path) -> Result<Vec<String>, String> {
        vfs_err!(self.files.load_file(path))?;
        vfs_err!(self.files.ensure_user_data(path, |_| Ok(VfsUserData::new())))?;
        vfs_err!(self.files.with_user_data(path, |u| {
            let (text, u) = u?;
            let text = match text {
                Some(t) => t,
                None => return Err(::vfs::Error::BadFileKind),
            };
            if u.highlighted_lines.is_empty() {
                let highlighted = highlight::highlight(&self.analysis,
                                                       &self.project_dir,
                                                       path.to_str().unwrap().to_owned(),
                                                       text.to_owned());

                let mut highlighted_lines = vec![];
                for line in highlighted.lines() {
                    highlighted_lines.push(line.replace("<br>", "\n"));
                }
                if text.ends_with('\n') {
                    highlighted_lines.push(String::new());
                }
                u.highlighted_lines = highlighted_lines;
            }

            Ok(u.highlighted_lines.clone())
        }))
    }

    pub fn get_highlighted_line(&self, file_name: &Path, line: span::Row<span::ZeroIndexed>) -> Result<String, String> {
        let lines = self.get_highlighted(Path::new(file_name))?;
        Ok(lines[line.0 as usize].clone())
    }

    pub fn update_analysis(&mut self) {
        // FIXME Possibly extreme, could invalidate by crate or by file. Also, only
        // need to invalidate Rust files.
        self.files.clear();

        info!("Processing analysis...");
        // TODO if this is a test run, we should mock the analysis, rather than trying to read it in.
        self.project_dir = env::current_dir().unwrap();
        self.analysis.reload(&self.project_dir, &self.project_dir, true).unwrap();
        info!("done");
    }

    pub fn id_search(&mut self, id: Id) -> Result<SearchResult, String> {
        self.ids_search(vec![id])
    }

    pub fn ident_search(&mut self, needle: &str) -> Result<SearchResult, String> {
        // First see if the needle corresponds to any definitions, if it does, get a list of the
        // ids, otherwise, return an empty search result.
        let ids = match self.analysis.search_for_id(needle) {
            Ok(ids) => ids.to_owned(),
            Err(_) => {
                return Ok(SearchResult {
                    defs: vec![],
                    refs: vec![],
                });
            }
        };

        self.ids_search(ids)
    }

    pub fn find_impls(&mut self, id: Id) -> Result<FindResult, String> {
        let impls = self.analysis.find_impls(id).map_err(|_| "No impls found".to_owned())?;
        Ok(FindResult {
            results: self.make_search_results(impls)?,
        })
    }

    fn ids_search(&mut self, ids: Vec<Id>) -> Result<SearchResult, String> {
        let mut defs = Vec::new();
        let mut refs = Vec::new();

        for id in ids {
            // If all_refs.len() > 0, the first entry will be the def.
            let all_refs = self.analysis.find_all_refs_by_id(id);
            let mut all_refs = match all_refs {
                Err(_) => return Err("Error finding references".to_owned()),
                Ok(ref all_refs) if all_refs.is_empty() => continue,
                Ok(all_refs) => all_refs.into_iter(),
            };

            defs.push(all_refs.next().unwrap());
            for ref_span in all_refs {
                refs.push(ref_span);
            }
        }

        // TODO need to save the span for highlighting
        // We then save each bucket of defs/refs as a vec, and put it together to return.
        return Ok(SearchResult {
            defs: self.make_search_results(defs)?,
            refs: self.make_search_results(refs)?,
        });

    }

    fn make_search_results(&self, raw: Vec<Span>) -> Result<Vec<FileResult>, String> {
        let mut file_buckets = HashMap::new();

        for span in &raw {
            let file_path = Path::new(&span.file);
            let file_path = file_path.strip_prefix(&self.project_dir).unwrap_or(file_path);
            let text = match self.get_highlighted_line(&file_path, span.range.row_start) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let line = LineResult::new(&span, text);
            file_buckets.entry(file_path.display().to_string()).or_insert_with(|| vec![]).push(line);
        }

        let mut result = vec![];
        for (file_path, mut lines) in file_buckets.into_iter() {
            lines.sort();
            let per_file = FileResult {
                file_name: file_path,
                lines: lines,
            };
            result.push(per_file);
        }
        result.sort();
        Ok(result)
    }

    fn make_summary(&self, id: Id) -> Result<DefSummary, String> {
        // fn render_markdown(input: &str) -> String {
        //     format!("{}", markdown::Markdown(input, RenderType::Hoedown))
        // }

        // // FIXME needs crate bread-crumb - needs a change to save-analysis to emit a top-level module: https://github.com/rust-lang/rust/issues/37818
        // let bread_crumbs = self.analysis.def_parents(id).unwrap_or(vec![]).into_iter().map(|(id, name)| {
        //     use rustdoc_highlight::Class;

        //     let mut buf = vec![];
        //     let mut extra = HashMap::new();
        //     extra.insert("link".to_owned(), format!("summary:{}", id));
        //     extra.insert("id".to_owned(), format!("breadcrumb_{}", id));
        //     highlight::write_span(&mut buf,
        //                           Class::None,
        //                           Some("link_breadcrumb".to_owned()),
        //                           name,
        //                           true,
        //                           extra).unwrap();
        //     String::from_utf8(buf).unwrap()
        // }).collect();

        // let def = self.analysis.get_def(id).map_err(|_| format!("No def for {}", id))?;

        // trace!("def: {:?}", def);

        // let docs = def.docs;
        // let (doc_summary, doc_rest) = match docs.find("\n\n") {
        //     Some(index) => (docs[..index].to_owned(), docs[index + 2..].to_owned()),
        //     _ => (docs, String::new()),
        // };

        // let sig = match def.sig {
        //     Some(sig) => {
        //         let mut h = highlight::BasicHighlighter::new();
        //         h.span(sig.ident_start as u32, sig.ident_end as u32, "summary_ident".to_owned(), format!("def_{}", id), Some(def.span.clone()));
        //         highlight::custom_highlight(def.span.file.to_str().unwrap().to_owned(), sig.text, &mut h)
        //     }
        //     None => def.name,
        // };

        // let children = self.analysis.for_each_child_def(id, |id, def| {
        //     trace!("child def: {:?}", def);
        //     let docs = def.docs.to_owned();
        //     let sig = def.sig.as_ref().map(|s| {
        //         let mut h = highlight::BasicHighlighter::new();
        //         h.span(s.ident_start as u32, s.ident_end as u32, "summary_ident".to_owned(), format!("def_{}", id), Some(def.span.clone()));
        //         highlight::custom_highlight(def.span.file.to_str().unwrap().to_owned(), s.text.clone(), &mut h)
        //     }).expect("No signature for def");
        //     let docs = render_markdown(&match docs.find("\n\n") {
        //         Some(index) => docs[..index].to_owned(),
        //         _ => docs,
        //     });
        //     DefChild {
        //         id: id,
        //         signature: sig,
        //         doc_summary: docs,
        //     }
        // }).map_err(|_| format!("No children for {}", id))?;

        // Ok(DefSummary {
        //     id: id,
        //     bread_crumbs: bread_crumbs,
        //     signature: sig,
        //     doc_summary: render_markdown(&doc_summary),
        //     doc_rest: render_markdown(&doc_rest),
        //     parent: def.parent.unwrap_or(0),
        //     children: children,
        // })
        unimplemented!();
    }
}
