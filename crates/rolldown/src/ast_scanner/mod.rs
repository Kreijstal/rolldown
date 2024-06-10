pub mod impl_visit;
pub mod side_effect_detector;

use oxc::{
  ast::{
    ast::{
      ExportAllDeclaration, ExportDefaultDeclaration, ExportNamedDeclaration, IdentifierReference,
      ImportDeclaration, ModuleDeclaration, Program,
    },
    Trivias, Visit,
  },
  semantic::SymbolId,
  span::{Atom, CompactStr, GetSpan, Span},
};
use oxc_index::IndexVec;
use rolldown_common::{
  AstScopes, ExportsKind, ImportKind, ImportRecordId, LocalExport, ModuleDefFormat, NamedImport,
  NormalModuleId, RawImportRecord, ResourceId, Specifier, StmtInfo, StmtInfos, SymbolRef,
};
use rolldown_error::BuildError;
use rolldown_oxc_utils::{BindingIdentifierExt, BindingPatternExt};
use rolldown_rstr::{Rstr, ToRstr};
use rolldown_utils::path_ext::PathExt;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;
use sugar_path::SugarPath;

use super::types::ast_symbols::AstSymbols;

#[derive(Debug)]
pub struct ScanResult {
  pub repr_name: String,
  pub named_imports: FxHashMap<SymbolRef, NamedImport>,
  pub named_exports: FxHashMap<Rstr, LocalExport>,
  pub stmt_infos: StmtInfos,
  pub import_records: IndexVec<ImportRecordId, RawImportRecord>,
  pub star_exports: Vec<ImportRecordId>,
  pub default_export_ref: SymbolRef,
  pub imports: FxHashMap<Span, ImportRecordId>,
  pub exports_kind: ExportsKind,
  pub warnings: Vec<BuildError>,
  pub normalized_dynamic_import_usage: FxHashMap<ImportRecordId, DynamicImportUse>,
}

#[derive(Default, Debug)]
pub enum DynamicImportUse {
  #[default]
  All,
  Partial(FxHashSet<CompactStr>),
}

#[derive(Default, Debug)]
struct DynamicImportUsageCollector {
  /// Each span is the span of `ImportExpression`, because we visit call_expression before visiting import_expression.
  /// and we use the span to find the corresponding import record, finally convert it into resolved
  /// module id
  pub dynamic_import_usage_map: FxHashMap<Span, DynamicImportUse>,
  pub in_import_then_body: bool,
  /// Using span to identify the module id, SymbolRef is used to collect the namespace object
  /// usage
  pub dynamic_module_ref: Option<(Span, SymbolRef)>,
}

pub struct AstScanner<'me> {
  idx: NormalModuleId,
  source: &'me Arc<str>,
  module_type: ModuleDefFormat,
  file_path: &'me ResourceId,
  scopes: &'me AstScopes,
  trivias: &'me Trivias,
  symbols: &'me mut AstSymbols,
  current_stmt_info: StmtInfo,
  result: ScanResult,
  esm_export_keyword: Option<Span>,
  esm_import_keyword: Option<Span>,
  /// Represents [Module Namespace Object](https://tc39.es/ecma262/#sec-module-namespace-exotic-objects)
  pub namespace_object_ref: SymbolRef,
  used_exports_ref: bool,
  used_module_ref: bool,
  dynamic_import_usage_collector: DynamicImportUsageCollector,
}

impl<'me> AstScanner<'me> {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    idx: NormalModuleId,
    scope: &'me AstScopes,
    symbols: &'me mut AstSymbols,
    repr_name: String,
    module_type: ModuleDefFormat,
    source: &'me Arc<str>,
    file_path: &'me ResourceId,
    trivias: &'me Trivias,
  ) -> Self {
    // This is used for converting "export default foo;" => "var default_symbol = foo;"
    let symbol_id_for_default_export_ref =
      symbols.create_symbol(format!("{repr_name}_default").into(), scope.root_scope_id());

    let name = format!("{repr_name}_ns");
    let namespace_object_ref: SymbolRef =
      (idx, symbols.create_symbol(name.into(), scope.root_scope_id())).into();

    let result = ScanResult {
      repr_name,
      named_imports: FxHashMap::default(),
      named_exports: FxHashMap::default(),
      stmt_infos: {
        let mut stmt_infos = StmtInfos::default();
        // The first `StmtInfo` is used to represent the statement that declares and constructs Module Namespace Object
        stmt_infos.push(StmtInfo::default());
        stmt_infos
      },
      import_records: IndexVec::new(),
      star_exports: Vec::new(),
      default_export_ref: (idx, symbol_id_for_default_export_ref).into(),
      imports: FxHashMap::default(),
      exports_kind: ExportsKind::None,
      warnings: Vec::new(),
      normalized_dynamic_import_usage: Default::default(),
    };

    Self {
      idx,
      scopes: scope,
      symbols,
      current_stmt_info: StmtInfo::default(),
      result,
      esm_export_keyword: None,
      esm_import_keyword: None,
      module_type,
      namespace_object_ref,
      used_exports_ref: false,
      used_module_ref: false,
      source,
      file_path,
      trivias,
      dynamic_import_usage_collector: DynamicImportUsageCollector::default(),
    }
  }

  pub fn scan(mut self, program: &Program<'_>) -> ScanResult {
    self.visit_program(program);
    let mut exports_kind = ExportsKind::None;

    if self.esm_export_keyword.is_some() {
      exports_kind = ExportsKind::Esm;
    } else if self.used_exports_ref || self.used_module_ref {
      exports_kind = ExportsKind::CommonJs;
    } else {
      // TODO(hyf0): Should add warnings if the module type doesn't satisfy the exports kind.
      match self.module_type {
        ModuleDefFormat::CJS | ModuleDefFormat::CjsPackageJson => {
          exports_kind = ExportsKind::CommonJs;
        }
        ModuleDefFormat::EsmMjs | ModuleDefFormat::EsmPackageJson => {
          exports_kind = ExportsKind::Esm;
        }
        ModuleDefFormat::Unknown => {
          if self.esm_import_keyword.is_some() {
            exports_kind = ExportsKind::Esm;
          }
        }
      }
    }
    let normalized_usage = self
      .dynamic_import_usage_collector
      .dynamic_import_usage_map
      .into_iter()
      .filter_map(|(span, usage)| self.result.imports.get(&span).map(|id| (*id, usage)))
      .collect::<FxHashMap<ImportRecordId, DynamicImportUse>>();
    self.result.normalized_dynamic_import_usage = normalized_usage;
    self.result.exports_kind = exports_kind;
    self.result
  }

  fn set_esm_export_keyword(&mut self, span: Span) {
    self.esm_export_keyword.get_or_insert(span);
  }

  fn add_declared_id(&mut self, id: SymbolId) {
    self.current_stmt_info.declared_symbols.push((self.idx, id).into());
  }

  fn get_root_binding(&self, name: &Atom) -> SymbolId {
    self.scopes.get_root_binding(name).expect("must have")
  }

  fn add_import_record(&mut self, module_request: &Atom, kind: ImportKind) -> ImportRecordId {
    // If 'foo' in `import ... from 'foo'` is finally a commonjs module, we will convert the import statement
    // to `var import_foo = __toESM(require_foo())`, so we create a symbol for `import_foo` here. Notice that we
    // just create the symbol. If the symbol is finally used would be determined in the linking stage.
    let namespace_ref: SymbolRef = (
      self.idx,
      self.symbols.create_symbol(
        format!("#LOCAL_NAMESPACE_IN_{}#", self.current_stmt_info.stmt_idx.unwrap_or_default())
          .into(),
        self.scopes.root_scope_id(),
      ),
    )
      .into();
    let rec = RawImportRecord::new(module_request.to_rstr(), kind, namespace_ref);

    let id = self.result.import_records.push(rec);
    self.current_stmt_info.import_records.push(id);
    id
  }

  fn add_named_import(
    &mut self,
    local: SymbolId,
    imported: &str,
    record_id: ImportRecordId,
    span_imported: Span,
  ) {
    self.result.named_imports.insert(
      (self.idx, local).into(),
      NamedImport {
        imported: Rstr::new(imported).into(),
        imported_as: (self.idx, local).into(),
        span_imported,
        record_id,
      },
    );
  }

  fn add_star_import(&mut self, local: SymbolId, record_id: ImportRecordId, span_imported: Span) {
    self.result.named_imports.insert(
      (self.idx, local).into(),
      NamedImport {
        imported: Specifier::Star,
        imported_as: (self.idx, local).into(),
        record_id,
        span_imported,
      },
    );
  }

  fn add_local_export(&mut self, export_name: &Atom, local: SymbolId) {
    self
      .result
      .named_exports
      .insert(export_name.to_rstr(), LocalExport { referenced: (self.idx, local).into() });
  }

  fn add_local_default_export(&mut self, local: SymbolId) {
    self
      .result
      .named_exports
      .insert("default".into(), LocalExport { referenced: (self.idx, local).into() });
  }

  /// Record `export { [imported] as [export_name] } from ...` statement.
  ///
  /// Notice that we will pretend
  /// ```js
  /// export { [imported] as [export_name] } from '...'
  /// ```
  /// to be
  /// ```js
  /// import { [imported] as [generated] } from '...'
  /// export { [generated] as [export_name] }
  /// ```
  /// Reasons are:
  /// - No extra logic for dealing with re-exports concept.
  /// - Cjs compatibility. We need a [generated] binding to holds the value reexport from commonjs. For example
  /// ```js
  /// export { foo } from 'commonjs'
  /// ```
  /// would be converted to
  /// ```js
  /// const import_commonjs = __toESM(require_commonjs())
  /// const [generated] = import_commonjs.foo
  /// export { [generated] as foo }
  /// ```
  /// `export { foo } from 'commonjs'` would be converted to `const import_commonjs = require()` in the linking stage.
  fn add_re_export(
    &mut self,
    export_name: &Atom,
    imported: &Atom,
    record_id: ImportRecordId,
    span_imported: Span,
  ) {
    // We will pretend `export { [imported] as [export_name] }` to be `import `
    let generated_imported_as_ref = (
      self.idx,
      self.symbols.create_symbol(
        if export_name.as_str() == "default" {
          let importee_repr = self.result.import_records[record_id]
            .module_request
            .as_path()
            .representative_file_name();
          format!("{importee_repr}_default").into()
        } else {
          export_name.clone().to_compact_str()
        },
        self.scopes.root_scope_id(),
      ),
    )
      .into();

    self.current_stmt_info.declared_symbols.push(generated_imported_as_ref);
    let name_import = NamedImport {
      imported: imported.to_rstr().into(),
      imported_as: generated_imported_as_ref,
      record_id,
      span_imported,
    };
    if name_import.imported.is_default() {
      self.result.import_records[record_id].contains_import_default = true;
    }
    self.result.named_imports.insert(generated_imported_as_ref, name_import);
    self
      .result
      .named_exports
      .insert(export_name.to_rstr(), LocalExport { referenced: generated_imported_as_ref });
  }

  fn add_star_re_export(
    &mut self,
    export_name: &Atom,
    record_id: ImportRecordId,
    span_for_export_name: Span,
  ) {
    let generated_imported_as_ref = (
      self.idx,
      self.symbols.create_symbol(export_name.to_compact_str(), self.scopes.root_scope_id()),
    )
      .into();
    self.current_stmt_info.declared_symbols.push(generated_imported_as_ref);
    let name_import = NamedImport {
      imported: Specifier::Star,
      span_imported: span_for_export_name,
      imported_as: generated_imported_as_ref,
      record_id,
    };
    self.result.named_imports.insert(generated_imported_as_ref, name_import);
    self.result.import_records[record_id].contains_import_star = true;
    self
      .result
      .named_exports
      .insert(export_name.to_rstr(), LocalExport { referenced: generated_imported_as_ref });
  }

  fn scan_export_all_decl(&mut self, decl: &ExportAllDeclaration) {
    let id = self.add_import_record(&decl.source.value, ImportKind::Import);
    if let Some(exported) = &decl.exported {
      // export * as ns from '...'
      self.add_star_re_export(exported.name(), id, decl.span);
    } else {
      // export * from '...'
      self.result.star_exports.push(id);
    }
    self.result.imports.insert(decl.span, id);
  }

  fn scan_export_named_decl(&mut self, decl: &ExportNamedDeclaration) {
    if let Some(source) = &decl.source {
      let record_id = self.add_import_record(&source.value, ImportKind::Import);
      decl.specifiers.iter().for_each(|spec| {
        self.add_re_export(spec.exported.name(), spec.local.name(), record_id, spec.local.span());
      });
      self.result.imports.insert(decl.span, record_id);
      // `export {} from '...'`
      self.result.import_records[record_id].is_plain_import = decl.specifiers.is_empty();
    } else {
      decl.specifiers.iter().for_each(|spec| {
        self.add_local_export(spec.exported.name(), self.get_root_binding(spec.local.name()));
      });
      if let Some(decl) = decl.declaration.as_ref() {
        match decl {
          oxc::ast::ast::Declaration::VariableDeclaration(var_decl) => {
            var_decl.declarations.iter().for_each(|decl| {
              decl.id.binding_identifiers().into_iter().for_each(|id| {
                self.result.named_exports.insert(
                  id.name.to_rstr(),
                  LocalExport { referenced: (self.idx, id.expect_symbol_id()).into() },
                );
              });
            });
          }
          oxc::ast::ast::Declaration::FunctionDeclaration(fn_decl) => {
            let id = fn_decl.id.as_ref().unwrap();
            self.add_local_export(&id.name, id.expect_symbol_id());
          }
          oxc::ast::ast::Declaration::ClassDeclaration(cls_decl) => {
            let id = cls_decl.id.as_ref().unwrap();
            self.add_local_export(&id.name, id.expect_symbol_id());
          }
          _ => unreachable!("doesn't support ts now"),
        }
      }
    }
  }

  // If the reference is a global variable, `None` will be returned.
  fn resolve_symbol_from_reference(&self, id_ref: &IdentifierReference) -> Option<SymbolId> {
    let ref_id = id_ref.reference_id.get().expect("must have reference id");
    self.scopes.symbol_id_for(ref_id)
  }
  fn scan_export_default_decl(&mut self, decl: &ExportDefaultDeclaration) {
    use oxc::ast::ast::ExportDefaultDeclarationKind;
    let local_binding_for_default_export = match &decl.declaration {
      oxc::ast::match_expression!(ExportDefaultDeclarationKind) => {
        // `export default [expression]` pattern
        None
      }
      oxc::ast::ast::ExportDefaultDeclarationKind::FunctionDeclaration(fn_decl) => {
        fn_decl.id.as_ref().map(rolldown_oxc_utils::BindingIdentifierExt::expect_symbol_id)
      }
      oxc::ast::ast::ExportDefaultDeclarationKind::ClassDeclaration(cls_decl) => {
        cls_decl.id.as_ref().map(rolldown_oxc_utils::BindingIdentifierExt::expect_symbol_id)
      }
      _ => unreachable!(),
    };

    let final_binding =
      local_binding_for_default_export.unwrap_or(self.result.default_export_ref.symbol);

    self.add_declared_id(final_binding);
    self.add_local_default_export(final_binding);
  }

  fn scan_import_decl(&mut self, decl: &ImportDeclaration) {
    let rec_id = self.add_import_record(&decl.source.value, ImportKind::Import);
    self.result.imports.insert(decl.span, rec_id);
    // // `import '...'` or `import {} from '...'`
    self.result.import_records[rec_id].is_plain_import =
      decl.specifiers.as_ref().map_or(true, |s| s.is_empty());

    let Some(specifiers) = &decl.specifiers else { return };
    specifiers.iter().for_each(|spec| match spec {
      oxc::ast::ast::ImportDeclarationSpecifier::ImportSpecifier(spec) => {
        let sym = spec.local.expect_symbol_id();
        let imported = spec.imported.name();
        self.add_named_import(sym, imported, rec_id, spec.imported.span());
        if imported == "default" {
          self.result.import_records[rec_id].contains_import_default = true;
        }
      }
      oxc::ast::ast::ImportDeclarationSpecifier::ImportDefaultSpecifier(spec) => {
        self.add_named_import(spec.local.expect_symbol_id(), "default", rec_id, spec.span);
        self.result.import_records[rec_id].contains_import_default = true;
      }
      oxc::ast::ast::ImportDeclarationSpecifier::ImportNamespaceSpecifier(spec) => {
        self.add_star_import(spec.local.expect_symbol_id(), rec_id, spec.span);
        self.result.import_records[rec_id].contains_import_star = true;
      }
    });
  }
  fn scan_module_decl(&mut self, decl: &ModuleDeclaration) {
    match decl {
      oxc::ast::ast::ModuleDeclaration::ImportDeclaration(decl) => {
        // TODO: this should be the span of `import` keyword, while now it is the span of the whole import declaration.
        self.esm_import_keyword.get_or_insert(decl.span);
        self.scan_import_decl(decl);
      }
      oxc::ast::ast::ModuleDeclaration::ExportAllDeclaration(decl) => {
        self.set_esm_export_keyword(decl.span);
        self.scan_export_all_decl(decl);
      }
      oxc::ast::ast::ModuleDeclaration::ExportNamedDeclaration(decl) => {
        self.set_esm_export_keyword(decl.span);
        self.scan_export_named_decl(decl);
      }
      oxc::ast::ast::ModuleDeclaration::ExportDefaultDeclaration(decl) => {
        self.set_esm_export_keyword(decl.span);
        self.scan_export_default_decl(decl);
      }
      _ => {}
    }
  }

  pub fn add_referenced_symbol(&mut self, id: SymbolId) {
    self.current_stmt_info.referenced_symbols.push((self.idx, id).into());
  }

  pub fn add_member_expr_reference(&mut self, id: SymbolId, chains: Vec<CompactStr>) {
    self.current_stmt_info.referenced_symbols.push((self.idx, id, chains).into());
  }

  fn is_top_level(&self, symbol_id: SymbolId) -> bool {
    self.scopes.root_scope_id() == self.symbols.scope_id_for(symbol_id)
  }

  fn try_diagnostic_forbid_const_assign(&mut self, symbol_id: SymbolId) {
    if self.symbols.get_flag(symbol_id).is_const_variable() {
      for reference in self.scopes.get_resolved_references(symbol_id) {
        if reference.is_write() {
          self.result.warnings.push(
            BuildError::forbid_const_assign(
              self.file_path.to_string(),
              Arc::clone(self.source),
              self.symbols.get_name(symbol_id).into(),
              self.symbols.get_span(symbol_id),
              reference.span(),
            )
            .with_severity_warning(),
          );
        }
      }
    }
  }
}
