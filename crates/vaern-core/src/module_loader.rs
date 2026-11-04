//! Resolves ES-module specifiers to files on disk and loads their source.
//!
//! Relative specifiers (`./x.ts`, `../y.js`) name an exact file next to the
//! importer. Bare specifiers (`pkg`, `@scope/pkg`, `pkg/sub`) are looked up
//! in `node_modules` directories, walking from the importer's directory to
//! the filesystem root as Node.js does, and the first package directory
//! found is the only one consulted.
//!
//! Inside a package, `exports` is honoured with the conditions `vaern`,
//! `import` and `default`, in that order of preference; `node` and `require`
//! are never matched, because a CommonJS entry point cannot run here.
//! Without `exports`, a subpath is an exact file and the root is `module`,
//! then `main` only when the package declares `"type": "module"`.
//!
//! Resolution is local only: nothing is fetched from a registry, and there is
//! no `imports` map, self-reference, CommonJS or JSON support. Every failure
//! is a typed [`VaernError::ModuleResolution`] listing what was tried.

use crate::error::{Result, VaernError};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

/// Conditions matched in an `exports` map. Order is irrelevant here: a
/// conditions object is read in its own key order and the first key that is
/// in this list wins, as Node.js specifies.
const CONDITIONS: [&str; 3] = ["vaern", "import", "default"];

/// Trait defining ES-module resolution and loading behavior.
pub trait ModuleLoader: Send + Sync {
    /// Resolves an import specifier relative to the referrer module path.
    fn resolve(&self, specifier: &str, referrer: &Path) -> Result<PathBuf>;

    /// Loads the source code for a resolved module path.
    fn load(&self, path: &Path) -> Result<String>;
}

/// Standard file-system module loader for relative files and local packages.
#[derive(Debug, Default, Clone)]
pub struct FsModuleLoader;

impl ModuleLoader for FsModuleLoader {
    fn resolve(&self, specifier: &str, referrer: &Path) -> Result<PathBuf> {
        let parent = match referrer.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        let failure = Failure {
            specifier,
            referrer,
        };

        if specifier.starts_with("./") || specifier.starts_with("../") {
            // Normalize without canonicalize, so the module keeps the name
            // the importer used for it.
            let normalized = parent.join(specifier).components().collect::<PathBuf>();
            if !normalized.is_file() {
                let shown = std::path::absolute(&normalized).unwrap_or(normalized);
                return Err(failure.error(
                    "the file does not exist",
                    vec![shown.display().to_string()],
                    "Check the path; relative imports need the exact file name, extension included.",
                ));
            }
            return Ok(normalized);
        }
        if specifier.starts_with("node:") {
            return Err(failure.error(
                "Node.js built-in modules are not provided",
                Vec::new(),
                "Use vaern:fs for files and vaern:http for servers; see docs/subset.md.",
            ));
        }
        if specifier.starts_with("vaern:") {
            return Err(failure.error(
                "there is no such vaern: module",
                Vec::new(),
                "The built-in modules are vaern:fs and vaern:http.",
            ));
        }
        match split_package(specifier) {
            Some((name, subpath)) => resolve_package(&failure, parent, name, subpath),
            None => Err(failure.error(
                "the specifier is neither relative nor a package name",
                Vec::new(),
                "Start a file import with ./ or ../, or import a package by its name.",
            )),
        }
    }

    fn load(&self, path: &Path) -> Result<String> {
        std::fs::read_to_string(path).map_err(|err| VaernError::Io {
            path: path.display().to_string(),
            source: err,
        })
    }
}

/// Carries the import being resolved so every error names it.
struct Failure<'a> {
    specifier: &'a str,
    referrer: &'a Path,
}

impl Failure<'_> {
    fn error(&self, reason: &str, tried: Vec<String>, action: &str) -> VaernError {
        VaernError::ModuleResolution {
            specifier: self.specifier.to_string(),
            referrer: self.referrer.display().to_string(),
            reason: reason.to_string(),
            tried,
            action: action.to_string(),
        }
    }
}

/// Splits `pkg/sub` or `@scope/pkg/sub` into the package name and the
/// subpath after it. Returns `None` for anything that is not a valid name.
fn split_package(specifier: &str) -> Option<(&str, Option<&str>)> {
    let name_len = if specifier.starts_with('@') {
        let scope_end = specifier.find('/')?;
        match specifier[scope_end + 1..].find('/') {
            Some(i) => scope_end + 1 + i,
            None => specifier.len(),
        }
    } else {
        specifier.find('/').unwrap_or(specifier.len())
    };
    let name = &specifier[..name_len];
    let valid = !name.is_empty()
        && !name.starts_with('.')
        && !name.contains(['\\', '%', ':'])
        && name.split('/').all(|part| !part.is_empty() && part != "@");
    if !valid {
        return None;
    }
    match specifier[name_len..].strip_prefix('/') {
        Some("") => None,
        Some(rest) => Some((name, Some(rest))),
        None => Some((name, None)),
    }
}

fn resolve_package(
    failure: &Failure<'_>,
    parent: &Path,
    name: &str,
    subpath: Option<&str>,
) -> Result<PathBuf> {
    let start = std::path::absolute(parent).map_err(|err| {
        failure.error(
            &format!("the importer's directory cannot be made absolute: {err}"),
            Vec::new(),
            "Check that the working directory still exists.",
        )
    })?;

    let mut tried = Vec::new();
    let mut package_dir = None;
    for dir in start.ancestors() {
        // Node.js never looks for node_modules/node_modules.
        if dir.file_name().is_some_and(|n| n == "node_modules") {
            continue;
        }
        let candidate = dir.join("node_modules").join(name);
        tried.push(candidate.display().to_string());
        if candidate.is_dir() {
            package_dir = Some(candidate);
            break;
        }
    }
    let Some(package_dir) = package_dir else {
        return Err(failure.error(
            &format!("package '{name}' is not installed in any node_modules directory"),
            tried,
            "Install the package next to the importing file or in a parent directory; Vaern does not download packages.",
        ));
    };

    let manifest_path = package_dir.join("package.json");
    let manifest = read_manifest(failure, &manifest_path)?;
    let target = match manifest.get("exports").filter(|e| !e.is_null()) {
        Some(exports) => {
            let key = match subpath {
                Some(rest) => format!("./{rest}"),
                None => ".".to_string(),
            };
            exports_target(exports, &key).map_err(|reason| {
                failure.error(
                    &format!("{reason} in {}", manifest_path.display()),
                    vec![format!("exports[\"{key}\"]")],
                    "Import a subpath the package exports, or ask its author to export this one.",
                )
            })?
        }
        None => match subpath {
            Some(rest) => format!("./{rest}"),
            None => entry_point(&manifest).ok_or_else(|| {
                failure.error(
                    &format!("package '{name}' has no ES module entry point"),
                    vec![
                        format!("{} \"exports\"", manifest_path.display()),
                        format!("{} \"module\"", manifest_path.display()),
                        format!(
                            "{} \"main\" (used only with \"type\": \"module\")",
                            manifest_path.display()
                        ),
                    ],
                    "Import a file in the package by its subpath, or use a package that ships ES modules.",
                )
            })?,
        },
    };

    if !stays_inside(&target) {
        return Err(failure.error(
            &format!("the package target '{target}' leaves the package directory"),
            vec![manifest_path.display().to_string()],
            "The package is malformed; its targets must stay inside its own directory.",
        ));
    }
    let file = package_dir.join(target.trim_start_matches("./"));
    if !file.is_file() {
        return Err(failure.error(
            "the file the package points to does not exist",
            vec![file.display().to_string()],
            "Reinstall the package; its files are incomplete.",
        ));
    }

    // Canonical names give a package one module identity however it is
    // reached, and follow links such as those a pnpm store creates.
    let canonical = std::fs::canonicalize(&file)
        .map(strip_verbatim)
        .map_err(|err| {
            failure.error(
                &format!("the file cannot be resolved to a real path: {err}"),
                vec![file.display().to_string()],
                "Check the permissions of the package directory.",
            )
        })?;
    Ok(canonical)
}

fn read_manifest(failure: &Failure<'_>, path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|err| {
            failure.error(
                &format!(
                    "{}:{}:{} is not valid JSON: {err}",
                    path.display(),
                    err.line(),
                    err.column()
                ),
                vec![path.display().to_string()],
                "Fix or reinstall the package's package.json.",
            )
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(Value::Object(serde_json::Map::new()))
        }
        Err(err) => Err(failure.error(
            &format!("{} cannot be read: {err}", path.display()),
            vec![path.display().to_string()],
            "Check the permissions of the package directory.",
        )),
    }
}

/// The root entry point of a package without `exports`, as a `./` path.
fn entry_point(manifest: &Value) -> Option<String> {
    let is_module = manifest.get("type").and_then(Value::as_str) == Some("module");
    let field = manifest.get("module").and_then(Value::as_str).or_else(|| {
        manifest
            .get("main")
            .and_then(Value::as_str)
            .filter(|_| is_module)
    })?;
    // `main` is commonly written without the leading `./`.
    Some(if field.starts_with("./") {
        field.to_string()
    } else {
        format!("./{field}")
    })
}

/// Looks `key` (`.` or `./sub`) up in an `exports` value and returns the
/// target path, or the reason it is not exported.
fn exports_target(exports: &Value, key: &str) -> std::result::Result<String, String> {
    let subpaths = match exports {
        Value::Object(map) if map.keys().any(|k| k.starts_with('.')) => {
            if !map.keys().all(|k| k.starts_with('.')) {
                return Err("\"exports\" mixes subpath keys and condition keys".to_string());
            }
            Some(map)
        }
        _ => None,
    };
    let Some(subpaths) = subpaths else {
        // A string or a conditions object is shorthand for { ".": value }.
        return if key == "." {
            resolve_target(exports, None)
        } else {
            Err(format!("subpath '{key}' is not exported"))
        };
    };

    if let Some(value) = subpaths.get(key).filter(|_| !key.contains('*')) {
        return resolve_target(value, None);
    }
    // Pattern keys with one `*`; the longest prefix wins, as in Node.js.
    let mut best: Option<(&str, &Value, &str)> = None;
    for (pattern, value) in subpaths {
        let Some((prefix, suffix)) = pattern.split_once('*') else {
            continue;
        };
        if suffix.contains('*') || key.len() < prefix.len() + suffix.len() {
            continue;
        }
        if let Some(middle) = key
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(suffix))
            && best.is_none_or(|(p, _, _)| prefix.len() > p.len())
        {
            best = Some((prefix, value, middle));
        }
    }
    match best {
        Some((_, value, middle)) => resolve_target(value, Some(middle)),
        None => Err(format!("subpath '{key}' is not exported")),
    }
}

fn resolve_target(value: &Value, star: Option<&str>) -> std::result::Result<String, String> {
    match value {
        Value::String(target) => {
            if !target.starts_with("./") {
                return Err(format!("export target '{target}' does not start with './'"));
            }
            Ok(match star {
                Some(middle) => target.replace('*', middle),
                None => target.clone(),
            })
        }
        Value::Null => Err("the subpath is explicitly not exported (null)".to_string()),
        Value::Object(conditions) => {
            for (condition, nested) in conditions {
                if CONDITIONS.contains(&condition.as_str()) {
                    return resolve_target(nested, star);
                }
            }
            let offered: Vec<&str> = conditions.keys().map(String::as_str).collect();
            Err(format!(
                "no export condition matches; the package offers [{}] and Vaern matches [{}]",
                offered.join(", "),
                CONDITIONS.join(", ")
            ))
        }
        Value::Array(_) => Err("export target arrays are not supported".to_string()),
        _ => Err("an export target must be a string, an object or null".to_string()),
    }
}

/// True when a `./`-relative target cannot climb out of the package, cross
/// into a nested `node_modules`, or name an absolute path.
fn stays_inside(target: &str) -> bool {
    let Some(rest) = target.strip_prefix("./") else {
        return false;
    };
    let path = Path::new(rest);
    !rest.is_empty()
        && !rest.contains('\\')
        && path.components().all(|c| match c {
            Component::Normal(part) => !part.eq_ignore_ascii_case("node_modules"),
            Component::CurDir => true,
            _ => false,
        })
}

/// Drops the `\\?\` prefix Windows adds to canonical paths, so module names
/// and stack traces show ordinary paths. UNC shares keep their `\\` form.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path,
    }
}
