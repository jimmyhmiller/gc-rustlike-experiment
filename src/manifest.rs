//! `gcr.toml` project manifest — a typed TOML reader for the handful
//! of keys gc-rust needs. A project is a directory containing `gcr.toml`; the
//! manifest names the package and points at its entry source file.
//!
//! ```toml
//! [package]
//! name = "myapp"
//! version = "0.1.0"
//! entry = "src/main.gcr"      # optional; defaults to "src/main.gcr"
//! ```
//!
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    /// Entry source file, relative to the manifest directory.
    pub entry: PathBuf,
    /// The directory the manifest lives in.
    pub dir: PathBuf,
    /// Native link configuration (the `[link]` section). Empty by default.
    pub link: LinkConfig,
}

/// The `[link]` section of `gcr.toml` — how to link native libraries into the
/// final executable, so `gcr build`/`gcr run` need no `--link-arg` on the CLI.
///
/// ```toml
/// [link]
/// libs = ["raylib"]                       # -l<name>
/// lib-paths = ["/opt/homebrew/lib"]        # -L<path>
/// frameworks = ["Cocoa", "OpenGL"]         # macOS: -framework <name>
/// args = ["-Wl,-rpath,/opt/homebrew/lib"]  # raw, passed through verbatim
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct LinkConfig {
    pub libs: Vec<String>,
    pub lib_paths: Vec<String>,
    pub frameworks: Vec<String>,
    pub args: Vec<String>,
}

impl LinkConfig {
    /// Flatten into the linker-argument list `build_executable` expects.
    pub fn to_args(&self) -> Vec<String> {
        let mut out = Vec::new();
        for p in &self.lib_paths {
            out.push(format!("-L{}", p));
        }
        for l in &self.libs {
            out.push(format!("-l{}", l));
        }
        for f in &self.frameworks {
            out.push("-framework".into());
            out.push(f.clone());
        }
        out.extend(self.args.iter().cloned());
        out
    }
}

#[derive(Debug)]
pub struct ManifestError(pub String);

impl Manifest {
    /// The absolute path to the entry source file.
    pub fn entry_path(&self) -> PathBuf {
        self.dir.join(&self.entry)
    }

    /// Native linker arguments from the `[link]` section.
    pub fn link_args(&self) -> Vec<String> {
        self.link.to_args()
    }

    /// Load `gcr.toml` from `dir`. Errors if the file is missing or malformed.
    pub fn load(dir: &Path) -> Result<Manifest, ManifestError> {
        let path = dir.join("gcr.toml");
        let text = std::fs::read_to_string(&path)
            .map_err(|e| ManifestError(format!("cannot read {}: {}", path.display(), e)))?;
        Self::parse(&text, dir)
    }

    /// Find a `gcr.toml` by walking up from `start` to the filesystem root.
    /// Returns `None` if no manifest is found (a bare-file build is still valid).
    pub fn discover(start: &Path) -> Result<Option<Manifest>, ManifestError> {
        // Relative paths must still search ancestors above the current directory.
        let start = if start.is_absolute() {
            start.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|e| ManifestError(format!("cannot determine project directory: {e}")))?
                .join(start)
        };
        let mut dir = if start.is_dir() {
            start.to_path_buf()
        } else if let Some(parent) = start.parent() {
            parent.to_path_buf()
        } else {
            return Ok(None);
        };
        loop {
            if dir.join("gcr.toml").exists() {
                return Manifest::load(&dir).map(Some);
            }
            if !dir.pop() {
                return Ok(None);
            }
        }
    }

    fn parse(text: &str, dir: &Path) -> Result<Manifest, ManifestError> {
        let parsed: ManifestFile =
            toml::from_str(text).map_err(|e| ManifestError(format!("gcr.toml: {e}")))?;
        let package = parsed.package;
        if package.name.is_empty()
            || Path::new(&package.name).components().count() != 1
            || !matches!(
                Path::new(&package.name).components().next(),
                Some(std::path::Component::Normal(_))
            )
        {
            return Err(ManifestError(
                "gcr.toml: package name must be a nonempty file name".into(),
            ));
        }
        if package.entry.as_os_str().is_empty() {
            return Err(ManifestError(
                "gcr.toml: entry must be a nonempty path".into(),
            ));
        }
        Ok(Manifest {
            name: package.name,
            version: package.version,
            entry: package.entry,
            dir: dir.to_path_buf(),
            link: parsed.link,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    package: Package,
    #[serde(default)]
    link: LinkConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Package {
    name: String,
    #[serde(default = "default_version")]
    version: String,
    #[serde(default = "default_entry")]
    entry: PathBuf,
}

fn default_version() -> String {
    "0.0.0".into()
}
fn default_entry() -> PathBuf {
    "src/main.gcr".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_manifest() {
        let src = "[package]\nname = \"myapp\"\nversion = \"1.2.3\"\nentry = \"src/app.gcr\"\n";
        let m = Manifest::parse(src, Path::new("/proj")).unwrap();
        assert_eq!(m.name, "myapp");
        assert_eq!(m.version, "1.2.3");
        assert_eq!(m.entry_path(), Path::new("/proj/src/app.gcr"));
    }

    #[test]
    fn entry_defaults() {
        let src = "[package]\nname = \"x\"\n";
        let m = Manifest::parse(src, Path::new("/p")).unwrap();
        assert_eq!(m.version, "0.0.0");
        assert_eq!(m.entry, PathBuf::from("src/main.gcr"));
    }

    #[test]
    fn comments_and_blank_lines_ignored() {
        let src = "# a project\n\n[package]   # the package table\nname = \"c\"  # its name\n";
        let m = Manifest::parse(src, Path::new("/p")).unwrap();
        assert_eq!(m.name, "c");
    }

    #[test]
    fn parses_link_section_into_linker_args() {
        let src = "[package]\nname = \"game\"\n\n[link]\n\
                   libs = [\"raylib\"]\n\
                   lib-paths = [\"/opt/homebrew/lib\"]\n\
                   frameworks = [\"Cocoa\", \"OpenGL\"]\n\
                   args = [\"-Wl,-rpath,/x\"]\n";
        let m = Manifest::parse(src, Path::new("/p")).unwrap();
        assert_eq!(m.link.libs, ["raylib"]);
        assert_eq!(
            m.link_args(),
            [
                "-L/opt/homebrew/lib",
                "-lraylib",
                "-framework",
                "Cocoa",
                "-framework",
                "OpenGL",
                "-Wl,-rpath,/x",
            ]
        );
    }

    #[test]
    fn no_link_section_means_no_args() {
        let m = Manifest::parse("[package]\nname = \"x\"\n", Path::new("/p")).unwrap();
        assert!(m.link_args().is_empty());
    }

    #[test]
    fn missing_name_errors() {
        let src = "[package]\nversion = \"1.0.0\"\n";
        assert!(Manifest::parse(src, Path::new("/p")).is_err());
    }

    #[test]
    fn unknown_key_errors() {
        let src = "[package]\nname = \"x\"\nbogus = \"y\"\n";
        assert!(Manifest::parse(src, Path::new("/p")).is_err());
    }
    #[test]
    fn rejects_malformed_types_duplicates_and_syntax() {
        for text in [
            "[package]\nname = bare",
            "[package]\nname = 42",
            "[package]\nname = \"x\"\nname = \"y\"",
            "[package]\nname = \"x\"\n[link]\nlibs = \"m\"",
            "[package]\nname = \"x\"\n[link]\nlibs = [1]",
            "[package]\nname = \"x\"\n[link]\nlibs = [\"m\"",
            "[package]\nname = \"\"",
            "[package]\nname = \"../x\"",
        ] {
            assert!(
                Manifest::parse(text, Path::new("/p")).is_err(),
                "accepted {text}"
            );
        }
    }

    #[test]
    fn parses_multiline_arrays_escapes_and_hashes_in_strings() {
        let m = Manifest::parse(
            r#"
            [package]
            name = "hash#app"
            [link]
            args = [
                "-Wl,-rpath,/a#b", # a comment
                "quoted\"argument",
            ]
        "#,
            Path::new("/p"),
        )
        .unwrap();
        assert_eq!(m.name, "hash#app");
        assert_eq!(m.link.args, ["-Wl,-rpath,/a#b", "quoted\"argument"]);
    }

    #[test]
    fn discovery_reports_malformed_parent_manifest() {
        let dir = std::env::temp_dir().join(format!("gcr-manifest-invalid-{}", std::process::id()));
        let nested = dir.join("src");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.join("gcr.toml"), "[package]\nname = 123").unwrap();
        let result = Manifest::discover(&nested);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(result.is_err());
    }
}
