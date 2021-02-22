use crate::base::ModuleData;
use rustc_ast::ptr::P;
use rustc_ast::{token, Attribute, Item};
use rustc_errors::{struct_span_err, PResult};
use rustc_parse::new_parser_from_file;
use rustc_session::parse::ParseSess;
use rustc_session::Session;
use rustc_span::symbol::{sym, Ident};
use rustc_span::Span;

use std::path::{self, Path, PathBuf};

#[derive(Copy, Clone)]
pub enum DirectoryOwnership {
    Owned {
        // None if `mod.rs`, `Some("foo")` if we're in `foo.rs`.
        relative: Option<Ident>,
    },
    UnownedViaBlock,
}

/// Information about the path to a module.
// Public for rustfmt usage.
pub struct ModulePath<'a> {
    name: String,
    path_exists: bool,
    pub result: PResult<'a, ModulePathSuccess>,
}

// Public for rustfmt usage.
pub struct ModulePathSuccess {
    pub path: PathBuf,
    pub ownership: DirectoryOwnership,
}

crate struct ParsedExternalMod {
    pub items: Vec<P<Item>>,
    pub inner_span: Span,
    pub file_path: PathBuf,
    pub dir_path: PathBuf,
    pub dir_ownership: DirectoryOwnership,
}

crate fn parse_external_mod(
    sess: &Session,
    id: Ident,
    span: Span, // The span to blame on errors.
    module: &ModuleData,
    mut dir_ownership: DirectoryOwnership,
    attrs: &mut Vec<Attribute>,
) -> ParsedExternalMod {
    // We bail on the first error, but that error does not cause a fatal error... (1)
    let result: PResult<'_, _> = try {
        // Extract the file path and the new ownership.
        let mp = submod_path(sess, id, span, &attrs, dir_ownership, &module.dir_path)?;
        dir_ownership = mp.ownership;

        // Ensure file paths are acyclic.
        error_on_circular_module(&sess.parse_sess, span, &mp.path, &module.file_path_stack)?;

        // Actually parse the external file as a module.
        let mut parser = new_parser_from_file(&sess.parse_sess, &mp.path, Some(span));
        let (mut inner_attrs, items, inner_span) = parser.parse_mod(&token::Eof)?;
        attrs.append(&mut inner_attrs);
        (items, inner_span, mp.path)
    };
    // (1) ...instead, we return a dummy module.
    let (items, inner_span, file_path) = result.map_err(|mut err| err.emit()).unwrap_or_default();

    // Extract the directory path for submodules of the module.
    let dir_path = file_path.parent().unwrap_or(&file_path).to_owned();

    ParsedExternalMod { items, inner_span, file_path, dir_path, dir_ownership }
}

fn error_on_circular_module<'a>(
    sess: &'a ParseSess,
    span: Span,
    path: &Path,
    file_path_stack: &[PathBuf],
) -> PResult<'a, ()> {
    if let Some(i) = file_path_stack.iter().position(|p| *p == path) {
        let mut err = String::from("circular modules: ");
        for p in &file_path_stack[i..] {
            err.push_str(&p.to_string_lossy());
            err.push_str(" -> ");
        }
        err.push_str(&path.to_string_lossy());
        return Err(sess.span_diagnostic.struct_span_err(span, &err[..]));
    }
    Ok(())
}

crate fn push_directory(
    sess: &Session,
    id: Ident,
    attrs: &[Attribute],
    module: &ModuleData,
    mut dir_ownership: DirectoryOwnership,
) -> (PathBuf, DirectoryOwnership) {
    let mut dir_path = module.dir_path.clone();
    if let Some(file_path) = sess.first_attr_value_str_by_name(attrs, sym::path) {
        dir_path.push(&*file_path.as_str());
        dir_ownership = DirectoryOwnership::Owned { relative: None };
    } else {
        // We have to push on the current module name in the case of relative
        // paths in order to ensure that any additional module paths from inline
        // `mod x { ... }` come after the relative extension.
        //
        // For example, a `mod z { ... }` inside `x/y.rs` should set the current
        // directory path to `/x/y/z`, not `/x/z` with a relative offset of `y`.
        if let DirectoryOwnership::Owned { relative } = &mut dir_ownership {
            if let Some(ident) = relative.take() {
                // Remove the relative offset.
                dir_path.push(&*ident.as_str());
            }
        }
        dir_path.push(&*id.as_str());
    }

    (dir_path, dir_ownership)
}

fn submod_path<'a>(
    sess: &'a Session,
    id: Ident,
    span: Span,
    attrs: &[Attribute],
    ownership: DirectoryOwnership,
    dir_path: &Path,
) -> PResult<'a, ModulePathSuccess> {
    if let Some(path) = submod_path_from_attr(sess, attrs, dir_path) {
        // All `#[path]` files are treated as though they are a `mod.rs` file.
        // This means that `mod foo;` declarations inside `#[path]`-included
        // files are siblings,
        //
        // Note that this will produce weirdness when a file named `foo.rs` is
        // `#[path]` included and contains a `mod foo;` declaration.
        // If you encounter this, it's your own darn fault :P
        let ownership = DirectoryOwnership::Owned { relative: None };
        return Ok(ModulePathSuccess { ownership, path });
    }

    let relative = match ownership {
        DirectoryOwnership::Owned { relative } => relative,
        DirectoryOwnership::UnownedViaBlock => None,
    };
    let ModulePath { path_exists, name, result } =
        default_submod_path(&sess.parse_sess, id, span, relative, dir_path);
    match ownership {
        DirectoryOwnership::Owned { .. } => Ok(result?),
        DirectoryOwnership::UnownedViaBlock => {
            let _ = result.map_err(|mut err| err.cancel());
            error_decl_mod_in_block(&sess.parse_sess, span, path_exists, &name)
        }
    }
}

fn error_decl_mod_in_block<'a, T>(
    sess: &'a ParseSess,
    span: Span,
    path_exists: bool,
    name: &str,
) -> PResult<'a, T> {
    let msg = "Cannot declare a non-inline module inside a block unless it has a path attribute";
    let mut err = sess.span_diagnostic.struct_span_err(span, msg);
    if path_exists {
        let msg = format!("Maybe `use` the module `{}` instead of redeclaring it", name);
        err.span_note(span, &msg);
    }
    Err(err)
}

/// Derive a submodule path from the first found `#[path = "path_string"]`.
/// The provided `dir_path` is joined with the `path_string`.
pub(super) fn submod_path_from_attr(
    sess: &Session,
    attrs: &[Attribute],
    dir_path: &Path,
) -> Option<PathBuf> {
    // Extract path string from first `#[path = "path_string"]` attribute.
    let path_string = sess.first_attr_value_str_by_name(attrs, sym::path)?;
    let path_string = path_string.as_str();

    // On windows, the base path might have the form
    // `\\?\foo\bar` in which case it does not tolerate
    // mixed `/` and `\` separators, so canonicalize
    // `/` to `\`.
    #[cfg(windows)]
    let path_string = path_string.replace("/", "\\");

    Some(dir_path.join(&*path_string))
}

/// Returns a path to a module.
// Public for rustfmt usage.
pub fn default_submod_path<'a>(
    sess: &'a ParseSess,
    id: Ident,
    span: Span,
    relative: Option<Ident>,
    dir_path: &Path,
) -> ModulePath<'a> {
    // If we're in a foo.rs file instead of a mod.rs file,
    // we need to look for submodules in
    // `./foo/<id>.rs` and `./foo/<id>/mod.rs` rather than
    // `./<id>.rs` and `./<id>/mod.rs`.
    let relative_prefix_string;
    let relative_prefix = if let Some(ident) = relative {
        relative_prefix_string = format!("{}{}", ident.name, path::MAIN_SEPARATOR);
        &relative_prefix_string
    } else {
        ""
    };

    let mod_name = id.name.to_string();
    let default_path_str = format!("{}{}.rs", relative_prefix, mod_name);
    let secondary_path_str =
        format!("{}{}{}mod.rs", relative_prefix, mod_name, path::MAIN_SEPARATOR);
    let default_path = dir_path.join(&default_path_str);
    let secondary_path = dir_path.join(&secondary_path_str);
    let default_exists = sess.source_map().file_exists(&default_path);
    let secondary_exists = sess.source_map().file_exists(&secondary_path);

    let result = match (default_exists, secondary_exists) {
        (true, false) => Ok(ModulePathSuccess {
            path: default_path,
            ownership: DirectoryOwnership::Owned { relative: Some(id) },
        }),
        (false, true) => Ok(ModulePathSuccess {
            path: secondary_path,
            ownership: DirectoryOwnership::Owned { relative: None },
        }),
        (false, false) => {
            let mut err = struct_span_err!(
                sess.span_diagnostic,
                span,
                E0583,
                "file not found for module `{}`",
                mod_name,
            );
            err.help(&format!(
                "to create the module `{}`, create file \"{}\"",
                mod_name,
                default_path.display(),
            ));
            Err(err)
        }
        (true, true) => {
            let mut err = struct_span_err!(
                sess.span_diagnostic,
                span,
                E0761,
                "file for module `{}` found at both {} and {}",
                mod_name,
                default_path_str,
                secondary_path_str,
            );
            err.help("delete or rename one of them to remove the ambiguity");
            Err(err)
        }
    };

    ModulePath { name: mod_name, path_exists: default_exists || secondary_exists, result }
}
