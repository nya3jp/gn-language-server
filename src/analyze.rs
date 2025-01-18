// Copyright 2025 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    collections::{BTreeMap, HashSet},
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
};

use either::Either;
use itertools::Itertools;
use pest::Span;
use tower_lsp::lsp_types::{DocumentSymbol, SymbolKind};

use crate::{
    ast::{parse, AssignOp, Block, Expr, LValue, Node, PrimaryExpr, Statement},
    storage::{Document, DocumentStorage},
    util::{parse_simple_literal, LineIndex},
};

fn is_exported(name: &str) -> bool {
    !name.starts_with("_")
}

fn find_workspace_root(path: &Path) -> std::io::Result<PathBuf> {
    for dir in path.ancestors().skip(1) {
        if dir.join(".gn").try_exists()? {
            return Ok(dir.to_path_buf());
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("Workspace not found for {}", path.to_string_lossy()),
    ))
}

fn resolve_path(name: &str, root_dir: &Path, current_dir: &Path) -> PathBuf {
    if let Some(rest) = name.strip_prefix("//") {
        root_dir.join(rest)
    } else {
        current_dir.join(name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorkspaceContext {
    pub root: PathBuf,
    pub build_config: PathBuf,
}

impl WorkspaceContext {
    pub fn resolve_path(&self, name: &str, current_dir: &Path) -> PathBuf {
        resolve_path(name, &self.root, current_dir)
    }
}

fn evaluate_dot_gn(path: &Path) -> std::io::Result<PathBuf> {
    let workspace_root = path.parent().unwrap();

    let input = std::fs::read_to_string(path)?;
    let line_index = LineIndex::new(&input);
    let ast_root = parse(&input);

    let mut build_config_path: Option<PathBuf> = None;

    for statement in &ast_root.statements {
        let Statement::Assignment(assignment) = statement else {
            continue;
        };
        if !matches!(&assignment.lvalue, LValue::Identifier(identifier) if identifier.name == "buildconfig")
        {
            continue;
        }

        let position = line_index.position(assignment.span.start());

        if assignment.op != AssignOp::Assign {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{}:{}:{}: buildconfig must be assigned exactly once",
                    path.to_string_lossy(),
                    position.line + 1,
                    position.character + 1
                ),
            ));
        }
        let Some(string) = assignment.rvalue.as_primary_string() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{}:{}:{}: buildconfig is not a simple string",
                    path.to_string_lossy(),
                    position.line + 1,
                    position.character + 1
                ),
            ));
        };
        let Some(name) = parse_simple_literal(&string.raw_value) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{}:{}:{}: buildconfig is not a simple string",
                    path.to_string_lossy(),
                    position.line + 1,
                    position.character + 1
                ),
            ));
        };

        if build_config_path
            .replace(resolve_path(name, workspace_root, workspace_root))
            .is_some()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{}:{}:{}: buildconfig is assigned multiple times",
                    path.to_string_lossy(),
                    position.line + 1,
                    position.character + 1
                ),
            ));
        }
    }

    let Some(build_config_path) = build_config_path else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{}: buildconfig is not assigned directly",
                path.to_string_lossy()
            ),
        ));
    };

    Ok(build_config_path)
}

pub struct AnalyzedFile {
    pub document: Arc<Document>,
    pub workspace: WorkspaceContext,
    pub ast_root: Block<'static>,
    pub analyzed_root: AnalyzedBlock<'static>,
    pub links: Vec<Link<'static>>,
    pub symbols: Vec<DocumentSymbol>,
}

impl AnalyzedFile {
    pub fn variables_at(&self, pos: usize) -> HashSet<AnalyzedVariable> {
        self.analyzed_root.variables_at(pos)
    }

    pub fn templates_at(&self, pos: usize) -> HashSet<&AnalyzedTemplate> {
        self.analyzed_root.templates_at(pos)
    }

    pub fn targets_at(&self, pos: usize) -> HashSet<&AnalyzedTarget> {
        self.analyzed_root.targets_at(pos)
    }
}

pub struct ThinAnalyzedFile {
    pub document: Arc<Document>,
    pub workspace: WorkspaceContext,
    pub analyzed_root: ThinAnalyzedBlock<'static>,
}

impl ThinAnalyzedFile {
    pub fn empty(path: &Path, workspace: &WorkspaceContext) -> Arc<Self> {
        let document = Document::empty(path);
        Arc::new(ThinAnalyzedFile {
            document,
            workspace: workspace.clone(),
            analyzed_root: ThinAnalyzedBlock::new(),
        })
    }
}

pub struct AnalyzedBlock<'i> {
    pub statements: Vec<AnalyzedStatement<'i>>,
    pub span: Span<'i>,
}

impl<'i> AnalyzedBlock<'i> {
    pub fn variables_at(&'i self, pos: usize) -> HashSet<AnalyzedVariable<'i>> {
        let mut variables = HashSet::new();
        for statement in &self.statements {
            match statement {
                AnalyzedStatement::NewVariable(new_variable) => {
                    if new_variable.span.end() <= pos {
                        variables.insert(AnalyzedVariable {
                            name: new_variable.name,
                            value: Some(new_variable.value.clone()),
                            document: new_variable.document,
                            span: new_variable.span,
                        });
                    }
                }
                AnalyzedStatement::ModifyVariable(modification) => {
                    if modification.span.end() <= pos {
                        variables = variables
                            .into_iter()
                            .map(|mut variable| {
                                if variable.name == modification.name {
                                    variable.value = None;
                                }
                                variable
                            })
                            .collect();
                    }
                }
                AnalyzedStatement::Conditions(blocks) => {
                    if blocks.last().unwrap().span.end() <= pos {
                        for block in blocks {
                            variables.extend(block.variables_at(pos));
                        }
                    } else {
                        for block in blocks {
                            if block.span.start() <= pos && pos <= block.span.end() {
                                variables.extend(block.variables_at(pos));
                            }
                        }
                    }
                }
                AnalyzedStatement::Import(import) => {
                    if import.span.end() <= pos {
                        variables.extend(import.file.analyzed_root.variables.clone());
                    }
                }
                AnalyzedStatement::DeclareArgs(block) => {
                    // A variable defined in declare_args() cannot be read in the later part of the
                    // same block.
                    if block.span.end() <= pos {
                        variables.extend(block.variables_at(pos));
                    }
                }
                AnalyzedStatement::NewScope(block) => {
                    if block.span.start() < pos && pos < block.span.end() {
                        variables.extend(block.variables_at(pos));
                    }
                }
                AnalyzedStatement::Template(_) | AnalyzedStatement::Target(_) => {}
            }
        }
        variables
    }

    pub fn templates_at(&'i self, pos: usize) -> HashSet<&'i AnalyzedTemplate<'i>> {
        let mut templates = HashSet::new();
        for statement in &self.statements {
            match statement {
                AnalyzedStatement::Conditions(blocks) => {
                    if blocks.last().unwrap().span.end() <= pos {
                        for block in blocks {
                            templates.extend(block.templates_at(pos));
                        }
                    } else {
                        for block in blocks {
                            if block.span.start() <= pos && pos <= block.span.end() {
                                templates.extend(block.templates_at(pos));
                            }
                        }
                    }
                }
                AnalyzedStatement::Import(import) => {
                    if import.span.end() <= pos {
                        templates.extend(import.file.analyzed_root.templates.iter());
                    }
                }
                AnalyzedStatement::Template(template) => {
                    if template.span.end() <= pos {
                        templates.insert(template);
                    }
                }
                AnalyzedStatement::NewScope(block) => {
                    if block.span.start() <= pos && pos <= block.span.end() {
                        templates.extend(block.templates_at(pos));
                    }
                }
                AnalyzedStatement::NewVariable(_)
                | AnalyzedStatement::ModifyVariable(_)
                | AnalyzedStatement::DeclareArgs(_)
                | AnalyzedStatement::Target(_) => {}
            }
        }
        templates
    }

    pub fn targets_at(&'i self, pos: usize) -> HashSet<&'i AnalyzedTarget<'i>> {
        let mut targets = HashSet::new();
        for statement in &self.statements {
            match statement {
                AnalyzedStatement::Conditions(blocks) => {
                    if blocks.last().unwrap().span.end() <= pos {
                        for block in blocks {
                            targets.extend(block.targets_at(pos));
                        }
                    } else {
                        for block in blocks {
                            if block.span.start() <= pos && pos <= block.span.end() {
                                targets.extend(block.targets_at(pos));
                            }
                        }
                    }
                }
                AnalyzedStatement::Import(import) => {
                    if import.span.end() <= pos {
                        targets.extend(import.file.analyzed_root.targets.iter());
                    }
                }
                AnalyzedStatement::Target(target) => {
                    if target.span.end() <= pos {
                        targets.insert(target);
                    }
                }
                AnalyzedStatement::NewScope(block) => {
                    if block.span.start() <= pos && pos <= block.span.end() {
                        targets.extend(block.targets_at(pos));
                    }
                }
                AnalyzedStatement::NewVariable(_)
                | AnalyzedStatement::ModifyVariable(_)
                | AnalyzedStatement::DeclareArgs(_)
                | AnalyzedStatement::Template(_) => {}
            }
        }
        targets
    }
}

pub struct ThinAnalyzedBlock<'i> {
    pub variables: HashSet<AnalyzedVariable<'i>>,
    pub templates: HashSet<AnalyzedTemplate<'i>>,
    pub targets: HashSet<AnalyzedTarget<'i>>,
}

impl ThinAnalyzedBlock<'_> {
    pub fn new() -> Self {
        ThinAnalyzedBlock {
            variables: HashSet::new(),
            templates: HashSet::new(),
            targets: HashSet::new(),
        }
    }

    pub fn merge(&mut self, other: &Self) {
        self.variables.extend(other.variables.clone());
        self.templates.extend(other.templates.clone());
        self.targets.extend(other.targets.clone());
    }
}

pub enum AnalyzedStatement<'i> {
    Conditions(Vec<AnalyzedBlock<'i>>),
    Import(AnalyzedImport<'i>),
    DeclareArgs(AnalyzedBlock<'i>),
    NewVariable(AnalyzedNewVariable<'i>),
    ModifyVariable(AnalyzedModifyVariable<'i>),
    Template(AnalyzedTemplate<'i>),
    Target(AnalyzedTarget<'i>),
    NewScope(AnalyzedBlock<'i>),
}

pub struct AnalyzedImport<'i> {
    pub file: Arc<ThinAnalyzedFile>,
    pub span: Span<'i>,
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct AnalyzedVariable<'i> {
    pub name: &'i str,
    pub value: Option<Expr<'i>>,
    pub document: &'i Document,
    pub span: Span<'i>,
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct AnalyzedNewVariable<'i> {
    pub name: &'i str,
    pub value: Expr<'i>,
    pub document: &'i Document,
    pub span: Span<'i>,
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct AnalyzedModifyVariable<'i> {
    pub name: &'i str,
    pub document: &'i Document,
    pub span: Span<'i>,
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct AnalyzedTemplate<'i> {
    pub name: &'i str,
    pub comments: Option<String>,
    pub document: &'i Document,
    pub header: Span<'i>,
    pub span: Span<'i>,
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct AnalyzedTarget<'i> {
    pub name: &'i str,
    pub document: &'i Document,
    pub header: Span<'i>,
    pub span: Span<'i>,
}

pub enum Link<'i> {
    /// Link to a file. No range is specified.
    File { path: PathBuf, span: Span<'i> },
    /// Link to a target defined in a BUILD.gn file.
    Target {
        path: PathBuf,
        name: &'i str,
        span: Span<'i>,
    },
}

#[allow(clippy::manual_map)]
fn resolve_label<'s>(
    label: &'s str,
    current_path: &Path,
    workspace: &WorkspaceContext,
) -> Option<(PathBuf, &'s str)> {
    if let Some((prefix, name)) = label.split_once(':') {
        if prefix.is_empty() {
            Some((current_path.to_path_buf(), name))
        } else if let Some(rel_dir) = prefix.strip_prefix("//") {
            Some((workspace.root.join(rel_dir).join("BUILD.gn"), name))
        } else {
            None
        }
    } else if let Some(rel_dir) = label.strip_prefix("//") {
        if !rel_dir.is_empty() {
            Some((
                workspace.root.join(rel_dir).join("BUILD.gn"),
                rel_dir.split('/').last().unwrap(),
            ))
        } else {
            None
        }
    } else {
        None
    }
}

fn collect_links<'i>(
    ast_root: &Block<'i>,
    path: &Path,
    workspace: &WorkspaceContext,
) -> Vec<Link<'i>> {
    ast_root
        .strings()
        .filter_map(|string| {
            let content = parse_simple_literal(string.raw_value)?;
            if !content.contains(":") && content.contains(".") {
                let path = workspace.resolve_path(content, path);
                if let Ok(true) = path.try_exists() {
                    return Some(Link::File {
                        path: path.to_path_buf(),
                        span: string.span,
                    });
                }
            } else if let Some((build_gn_path, name)) = resolve_label(content, path, workspace) {
                return Some(Link::Target {
                    path: build_gn_path,
                    name,
                    span: string.span,
                });
            }
            None
        })
        .collect()
}

#[allow(deprecated)]
fn collect_symbols(node: &dyn Node, line_index: &LineIndex) -> Vec<DocumentSymbol> {
    let mut symbols = Vec::new();
    if let Some(statement) = node.as_statement() {
        match statement {
            Statement::Assignment(assignment) => {
                symbols.push(DocumentSymbol {
                    name: format!(
                        "{} {} ...",
                        assignment.lvalue.span().as_str(),
                        assignment.op
                    ),
                    detail: None,
                    kind: SymbolKind::VARIABLE,
                    tags: None,
                    deprecated: None,
                    range: line_index.range(assignment.span()),
                    selection_range: line_index.range(assignment.lvalue.span()),
                    children: Some(collect_symbols(assignment.rvalue.as_node(), line_index)),
                });
            }
            Statement::Call(call) => {
                if let Some(block) = &call.block {
                    symbols.push(DocumentSymbol {
                        name: call.function.name.to_string(),
                        detail: None,
                        kind: SymbolKind::FUNCTION,
                        tags: None,
                        deprecated: None,
                        range: line_index.range(call.span()),
                        selection_range: line_index.range(call.function.span()),
                        children: Some(collect_symbols(block.as_node(), line_index)),
                    });
                }
            }
            Statement::Condition(condition) => {
                symbols.push(DocumentSymbol {
                    name: format!("if ({})", condition.condition.span().as_str()),
                    detail: None,
                    kind: SymbolKind::NAMESPACE,
                    tags: None,
                    deprecated: None,
                    range: line_index.range(condition.span()),
                    selection_range: line_index.range(condition.condition.span()),
                    children: Some(collect_symbols(condition.then_block.as_node(), line_index)),
                });
                let mut next_else_block = &condition.else_block;
                loop {
                    match next_else_block {
                        None => break,
                        Some(Either::Left(condition)) => {
                            symbols.push(DocumentSymbol {
                                name: format!("else if ({})", condition.condition.span().as_str()),
                                detail: None,
                                kind: SymbolKind::NAMESPACE,
                                tags: None,
                                deprecated: None,
                                range: line_index.range(condition.span()),
                                selection_range: line_index.range(condition.condition.span()),
                                children: Some(collect_symbols(
                                    condition.then_block.as_node(),
                                    line_index,
                                )),
                            });
                            next_else_block = &condition.else_block;
                        }
                        Some(Either::Right(block)) => {
                            symbols.push(DocumentSymbol {
                                name: "else".to_string(),
                                detail: None,
                                kind: SymbolKind::NAMESPACE,
                                tags: None,
                                deprecated: None,
                                range: line_index.range(block.span()),
                                selection_range: line_index.range(block.span()),
                                children: Some(collect_symbols(block.as_node(), line_index)),
                            });
                            break;
                        }
                    }
                }
            }
            Statement::Unknown(_) => {}
            Statement::UnmatchedBrace(_) => {}
        }
    } else {
        for child in node.children() {
            symbols.extend(collect_symbols(child, line_index));
        }
    }
    symbols
}

#[derive(Debug)]
struct LoopError {
    cycle: Vec<PathBuf>,
}

impl std::fmt::Display for LoopError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Cycle detected: ")?;
        for (i, path) in self.cycle.iter().enumerate() {
            if i > 0 {
                write!(f, " -> ")?;
            }
            write!(f, "{}", path.to_string_lossy())?;
        }
        Ok(())
    }
}

impl std::error::Error for LoopError {}

impl From<LoopError> for std::io::Error {
    fn from(err: LoopError) -> Self {
        std::io::Error::new(std::io::ErrorKind::InvalidData, err)
    }
}

pub struct Analyzer {
    storage: DocumentStorage,
    cache_workspace: BTreeMap<PathBuf, WorkspaceContext>,
    cache_fat: BTreeMap<PathBuf, Arc<AnalyzedFile>>,
    cache_thin: BTreeMap<PathBuf, Arc<ThinAnalyzedFile>>,
}

impl Analyzer {
    pub fn new(storage: DocumentStorage) -> Self {
        Self {
            storage,
            cache_workspace: BTreeMap::new(),
            cache_fat: BTreeMap::new(),
            cache_thin: BTreeMap::new(),
        }
    }

    pub fn storage_mut(&mut self) -> &mut DocumentStorage {
        &mut self.storage
    }

    pub fn analyze(&mut self, path: &Path) -> std::io::Result<Arc<AnalyzedFile>> {
        let path = path.canonicalize()?;
        let workspace = self.workspace_for(&path)?;
        self.analyze_fat_cached(&path, &workspace)
    }

    fn workspace_for(&mut self, path: &Path) -> std::io::Result<WorkspaceContext> {
        let workspace_root = find_workspace_root(path)?;
        if let Some(workspace) = self.cache_workspace.get(&workspace_root) {
            return Ok(workspace.clone());
        }

        let workspace = WorkspaceContext {
            root: workspace_root.clone(),
            build_config: evaluate_dot_gn(&workspace_root.join(".gn"))?,
        };
        self.cache_workspace
            .insert(workspace_root, workspace.clone());
        Ok(workspace)
    }

    fn analyze_fat_cached(
        &mut self,
        path: &Path,
        workspace: &WorkspaceContext,
    ) -> std::io::Result<Arc<AnalyzedFile>> {
        if let Some(cached_file) = self.cache_fat.get(path) {
            if &cached_file.workspace == workspace {
                let latest_version = self.storage.read_version(path)?;
                if latest_version == cached_file.document.version {
                    return Ok(cached_file.clone());
                }
            }
        }

        let new_file = self.analyze_fat_uncached(path, workspace)?;
        self.cache_fat.insert(path.to_path_buf(), new_file.clone());

        Ok(new_file)
    }

    fn analyze_fat_uncached(
        &mut self,
        path: &Path,
        workspace: &WorkspaceContext,
    ) -> std::io::Result<Arc<AnalyzedFile>> {
        let document = self.storage.read(path)?;
        let ast_root = parse(&document.data);

        let mut analyzed_root = self.analyze_fat_block(&ast_root, workspace, &document)?;

        // Insert a synthetic import of BUILDCONFIG.gn.
        analyzed_root.statements.insert(
            0,
            AnalyzedStatement::Import(AnalyzedImport {
                file: self.analyze_thin_cached(
                    &workspace.build_config,
                    workspace,
                    &mut Vec::new(),
                )?,
                span: Span::new(&document.data, 0, 0).unwrap(),
            }),
        );

        // SAFETY: *_block's contents are backed by document.data that are guaranteed to have
        // the identical lifetime because AnalyzedFile in Arc is immutable.
        let ast_root = unsafe { std::mem::transmute::<Block, Block>(ast_root) };
        let analyzed_root =
            unsafe { std::mem::transmute::<AnalyzedBlock, AnalyzedBlock>(analyzed_root) };
        let links = collect_links(&ast_root, path, workspace);
        let symbols = collect_symbols(&ast_root, &document.line_index);
        Ok(Arc::new(AnalyzedFile {
            document,
            workspace: workspace.clone(),
            ast_root,
            analyzed_root,
            links,
            symbols,
        }))
    }

    fn analyze_thin_cached(
        &mut self,
        path: &Path,
        workspace: &WorkspaceContext,
        actives: &mut Vec<PathBuf>,
    ) -> std::io::Result<Arc<ThinAnalyzedFile>> {
        if let Some(cached_file) = self.cache_thin.get(path) {
            if &cached_file.workspace == workspace {
                let latest_version = self.storage.read_version(path)?;
                if latest_version == cached_file.document.version {
                    return Ok(cached_file.clone());
                }
            }
        }

        let new_file = self.analyze_thin_uncached(path, workspace, actives)?;
        self.cache_thin.insert(path.to_path_buf(), new_file.clone());

        Ok(new_file)
    }

    fn analyze_thin_uncached(
        &mut self,
        path: &Path,
        workspace: &WorkspaceContext,
        actives: &mut Vec<PathBuf>,
    ) -> std::io::Result<Arc<ThinAnalyzedFile>> {
        if actives.iter().any(|p| p == path) {
            return Err(LoopError {
                cycle: std::mem::take(actives),
            }
            .into());
        }

        actives.push(path.to_path_buf());
        let result = self.analyze_thin_uncached_inner(path, workspace, actives);
        actives.pop();
        result
    }

    fn analyze_thin_uncached_inner(
        &mut self,
        path: &Path,
        workspace: &WorkspaceContext,
        actives: &mut Vec<PathBuf>,
    ) -> std::io::Result<Arc<ThinAnalyzedFile>> {
        let document = match self.storage.read(path) {
            Ok(document) => document,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                // Ignore missing imports as they might be imported conditionally.
                return Ok(ThinAnalyzedFile::empty(path, workspace));
            }
            Err(err) => return Err(err),
        };
        let block = parse(&document.data);

        let analyzed_root = self.analyze_thin_block(&block, workspace, &document, actives)?;

        // SAFETY: analyzed_root's contents are backed by document.data that are guaranteed to have
        // the identical lifetime because AnalyzedFile in Arc is immutable.
        let analyzed_root =
            unsafe { std::mem::transmute::<ThinAnalyzedBlock, ThinAnalyzedBlock>(analyzed_root) };
        Ok(Arc::new(ThinAnalyzedFile {
            document,
            workspace: workspace.clone(),
            analyzed_root,
        }))
    }

    fn analyze_fat_block<'i>(
        &mut self,
        block: &Block<'i>,
        workspace: &WorkspaceContext,
        document: &'i Document,
    ) -> std::io::Result<AnalyzedBlock<'i>> {
        let statements: Vec<AnalyzedStatement> = block
            .statements
            .iter()
            .map(|statement| -> std::io::Result<Vec<AnalyzedStatement>> {
                match statement {
                    Statement::Assignment(assignment) => {
                        let mut statements = Vec::new();
                        let name = match &assignment.lvalue {
                            LValue::Identifier(identifier) => identifier.name,
                            LValue::ArrayAccess(array_access) => array_access.array.name,
                            LValue::ScopeAccess(scope_access) => scope_access.scope.name,
                        };
                        if assignment.op == AssignOp::Assign
                            && matches!(&assignment.lvalue, LValue::Identifier(_))
                        {
                            statements.push(AnalyzedStatement::NewVariable(AnalyzedNewVariable {
                                name,
                                value: (*assignment.rvalue).clone(),
                                document,
                                span: assignment.span,
                            }));
                        } else {
                            statements.push(AnalyzedStatement::ModifyVariable(
                                AnalyzedModifyVariable {
                                    name,
                                    document,
                                    span: assignment.span,
                                },
                            ));
                        }
                        statements.extend(self.analyze_fat_expr(
                            &assignment.rvalue,
                            workspace,
                            document,
                        )?);
                        Ok(statements)
                    }
                    Statement::Call(call) => {
                        match call.function.name {
                            "import" => {
                                if let Some(name) = call
                                    .args
                                    .iter()
                                    .exactly_one()
                                    .ok()
                                    .and_then(|expr| expr.as_primary_string())
                                    .and_then(|s| parse_simple_literal(s.raw_value))
                                {
                                    let path = workspace
                                        .resolve_path(name, document.path.parent().unwrap());
                                    let file = match self.analyze_thin_cached(
                                        &path,
                                        workspace,
                                        &mut Vec::new(),
                                    ) {
                                        Err(err) if err.kind() == ErrorKind::NotFound => {
                                            // Ignore missing imports as they might be imported conditionally.
                                            ThinAnalyzedFile::empty(&path, workspace)
                                        }
                                        other => other?,
                                    };
                                    Ok(vec![AnalyzedStatement::Import(AnalyzedImport {
                                        file,
                                        span: call.span(),
                                    })])
                                } else {
                                    Ok(Vec::new())
                                }
                            }
                            "template" => {
                                let mut statements = Vec::new();
                                if let Some(name) = call
                                    .args
                                    .iter()
                                    .exactly_one()
                                    .ok()
                                    .and_then(|expr| expr.as_primary_string())
                                    .and_then(|s| parse_simple_literal(s.raw_value))
                                {
                                    statements.push(AnalyzedStatement::Template(
                                        AnalyzedTemplate {
                                            name,
                                            comments: call
                                                .comments
                                                .as_ref()
                                                .map(|comments| comments.text.clone()),
                                            document,
                                            header: call.function.span,
                                            span: call.span,
                                        },
                                    ));
                                }
                                if let Some(block) = &call.block {
                                    statements.push(AnalyzedStatement::NewScope(
                                        self.analyze_fat_block(block, workspace, document)?,
                                    ));
                                }
                                Ok(statements)
                            }
                            "declare_args" => {
                                if let Some(block) = &call.block {
                                    let analyzed_root =
                                        self.analyze_fat_block(block, workspace, document)?;
                                    Ok(vec![AnalyzedStatement::DeclareArgs(analyzed_root)])
                                } else {
                                    Ok(Vec::new())
                                }
                            }
                            "foreach" => {
                                if let Some(block) = &call.block {
                                    Ok(self
                                        .analyze_fat_block(block, workspace, document)?
                                        .statements)
                                } else {
                                    Ok(Vec::new())
                                }
                            }
                            "set_defaults" => {
                                if let Some(block) = &call.block {
                                    let analyzed_root =
                                        self.analyze_fat_block(block, workspace, document)?;
                                    Ok(vec![AnalyzedStatement::NewScope(analyzed_root)])
                                } else {
                                    Ok(Vec::new())
                                }
                            }
                            _ => {
                                let mut statements = Vec::new();
                                if let Some(name) = call
                                    .args
                                    .iter()
                                    .exactly_one()
                                    .ok()
                                    .and_then(|expr| expr.as_primary_string())
                                    .and_then(|s| parse_simple_literal(s.raw_value))
                                {
                                    statements.push(AnalyzedStatement::Target(AnalyzedTarget {
                                        name,
                                        document,
                                        header: call.args[0].span(),
                                        span: call.span,
                                    }));
                                }
                                if let Some(block) = &call.block {
                                    statements.push(AnalyzedStatement::NewScope(
                                        self.analyze_fat_block(block, workspace, document)?,
                                    ));
                                }
                                Ok(statements)
                            }
                        }
                    }
                    Statement::Condition(condition) => {
                        let mut statements = Vec::new();
                        let mut condition_blocks = Vec::new();
                        let mut current_condition = condition;
                        loop {
                            statements.extend(self.analyze_fat_expr(
                                &current_condition.condition,
                                workspace,
                                document,
                            )?);
                            condition_blocks.push(self.analyze_fat_block(
                                &current_condition.then_block,
                                workspace,
                                document,
                            )?);
                            match &current_condition.else_block {
                                None => break,
                                Some(Either::Left(next_condition)) => {
                                    current_condition = next_condition;
                                }
                                Some(Either::Right(block)) => {
                                    condition_blocks
                                        .push(self.analyze_fat_block(block, workspace, document)?);
                                    break;
                                }
                            }
                        }
                        statements.push(AnalyzedStatement::Conditions(condition_blocks));
                        Ok(statements)
                    }
                    Statement::Unknown(_) | Statement::UnmatchedBrace(_) => Ok(Vec::new()),
                }
            })
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();

        Ok(AnalyzedBlock {
            statements,
            span: block.span,
        })
    }

    fn analyze_fat_expr<'i>(
        &mut self,
        expr: &Expr<'i>,
        workspace: &WorkspaceContext,
        document: &'i Document,
    ) -> std::io::Result<Vec<AnalyzedStatement<'i>>> {
        match expr {
            Expr::Primary(primary_expr) => match primary_expr {
                PrimaryExpr::Block(block) => {
                    let analyzed_root = self.analyze_fat_block(block, workspace, document)?;
                    Ok(vec![AnalyzedStatement::NewScope(analyzed_root)])
                }
                PrimaryExpr::Call(call) => {
                    let mut statements: Vec<_> = call
                        .args
                        .iter()
                        .map(|expr| self.analyze_fat_expr(expr, workspace, document))
                        .collect::<std::io::Result<Vec<_>>>()?
                        .into_iter()
                        .flatten()
                        .collect();
                    if let Some(block) = &call.block {
                        let analyzed_root = self.analyze_fat_block(block, workspace, document)?;
                        statements.push(AnalyzedStatement::NewScope(analyzed_root));
                    }
                    Ok(statements)
                }
                PrimaryExpr::ParenExpr(expr) => self.analyze_fat_expr(expr, workspace, document),
                PrimaryExpr::List(list_literal) => Ok(list_literal
                    .values
                    .iter()
                    .map(|expr| self.analyze_fat_expr(expr, workspace, document))
                    .collect::<std::io::Result<Vec<_>>>()?
                    .into_iter()
                    .flatten()
                    .collect()),
                PrimaryExpr::Identifier(_)
                | PrimaryExpr::Integer(_)
                | PrimaryExpr::String(_)
                | PrimaryExpr::ArrayAccess(_)
                | PrimaryExpr::ScopeAccess(_) => Ok(Vec::new()),
            },
            Expr::Unary(unary_expr) => self.analyze_fat_expr(&unary_expr.expr, workspace, document),
            Expr::Binary(binary_expr) => {
                let mut statements =
                    self.analyze_fat_expr(&binary_expr.lhs, workspace, document)?;
                statements.extend(self.analyze_fat_expr(&binary_expr.rhs, workspace, document)?);
                Ok(statements)
            }
        }
    }

    fn analyze_thin_block<'i>(
        &mut self,
        block: &Block<'i>,
        workspace: &WorkspaceContext,
        document: &'i Document,
        actives: &mut Vec<PathBuf>,
    ) -> std::io::Result<ThinAnalyzedBlock<'i>> {
        let mut analyzed_block = ThinAnalyzedBlock::new();

        for statement in &block.statements {
            match statement {
                Statement::Assignment(assignment) => {
                    if let LValue::Identifier(identifier) = &assignment.lvalue {
                        if is_exported(identifier.name) && assignment.op == AssignOp::Assign {
                            analyzed_block.variables.insert(AnalyzedVariable {
                                name: identifier.name,
                                value: Some((*assignment.rvalue).clone()),
                                document,
                                span: assignment.span,
                            });
                        }
                    }
                    let name = match &assignment.lvalue {
                        LValue::Identifier(identifier) => identifier.name,
                        LValue::ArrayAccess(array_access) => array_access.array.name,
                        LValue::ScopeAccess(scope_access) => scope_access.scope.name,
                    };
                    if is_exported(name)
                        && (assignment.op != AssignOp::Assign
                            || !matches!(&assignment.lvalue, LValue::Identifier(_)))
                    {
                        analyzed_block.variables = analyzed_block
                            .variables
                            .into_iter()
                            .map(|mut variable| {
                                if variable.name == name {
                                    variable.value = None;
                                }
                                variable
                            })
                            .collect();
                    }
                }
                Statement::Call(call) => match call.function.name {
                    "import" => {
                        if let Some(name) = call
                            .args
                            .iter()
                            .exactly_one()
                            .ok()
                            .and_then(|expr| expr.as_primary_string())
                            .and_then(|s| parse_simple_literal(s.raw_value))
                        {
                            let path =
                                workspace.resolve_path(name, document.path.parent().unwrap());
                            let file = self.analyze_thin_cached(&path, workspace, actives)?;
                            analyzed_block.merge(&file.analyzed_root);
                        }
                    }
                    "template" => {
                        if let Some(name) = call
                            .args
                            .iter()
                            .exactly_one()
                            .ok()
                            .and_then(|expr| expr.as_primary_string())
                            .and_then(|s| parse_simple_literal(s.raw_value))
                        {
                            if is_exported(name) {
                                analyzed_block.templates.insert(AnalyzedTemplate {
                                    name,
                                    comments: call
                                        .comments
                                        .as_ref()
                                        .map(|comments| comments.text.clone()),
                                    document,
                                    header: call.function.span,
                                    span: call.span,
                                });
                            }
                        }
                    }
                    "declare_args" | "foreach" => {
                        if let Some(block) = &call.block {
                            analyzed_block.merge(
                                &self.analyze_thin_block(block, workspace, document, actives)?,
                            );
                        }
                    }
                    "set_defaults" => {}
                    _ => {
                        if let Some(name) = call
                            .args
                            .iter()
                            .exactly_one()
                            .ok()
                            .and_then(|expr| expr.as_primary_string())
                            .and_then(|s| parse_simple_literal(s.raw_value))
                        {
                            analyzed_block.targets.insert(AnalyzedTarget {
                                name,
                                document,
                                header: call.args[0].span(),
                                span: call.span,
                            });
                        }
                    }
                },
                Statement::Condition(condition) => {
                    let mut current_condition = condition;
                    loop {
                        analyzed_block.merge(&self.analyze_thin_block(
                            &current_condition.then_block,
                            workspace,
                            document,
                            actives,
                        )?);
                        match &current_condition.else_block {
                            None => break,
                            Some(Either::Left(next_condition)) => {
                                current_condition = next_condition;
                            }
                            Some(Either::Right(block)) => {
                                analyzed_block.merge(
                                    &self
                                        .analyze_thin_block(block, workspace, document, actives)?,
                                );
                                break;
                            }
                        }
                    }
                }
                Statement::Unknown(_) | Statement::UnmatchedBrace(_) => {}
            }
        }

        Ok(analyzed_block)
    }
}
