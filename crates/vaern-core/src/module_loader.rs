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
//!
//! Every module source, the entry file included, is read through
//! [`read_source`], which bounds the read before the text reaches the heap.

use crate::error::{Result, VaernError};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

/// Conditions matched in an `exports` map. Order is irrelevant here: a
/// conditions object is read in its own key order and the first key that is
/// in this list wins, as Node.js specifies.
const CONDITIONS: [&str; 3] = ["vaern", "import", "default"];

/// Largest module source the loader reads. Source text is copied into the
/// QuickJS heap and parsed there, so it gets the same quarter-of-the-heap
/// bound as a `vaern:fs` read; without one, a huge or endless file would be
/// read into host memory before any engine limit applies.
pub const MAX_SOURCE_BYTES: u64 = crate::fs::MAX_READ_BYTES;

/// Reads a module's UTF-8 source, refusing anything over [`MAX_SOURCE_BYTES`].
/// A leading byte order mark is dropped: the engine counts it as part of the
/// first line, which shifts every location reported on it.
pub fn read_source(path: &Path) -> Result<String> {
    use std::io::Read;
    let io_error = |source| VaernError::Io {
        path: path.display().to_string(),
        source,
    };
    let too_large = || VaernError::SourceTooLarge {
        path: path.display().to_string(),
        limit_mib: MAX_SOURCE_BYTES / (1024 * 1024),
    };
    let file = std::fs::File::open(path).map_err(io_error)?;
    if file.metadata().map_err(io_error)?.len() > MAX_SOURCE_BYTES {
        return Err(too_large());
    }
    // The reported length can be stale or zero (a pipe or device), so the
    // read itself stops one byte past the limit.
    let mut bytes = Vec::new();
    file.take(MAX_SOURCE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > MAX_SOURCE_BYTES {
        return Err(too_large());
    }
    let text = String::from_utf8(bytes).map_err(|err| {
        io_error(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "the source is not valid UTF-8 at byte {}",
                err.utf8_error().valid_up_to()
            ),
        ))
    })?;
    Ok(match text.strip_prefix('\u{FEFF}') {
        Some(rest) => rest.to_string(),
        None => text,
    })
}

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
        read_source(path)
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
    // No part, scope included, may start with a dot, as in npm, so `@a/..`
    // cannot name the node_modules directory itself.
    let valid = !name.is_empty()
        && !name.contains(['\\', '%', ':'])
        && name.split('/').all(|part| {
            let bare = part.strip_prefix('@').unwrap_or(part);
            !bare.is_empty() && !bare.starts_with('.')
        });
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
            exported_target(exports, None)
        } else {
            Err(format!("subpath '{key}' is not exported"))
        };
    };

    if let Some(value) = subpaths.get(key).filter(|_| !key.contains('*')) {
        return exported_target(value, None);
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
        Some((_, value, middle)) => exported_target(value, Some(middle)),
        None => Err(format!("subpath '{key}' is not exported")),
    }
}

/// Result of reading one export target. `NoMatch` is a conditions object
/// none of whose keys apply; Node.js treats it as undefined and moves on to
/// the next sibling condition, whereas `null` and malformed targets end the
/// lookup as errors.
enum Target {
    Found(String),
    NoMatch,
}

/// Resolves the value an `exports` key maps to, failing when no condition
/// at any level applies.
fn exported_target(value: &Value, star: Option<&str>) -> std::result::Result<String, String> {
    match resolve_target(value, star)? {
        Target::Found(target) => Ok(target),
        Target::NoMatch => {
            let offered: Vec<&str> = match value {
                Value::Object(conditions) => conditions.keys().map(String::as_str).collect(),
                _ => Vec::new(),
            };
            Err(format!(
                "no export condition matches; the package offers [{}] and Vaern matches [{}]",
                offered.join(", "),
                CONDITIONS.join(", ")
            ))
        }
    }
}

fn resolve_target(value: &Value, star: Option<&str>) -> std::result::Result<Target, String> {
    match value {
        Value::String(target) => {
            if !target.starts_with("./") {
                return Err(format!("export target '{target}' does not start with './'"));
            }
            Ok(Target::Found(match star {
                Some(middle) => target.replace('*', middle),
                None => target.clone(),
            }))
        }
        Value::Null => Err("the subpath is explicitly not exported (null)".to_string()),
        Value::Object(conditions) => {
            for (condition, nested) in conditions {
                if !CONDITIONS.contains(&condition.as_str()) {
                    continue;
                }
                if let Target::Found(target) = resolve_target(nested, star)? {
                    return Ok(Target::Found(target));
                }
            }
            Ok(Target::NoMatch)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_over_the_limit_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let big = root.path().join("big.js");
        // Sparse, so the test costs no disk space or read time.
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_SOURCE_BYTES + 1)
            .unwrap();
        match read_source(&big) {
            Err(VaernError::SourceTooLarge { path, limit_mib }) => {
                assert!(path.ends_with("big.js"), "{path}");
                assert_eq!(limit_mib, 64);
            }
            Err(other) => panic!("expected SourceTooLarge, got {other}"),
            Ok(text) => panic!("expected SourceTooLarge, read {} bytes", text.len()),
        }
    }

    #[test]
    fn test_source_must_be_utf8_and_names_the_bad_byte() {
        let root = tempfile::tempdir().unwrap();
        let good = root.path().join("good.js");
        std::fs::write(&good, "export const x = '\u{6587}';").unwrap();
        assert_eq!(read_source(&good).unwrap(), "export const x = '\u{6587}';");
        let bad = root.path().join("bad.js");
        std::fs::write(&bad, b"abc\xff").unwrap();
        let text = read_source(&bad).unwrap_err().to_string();
        assert!(text.contains("not valid UTF-8 at byte 3"), "{text}");
    }

    /// Writes `files` (relative path, contents) under a new temporary root.
    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for (rel, contents) in files {
            let path = root.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        root
    }

    fn resolve_from(root: &tempfile::TempDir, importer: &str, specifier: &str) -> Result<PathBuf> {
        FsModuleLoader.resolve(specifier, &root.path().join(importer))
    }

    fn real(root: &tempfile::TempDir, rel: &str) -> PathBuf {
        strip_verbatim(std::fs::canonicalize(root.path().join(rel)).unwrap())
    }

    fn reason_and_tried(err: VaernError) -> (String, Vec<String>) {
        match err {
            VaernError::ModuleResolution { reason, tried, .. } => (reason, tried),
            other => panic!("expected a resolution error, got {other}"),
        }
    }

    #[test]
    fn test_nearest_node_modules_wins_while_walking_up() {
        let root = tree(&[
            ("app/src/main.js", ""),
            ("node_modules/pkg/package.json", r#"{"module": "outer.js"}"#),
            ("node_modules/pkg/outer.js", ""),
            (
                "app/node_modules/pkg/package.json",
                r#"{"module": "./inner.js"}"#,
            ),
            ("app/node_modules/pkg/inner.js", ""),
            (
                "node_modules/other/package.json",
                r#"{"type": "module", "main": "lib/index.js"}"#,
            ),
            ("node_modules/other/lib/index.js", ""),
        ]);
        assert_eq!(
            resolve_from(&root, "app/src/main.js", "pkg").unwrap(),
            real(&root, "app/node_modules/pkg/inner.js")
        );
        assert_eq!(
            resolve_from(&root, "app/src/main.js", "other").unwrap(),
            real(&root, "node_modules/other/lib/index.js")
        );
    }

    #[test]
    fn test_exports_conditions_follow_declaration_order() {
        let root = tree(&[
            ("main.js", ""),
            (
                "node_modules/cond/package.json",
                r#"{"exports": {"node": "./node.cjs", "require": "./req.cjs",
                    "import": {"types": "./x.d.ts", "default": "./esm.js"},
                    "default": "./fallback.js"}}"#,
            ),
            ("node_modules/cond/esm.js", ""),
            (
                "node_modules/vaern-first/package.json",
                r#"{"exports": {"default": "./d.js", "vaern": "./v.js"}}"#,
            ),
            ("node_modules/vaern-first/d.js", ""),
            (
                "node_modules/str/package.json",
                r#"{"exports": "./entry.ts"}"#,
            ),
            ("node_modules/str/entry.ts", ""),
        ]);
        assert_eq!(
            resolve_from(&root, "main.js", "cond").unwrap(),
            real(&root, "node_modules/cond/esm.js")
        );
        // "default" comes first in that package, so it wins over "vaern".
        assert_eq!(
            resolve_from(&root, "main.js", "vaern-first").unwrap(),
            real(&root, "node_modules/vaern-first/d.js")
        );
        assert_eq!(
            resolve_from(&root, "main.js", "str").unwrap(),
            real(&root, "node_modules/str/entry.ts")
        );
    }

    #[test]
    fn test_exports_subpaths_patterns_and_refusals() {
        let root = tree(&[
            ("main.js", ""),
            (
                "node_modules/@scope/kit/package.json",
                r#"{"exports": {".": "./index.js", "./util": {"import": "./src/util.js"},
                    "./features/*.js": "./src/features/*.js", "./features/secret.js": null,
                    "./escape": "./../outside.js", "./cjs": {"require": "./c.cjs"}}}"#,
            ),
            ("node_modules/@scope/kit/index.js", ""),
            ("node_modules/@scope/kit/src/util.js", ""),
            ("node_modules/@scope/kit/src/features/a/b.js", ""),
            ("node_modules/@scope/kit/src/hidden.js", ""),
        ]);
        assert_eq!(
            resolve_from(&root, "main.js", "@scope/kit").unwrap(),
            real(&root, "node_modules/@scope/kit/index.js")
        );
        assert_eq!(
            resolve_from(&root, "main.js", "@scope/kit/util").unwrap(),
            real(&root, "node_modules/@scope/kit/src/util.js")
        );
        assert_eq!(
            resolve_from(&root, "main.js", "@scope/kit/features/a/b.js").unwrap(),
            real(&root, "node_modules/@scope/kit/src/features/a/b.js")
        );
        for (specifier, expected) in [
            ("@scope/kit/features/secret.js", "explicitly not exported"),
            ("@scope/kit/src/hidden.js", "is not exported"),
            ("@scope/kit/escape", "leaves the package directory"),
            ("@scope/kit/cjs", "no export condition matches"),
        ] {
            let err = resolve_from(&root, "main.js", specifier).unwrap_err();
            let (reason, _) = reason_and_tried(err);
            assert!(reason.contains(expected), "{specifier}: {reason}");
        }
    }

    #[test]
    fn test_packages_without_exports_need_an_es_module_entry() {
        let root = tree(&[
            ("main.js", ""),
            (
                "node_modules/legacy/package.json",
                r#"{"main": "index.js"}"#,
            ),
            ("node_modules/legacy/index.js", ""),
            ("node_modules/legacy/esm/part.js", ""),
        ]);
        let (reason, tried) =
            reason_and_tried(resolve_from(&root, "main.js", "legacy").unwrap_err());
        assert!(reason.contains("no ES module entry point"), "{reason}");
        assert_eq!(tried.len(), 3);
        assert_eq!(
            resolve_from(&root, "main.js", "legacy/esm/part.js").unwrap(),
            real(&root, "node_modules/legacy/esm/part.js")
        );
        for (specifier, expected) in [
            ("legacy/esm/part", "does not exist"),
            ("legacy/../x.js", "leaves the package directory"),
        ] {
            let err = resolve_from(&root, "main.js", specifier).unwrap_err();
            let (reason, _) = reason_and_tried(err);
            assert!(reason.contains(expected), "{specifier}: {reason}");
        }
    }

    #[test]
    fn test_missing_package_lists_every_directory_tried() {
        let root = tree(&[("a/b/main.js", "")]);
        let err = resolve_from(&root, "a/b/main.js", "absent").unwrap_err();
        let text = err.to_string();
        let (reason, tried) = reason_and_tried(err);
        assert!(reason.contains("'absent' is not installed"), "{reason}");
        let first = root.path().join("a/b/node_modules/absent");
        assert_eq!(
            tried[0],
            std::path::absolute(first).unwrap().display().to_string()
        );
        assert_eq!(tried.len(), root.path().join("a/b").ancestors().count());
        assert!(text.contains("tried: "), "{text}");
    }

    #[test]
    fn test_node_builtins_and_invalid_specifiers_are_refused() {
        let root = tree(&[("main.js", "")]);
        for (specifier, expected) in [
            ("node:fs", "built-in modules are not provided"),
            ("vaern:nope", "no such vaern: module"),
            ("/abs.js", "neither relative nor a package name"),
            (".hidden", "neither relative nor a package name"),
            ("@scope", "neither relative nor a package name"),
            ("pkg/", "neither relative nor a package name"),
            ("C:\\x.js", "neither relative nor a package name"),
            ("https://x.test/a.js", "neither relative nor a package name"),
        ] {
            let err = resolve_from(&root, "main.js", specifier).unwrap_err();
            let (reason, _) = reason_and_tried(err);
            assert!(reason.contains(expected), "{specifier}: {reason}");
        }
    }

    #[test]
    fn test_unmatched_nested_conditions_fall_through_to_siblings() {
        let root = tree(&[
            ("main.js", ""),
            (
                "node_modules/typed/package.json",
                r#"{"exports": {"import": {"types": "./index.d.ts"}, "default": "./index.js"}}"#,
            ),
            ("node_modules/typed/index.js", ""),
            (
                "node_modules/deep/package.json",
                r#"{"exports": {"./sub": {"import": {"node": {"types": "./a.d.ts"}},
                    "default": {"import": "./sub.js"}}}}"#,
            ),
            ("node_modules/deep/sub.js", ""),
            (
                "node_modules/nulled/package.json",
                r#"{"exports": {"import": null, "default": "./index.js"}}"#,
            ),
            ("node_modules/nulled/index.js", ""),
            (
                "node_modules/typesonly/package.json",
                r#"{"exports": {"import": {"types": "./index.d.ts"}}}"#,
            ),
        ]);
        assert_eq!(
            resolve_from(&root, "main.js", "typed").unwrap(),
            real(&root, "node_modules/typed/index.js")
        );
        assert_eq!(
            resolve_from(&root, "main.js", "deep/sub").unwrap(),
            real(&root, "node_modules/deep/sub.js")
        );
        // null ends the lookup instead of falling through, as in Node.js.
        for (specifier, expected) in [
            ("nulled", "explicitly not exported"),
            (
                "typesonly",
                "no export condition matches; the package offers [import]",
            ),
        ] {
            let err = resolve_from(&root, "main.js", specifier).unwrap_err();
            let (reason, _) = reason_and_tried(err);
            assert!(reason.contains(expected), "{specifier}: {reason}");
        }
    }

    #[test]
    fn test_dot_parts_are_not_package_names() {
        let root = tree(&[("main.js", ""), ("node_modules/@a/x/index.js", "")]);
        for specifier in ["@a/..", "@a/.", "@a/../x", "@a/.x", "@./x", "..pkg"] {
            let err = resolve_from(&root, "main.js", specifier).unwrap_err();
            let (reason, _) = reason_and_tried(err);
            assert!(
                reason.contains("neither relative nor a package name"),
                "{specifier}: {reason}"
            );
        }
    }

    #[test]
    fn test_strip_verbatim_keeps_ordinary_paths() {
        for (input, expected) in [
            (r"\\?\C:\x\y.js", r"C:\x\y.js"),
            (r"\\?\UNC\srv\share\a.js", r"\\srv\share\a.js"),
            ("/usr/lib/a.js", "/usr/lib/a.js"),
        ] {
            assert_eq!(
                strip_verbatim(PathBuf::from(input)),
                PathBuf::from(expected)
            );
        }
    }
}
