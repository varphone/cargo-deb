use std::env::consts::ARCH;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::collections::HashSet;
use toml;
use file;
use glob;
use dependencies::resolve;
use serde_json;
use error::*;
use try::Try;
use config::CargoConfig;

fn is_glob_pattern(s: &str) -> bool {
    s.contains('*') || s.contains('[') || s.contains(']') || s.contains('!')
}

#[derive(Debug)]
pub struct Asset {
    pub source_file: PathBuf,
    pub target_path: PathBuf,
    pub chmod: u32,
}

impl Asset {
    pub fn new(source_file: PathBuf, mut target_path: PathBuf, chmod: u32) -> Self {
        if target_path.is_absolute() {
            target_path = target_path.strip_prefix("/").expect("no root dir").to_owned();
        }
        // is_dir() is only for paths that exist
        if target_path.to_string_lossy().ends_with("/") {
            target_path = target_path.join(source_file.file_name().expect("source must be a file"));
    }
        Self {
            source_file,
            target_path,
            chmod,
        }
    }
    pub fn is_binary_executable(&self, workspace_root: &Path, release_dir_prefix: &Path) -> bool {
        let source_file_abspath = workspace_root.join(&self.source_file);
        source_file_abspath.starts_with(release_dir_prefix) && 0 != (self.chmod & 0o111)
    }
}

#[derive(Debug)]
pub struct Config {
    pub workspace_root: PathBuf,
    pub target: Option<String>,
    pub target_dir: PathBuf,
    /// The name of the project to build
    pub name: String,
    /// The software license of the project.
    pub license: Option<String>,
    /// The location of the license file followed by the amount of lines to skip.
    pub license_file: Option<PathBuf>,
    pub license_file_skip_lines: usize,
    /// The copyright of the project.
    pub copyright: String,
    /// The version number of the project.
    pub version: String,
    /// The homepage of the project.
    pub homepage: Option<String>,
    pub documentation: Option<String>,
    /// The URL of the software repository.
    pub repository: Option<String>,
    /// A short description of the project.
    pub description: String,
    /// An extended description of the project.
    pub extended_description: Option<String>,
    /// The maintainer of the Debian package.
    pub maintainer: String,
    /// The Debian dependencies required to run the project.
    pub depends: String,
    /// The category by which the package belongs.
    pub section: Option<String>,
    /// The priority of the project. Typically 'optional'.
    pub priority: String,

    /// https://wiki.debian.org/PackageTransition
    pub conflicts: Option<String>,
    /// https://wiki.debian.org/PackageTransition
    pub breaks: Option<String>,
    /// https://wiki.debian.org/PackageTransition
    pub replaces: Option<String>,
    /// https://wiki.debian.org/PackageTransition
    pub provides: Option<String>,

    /// The architecture of the running system.
    pub architecture: String,
    /// A list of configuration files installed by the package.
    pub conf_files: Option<String>,
    /// All of the files that are to be packaged.
    pub assets: Vec<Asset>,
    /// The path were possible maintainer scripts live
    pub maintainer_scripts: Option<PathBuf>,
    /// List of Cargo features to use during build
    pub features: Vec<String>,
    pub default_features: bool,
    /// Should the binary be stripped from debug symbols?
    pub strip: bool,
}

impl Config {
    pub fn from_manifest(target: Option<&str>) -> CDResult<(Config, Vec<String>)> {
        let metadata = cargo_metadata()?;
        let root_id = metadata.resolve.root;
        let root_package = metadata.packages.iter()
            .filter(|p|p.id == root_id).next()
            .ok_or("Unable to find root package in cargo metadata")?;
        let target_dir = Path::new(&metadata.target_directory);
        let manifest_path = Path::new(&root_package.manifest_path);
        let workspace_root = if let Some(ref workspace_root) = metadata.workspace_root {
            Path::new(workspace_root)
        } else {
            manifest_path.parent().expect("no workspace_root")
        };
        let content = file::get_text(&manifest_path)
            .map_err(|e| CargoDebError::IoFile("unable to read Cargo.toml", e, manifest_path.to_owned()))?;
        toml::from_str::<Cargo>(&content)?.to_config(root_package, &workspace_root, &target_dir, target)
    }

    pub fn get_dependencies(&self) -> CDResult<String> {
        let mut deps = HashSet::new();
        for word in self.depends.split(',') {
            let word = word.trim();
            if word == "$auto" {
                for bname in self.binaries().iter() {
                    match resolve(bname, &self.architecture) {
                        Ok(bindeps) => for dep in bindeps {
                            deps.insert(dep);
                        },
                        Err(err) => eprintln!("warning: {} (no auto deps for {})", err, bname.display()),
                    };
                }
            } else {
                deps.insert(word.to_owned());
            }
        }
        Ok(deps.into_iter().collect::<Vec<_>>().join(", "))
    }

    pub fn add_copyright_asset(&mut self) {
        // The file is autogenerated later
        let path = self.path_in_deb("copyright");
        self.assets.push(Asset::new(
            path,
            PathBuf::from("usr/share/doc").join(&self.name).join("copyright"),
            0o644,
        ));
    }

    fn add_changelog_asset(&mut self, changelog: Option<String>) {
        if let Some(log_file) = changelog {
            self.assets.push(Asset::new(
                PathBuf::from(log_file),
                PathBuf::from("usr/share/doc").join(&self.name).join("changelog"),
                0o644,
            ));
        }
    }

    pub fn binaries(&self) -> Vec<&Path> {
        let target_dir = if self.target.is_some() {
            // Strip target triple
            self.target_dir.parent().expect("no target dir")
        } else {
            &self.target_dir
        };
        let release_dir_prefix = target_dir.join("release");
        self.assets.iter().filter_map(|asset| {
            // Assumes files in build dir which have executable flag set are binaries
            if asset.is_binary_executable(&self.workspace_root, &release_dir_prefix) {
                Some(asset.source_file.as_path())
            } else {
                None
            }
        }).collect()
    }

    /// Tries to guess type of source control used for the repo URL.
    /// It's a guess, and it won't be 100% accurate, because Cargo suggests using
    /// user-friendly URLs or webpages instead of tool-specific URL schemes.
    pub fn repository_type(&self) -> Option<&str> {
        if let Some(ref repo) = self.repository {
            if repo.starts_with("git+") || repo.ends_with(".git") || repo.contains("git@") || repo.contains("github.com") || repo.contains("gitlab.com") {
                return Some("Git");
            }
            if repo.starts_with("cvs+") || repo.contains("pserver:") || repo.contains("@cvs.") {
                return Some("Cvs");
            }
            if repo.starts_with("hg+") || repo.contains("hg@") || repo.contains("/hg.") {
                return Some("Hg");
            }
            if repo.starts_with("svn+") || repo.contains("/svn.") {
                return Some("Svn");
            }
            return None;
        }
        None
    }

    pub fn path_in_build<P: AsRef<Path>>(&self, rel_path: P) -> PathBuf {
        self.target_dir.join("release").join(rel_path)
    }

    pub fn deb_dir(&self) -> PathBuf {
        self.target_dir.join("debian")
    }

    pub fn path_in_deb<P: AsRef<Path>>(&self, rel_path: P) -> PathBuf {
        self.deb_dir().join(rel_path)
    }

    pub fn cargo_config(&self) -> CDResult<Option<CargoConfig>> {
        CargoConfig::new(&self.target_dir)
    }
}

#[derive(Clone, Debug, Deserialize)]
struct Cargo {
    pub package: CargoPackage,
    pub profile: Option<CargoProfiles>,
}

impl Cargo {
    fn to_config(mut self, root_package: &CargoMetadataPackage, workspace_root: &Path, target_dir: &Path, target: Option<&str>)
        -> CDResult<(Config, Vec<String>)>
    {
        // Cargo cross-compiles to a dir
        let target_dir = if let Some(target) = target {
            target_dir.join(target)
        } else {
            target_dir.to_owned()
        };

        let mut deb = self.package.metadata.take().and_then(|m|m.deb)
            .unwrap_or_else(|| CargoDeb::default());
        let (license_file, license_file_skip_lines) = self.license_file(deb.license_file.as_ref())?;
        let readme = self.package.readme.as_ref();
        let warnings = self.check_config(readme, &deb);
        let mut config = Config {
            workspace_root: workspace_root.to_owned(),
            target: target.map(|t| t.to_string()),
            target_dir,
            name: self.package.name.clone(),
            license: self.package.license.take(),
            license_file,
            license_file_skip_lines,
            copyright: deb.copyright.take().ok_or_then(|| {
                Ok(self.package.authors.as_ref().ok_or("Package must have a copyright or authors")?.join(", "))
            })?,
            version: self.version_string(deb.revision),
            homepage: self.package.homepage.clone(),
            documentation: self.package.documentation.clone(),
            repository: self.package.repository.take(),
            description: self.package.description.take().unwrap_or_else(||format!("{} -- autogenerated Rust project", self.package.name)),
            extended_description: self.extended_description(deb.extended_description.take(), readme)?,
            maintainer: deb.maintainer.take().ok_or_then(|| {
                Ok(self.package.authors.as_ref().and_then(|a|a.get(0))
                    .ok_or("Package must have a maintainer or authors")?.to_owned())
            })?,
            depends: deb.depends.take().unwrap_or("$auto".to_owned()),
            conflicts: deb.conflicts.take(),
            breaks: deb.breaks.take(),
            replaces: deb.replaces.take(),
            provides: deb.provides.take(),
            section: deb.section.take(),
            priority: deb.priority.take().unwrap_or("optional".to_owned()),
            architecture: get_arch(target.unwrap_or(ARCH)).to_owned(),
            conf_files: deb.conf_files.map(|x| x.iter().fold(String::new(), |a, b| a + b + "\n")),
            assets: vec![],
            maintainer_scripts: deb.maintainer_scripts.map(|s| PathBuf::from(s)),
            features: deb.features.take().unwrap_or(vec![]),
            default_features: deb.default_features.unwrap_or(true),
            strip: self.profile.as_ref().and_then(|p|p.release.as_ref())
                .and_then(|r|r.debug).map(|debug|!debug).unwrap_or(true),
        };

        let assets = self.take_assets(&config, deb.assets.take(), &root_package.targets, readme)?;
        if assets.is_empty() {
            Err("No binaries found. The package is empty. Please specify some assets to package in Cargo.toml")?;
        }
        config.assets.extend(assets);
        config.add_copyright_asset();
        config.add_changelog_asset(deb.changelog.take());

        Ok((config, warnings))
    }

    fn check_config(&self, readme: Option<&String>, deb: &CargoDeb) -> Vec<String> {
        let mut warnings = vec![];
        if self.package.description.is_none() {
            warnings.push("description field is missing in Cargo.toml".to_owned());
        }
        if self.package.license.is_none() {
            warnings.push("license field is missing in Cargo.toml".to_owned());
        }
        if let Some(readme) = readme {
            if deb.extended_description.is_none() && (readme.ends_with(".md") || readme.ends_with(".markdown")) {
                warnings.push(format!("extended-description field missing. Using {}, but markdown may not render well.",readme));
            }
        } else {
            for p in &["README.md", "README.txt", "README"] {
                if Path::new(p).exists() {
                    warnings.push(format!("{} file exists, but is not specified in `readme` Cargo.toml field", p));
                    break;
                }
            }
        }
        warnings
    }

    fn extended_description(&self, desc: Option<String>, readme: Option<&String>) -> CDResult<Option<String>> {
        Ok(if desc.is_some() {
            desc
        } else if let Some(readme) = readme {
            Some(file::get_text(readme)
                .map_err(|err| CargoDebError::IoFile("unable to read README", err, PathBuf::from(readme)))?)
        } else {
            None
        })
    }

    fn license_file(&mut self, license_file: Option<&Vec<String>>) -> CDResult<(Option<PathBuf>, usize)> {
        if let Some(args) = license_file {
            let mut args = args.iter();
            let file = args.next();
            let lines = if let Some(lines) = args.next() {
                lines.parse().map_err(|e| CargoDebError::NumParse("invalid number of lines", e))?
            } else {0};
            Ok((file.map(|s|s.into()), lines))
        } else {
            Ok((self.package.license_file.as_ref().map(|s|s.into()), 0))
        }
    }

    fn take_assets(&self, options: &Config, assets: Option<Vec<Vec<String>>>, targets: &[CargoMetadataTarget], readme: Option<&String>) -> CDResult<Vec<Asset>> {
        Ok(if let Some(assets) = assets {
            let mut all_assets = Vec::with_capacity(assets.len());
            for mut v in assets.into_iter() {
                let mut v = v.drain(..);
                let mut source_path = PathBuf::from(v.next().ok_or("missing path for asset")?);
                if source_path.starts_with("target/release") {
                    source_path = options.path_in_build(source_path.strip_prefix("target/release").unwrap());
                }
                let source_path_str = source_path.to_str().unwrap();
                let target_path = PathBuf::from(v.next().ok_or("missing target for asset")?);
                let mode = u32::from_str_radix(&v.next().ok_or("missing chmod for asset")?, 8)
                    .map_err(|e| CargoDebError::NumParse("unable to parse chmod argument", e))?;
                let source_prefix: PathBuf = source_path.iter()
                    .take_while(|part| !is_glob_pattern(part.to_str().unwrap()))
                    .collect();
                for entry in glob::glob(source_path_str)? {
                    let source_file = entry?;
                    if source_file.is_dir() {
                        continue;
                    }
                    // XXX: how do we handle duplicated assets?
                    let target_file = if is_glob_pattern(source_path_str) {
                        target_path.join(source_file.strip_prefix(&source_prefix).unwrap())
                    } else {
                        target_path.clone()
                    };
                    all_assets.push(Asset::new(
                        source_file,
                        target_file,
                        mode
                    ));
                }
            }
            all_assets
        } else {
            let mut implied_assets: Vec<_> = targets
                .iter()
                .filter(|t| t.crate_types.iter().any(|ty|ty=="bin") && t.kind.iter().any(|k|k=="bin"))
                .map(|bin| {
                Asset::new(
                    options.path_in_build(&bin.name),
                    PathBuf::from("usr/bin").join(&bin.name),
                    0o755,
                )
            }).collect();
            if let Some(readme) = readme {
                let target_path = PathBuf::from("usr/share/doc").join(&self.package.name).join(readme);
                implied_assets.push(Asset::new(
                    PathBuf::from(readme),
                    target_path,
                    0o644,
                ));
            }
            implied_assets
        })
    }

    fn version_string(&self, revision: Option<String>) -> String {
        if let Some(revision) = revision {
            format!("{}-{}", self.package.version, revision)
        } else {
            self.package.version.clone()
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct CargoPackage {
    pub name: String,
    pub authors: Option<Vec<String>>,
    pub license: Option<String>,
    pub license_file: Option<String>,
    pub homepage: Option<String>,
    pub documentation: Option<String>,
    pub repository: Option<String>,
    pub version: String,
    pub description: Option<String>,
    pub readme: Option<String>,
    pub metadata: Option<CargoPackageMetadata>,
}

#[derive(Clone, Debug, Deserialize)]
struct CargoPackageMetadata {
    pub deb: Option<CargoDeb>
}

#[derive(Clone, Debug, Deserialize)]
struct CargoProfiles {
    pub release: Option<CargoProfile>
}

#[derive(Clone, Debug, Deserialize)]
struct CargoProfile {
    pub debug: Option<bool>
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct CargoBin {
    pub name: String,
    pub plugin: Option<bool>,
    pub proc_macro: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct CargoDeb {
    pub maintainer: Option<String>,
    pub copyright: Option<String>,
    pub license_file: Option<Vec<String>>,
    pub changelog: Option<String>,
    pub depends: Option<String>,
    pub conflicts: Option<String>,
    pub breaks: Option<String>,
    pub replaces: Option<String>,
    pub provides: Option<String>,
    pub extended_description: Option<String>,
    pub section: Option<String>,
    pub priority: Option<String>,
    pub revision: Option<String>,
    pub conf_files: Option<Vec<String>>,
    pub assets: Option<Vec<Vec<String>>>,
    pub maintainer_scripts: Option<String>,
    pub features: Option<Vec<String>>,
    pub default_features: Option<bool>,
}

#[derive(Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoMetadataPackage>,
    resolve: CargoMetadataResolve,
    target_directory: String,
    workspace_root: Option<String>,
}

#[derive(Deserialize)]
struct CargoMetadataResolve {
    root: String,
}

#[derive(Deserialize)]
struct CargoMetadataPackage {
    pub id: String,
    pub targets: Vec<CargoMetadataTarget>,
    pub manifest_path: String,
}

#[derive(Deserialize)]
struct CargoMetadataTarget {
    pub name: String,
    pub kind: Vec<String>,
    pub crate_types: Vec<String>,
}

/// Returns the path of the `Cargo.toml` that we want to build.
fn cargo_metadata() -> CDResult<CargoMetadata> {
    let output = Command::new("cargo").arg("metadata").arg("--format-version=1")
        .output().map_err(|e| CargoDebError::CommandFailed(e, "cargo (is it in your PATH?)"))?;
    if !output.status.success() {
        return Err(CargoDebError::CommandError("cargo", "metadata".to_owned(), output.stderr));
    }

    let stdout = String::from_utf8(output.stdout).unwrap();
    let metadata = serde_json::from_str(&stdout)?;
    Ok(metadata)
}

/// Debianizes the architecture name
fn get_arch(target: &str) -> &str {
    let mut parts = target.split('-');
    let arch = parts.next().unwrap();
    let abi = parts.last().unwrap_or("");
    match (arch, abi) {
        // https://wiki.debian.org/Multiarch/Tuples
        // rustc --print target-list
        // https://doc.rust-lang.org/std/env/consts/constant.ARCH.html
        ("aarch64", _)          => "arm64",
        ("mips64", "gnuabin32") => "mipsn32",
        ("mips64el", "gnuabin32") => "mipsn32el",
        ("mipsisa32r6", _) => "mipsr6",
        ("mipsisa32r6el", _) => "mipsr6el",
        ("mipsisa64r6", "gnuabi64") => "mips64r6",
        ("mipsisa64r6", "gnuabin32") => "mipsn32r6",
        ("mipsisa64r6el", "gnuabi64") => "mips64r6el",
        ("mipsisa64r6el", "gnuabin32") => "mipsn32r6el",
        ("powerpc", "gnuspe") => "powerpcspe",
        ("powerpc64", _)   => "ppc64",
        ("powerpc64le", _) => "ppc64el",
        ("i586", _)  => "i386",
        ("i686", _)  => "i386",
        ("x86", _)   => "i386",
        ("x86_64", "gnux32") => "x32",
        ("x86_64", _) => "amd64",
        (arm, gnueabi) if arm.starts_with("arm") && gnueabi.ends_with("hf") => "armhf",
        (arm, _) if arm.starts_with("arm") => "armel",
        (other_arch, _) => other_arch,
    }
}

#[test]
fn assets() {
    let a = Asset::new(
        PathBuf::from("foo/bar"),
        PathBuf::from("baz/"),
        0o644,
    );
    assert_eq!("baz/bar", a.target_path.to_str().unwrap());

    let a = Asset::new(
        PathBuf::from("foo/bar"),
        PathBuf::from("/baz/quz"),
        0o644,
    );
    assert_eq!("baz/quz", a.target_path.to_str().unwrap());
}
