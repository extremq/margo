use serde::{Deserialize, Serialize};
use snafu::prelude::*;
use std::{
    fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    str,
};
use url::Url;

#[derive(Debug, argh::FromArgs)]
/// Manage a static crate registry
struct Args {
    #[argh(subcommand)]
    subcommand: Subcommand,
}

#[derive(Debug, argh::FromArgs)]
#[argh(subcommand)]
enum Subcommand {
    Init(InitArgs),
    Add(AddArgs),
}

/// Initialize a new registry
#[derive(Debug, argh::FromArgs)]
#[argh(subcommand)]
#[argh(name = "init")]
struct InitArgs {
    /// the URL that the registry is hosted at
    #[argh(option)]
    base_url: Url,

    #[argh(positional)]
    path: PathBuf,
}

/// Add a crate to the registry
#[derive(Debug, argh::FromArgs)]
#[argh(subcommand)]
#[argh(name = "add")]
struct AddArgs {
    /// path to the registry to modify
    #[argh(option)]
    registry: PathBuf,

    #[argh(positional)]
    path: PathBuf,
}

#[snafu::report]
fn main() -> Result<(), Error> {
    let args: Args = argh::from_env();

    let global = Global::new()?;
    let global = Box::leak(Box::new(global));

    match args.subcommand {
        Subcommand::Init(init) => do_init(global, init)?,
        Subcommand::Add(add) => do_add(global, add)?,
    }

    Ok(())
}

#[derive(Debug, Snafu)]
enum Error {
    #[snafu(display("Could not initialize global variables"))]
    #[snafu(context(false))]
    Global { source: GlobalError },

    #[snafu(transparent)]
    Initialize { source: InitializeError },

    #[snafu(transparent)]
    Open { source: OpenError },

    #[snafu(transparent)]
    Add { source: AddError },
}

fn do_init(_global: &Global, init: InitArgs) -> Result<(), InitializeError> {
    let config = ConfigV1 {
        base_url: init.base_url,
    };

    Registry::initialize(config, &init.path)?;

    Ok(())
}

fn do_add(global: &Global, add: AddArgs) -> Result<(), Error> {
    let r = Registry::open(&add.registry)?;
    r.add(global, &add.path)?;

    Ok(())
}

#[derive(Debug)]
struct Registry {
    path: PathBuf,
    config: ConfigV1,
}

impl Registry {
    fn initialize(config: ConfigV1, path: impl Into<PathBuf>) -> Result<Self, InitializeError> {
        use initialize_error::*;

        let path = path.into();

        println!("Initializing registry in `{}`", path.display());

        fs::create_dir_all(&path).context(RegistryCreateSnafu)?;

        let config_toml_path = path.join(CONFIG_FILE_NAME);
        let config = Config::V1(config);
        let config_toml = toml::to_string(&config).context(ConfigTomlSerializeSnafu)?;
        fs::write(&config_toml_path, config_toml).context(ConfigTomlWriteSnafu {
            path: &config_toml_path,
        })?;

        let Config::V1(config) = config;

        let dl = format!(
            "{base_url}crates/{{lowerprefix}}/{{crate}}/{{version}}.crate",
            base_url = config.base_url,
        );

        let config_json_path = path.join("config.json");
        let config_json = config_json::Root {
            dl,
            api: None,
            auth_required: false,
        };
        let config_json = serde_json::to_string(&config_json).context(ConfigJsonSerializeSnafu)?;
        fs::write(&config_json_path, config_json).context(ConfigJsonWriteSnafu {
            path: &config_json_path,
        })?;

        Ok(Self { path, config })
    }

    fn open(path: impl Into<PathBuf>) -> Result<Self, OpenError> {
        use open_error::*;

        let path = path.into();

        let config_path = path.join(CONFIG_FILE_NAME);
        let config = fs::read_to_string(&config_path).context(ReadSnafu { path: &config_path })?;
        let Config::V1(config) =
            toml::from_str(&config).context(DeserializeSnafu { path: &config_path })?;

        Ok(Self { path, config })
    }

    fn add(&self, global: &Global, crate_path: impl AsRef<Path>) -> Result<(), AddError> {
        use add_error::*;

        let crate_path = crate_path.as_ref();

        println!("Adding crate `{}` to registry", crate_path.display());

        let crate_file = fs::read(crate_path).context(ReadCrateSnafu)?;

        use sha2::Digest;
        let checksum = sha2::Sha256::digest(&crate_file);
        let checksum_hex = hex::encode(checksum);

        let cargo_toml = extract_root_cargo_toml(&crate_file)?.context(CargoTomlMissingSnafu)?;

        let cargo_toml = String::from_utf8(cargo_toml).context(CargoTomlUtf8Snafu)?;
        let cargo_toml = toml::from_str(&cargo_toml).context(CargoTomlMalformedSnafu)?;

        let index_entry =
            adapt_cargo_toml_to_index_entry(global, &self.config, cargo_toml, checksum_hex);

        let mut index_path = self.path.clone();
        index_entry.name.append_prefix_directories(&mut index_path);
        fs::create_dir_all(&index_path).context(IndexDirSnafu { path: &index_path })?;
        index_path.push(&index_entry.name);

        // FUTURE: Add `yank` subcommand
        // FUTURE: Add `remove` subcommand
        // FUTURE: Handle republishing the same version
        // FUTURE: Stronger file system consistency (atomic file overwrites, rollbacks on error)
        let entry = serde_json::to_string(&index_entry).context(IndexEntrySerializeSnafu)?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&index_path)
            .context(IndexOpenSnafu { path: &index_path })?;

        (|| {
            file.write_all(entry.as_bytes())?;
            file.write_all(b"\n")
        })()
        .context(IndexWriteSnafu { path: &index_path })?;

        println!("Wrote crate index to `{}`", index_path.display());

        let mut crate_dir = self.path.join(CRATE_DIR_NAME);
        index_entry.name.append_prefix_directories(&mut crate_dir);
        crate_dir.push(&index_entry.name);

        fs::create_dir_all(&crate_dir).context(CrateDirSnafu { path: &crate_dir })?;

        let mut crate_file_path = crate_dir;
        crate_file_path.push(&format!("{}.crate", index_entry.vers));

        fs::write(&crate_file_path, &crate_file).context(CrateWriteSnafu {
            path: &crate_file_path,
        })?;
        println!("Wrote crate to `{}`", crate_file_path.display());

        Ok(())
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum InitializeError {
    #[snafu(display("Could not create the registry directory"))]
    RegistryCreate { source: io::Error },

    #[snafu(display("Could not serialize the registry's internal configuration"))]
    ConfigTomlSerialize { source: toml::ser::Error },

    #[snafu(display("Could not write the registry's internal configuration to {}", path.display()))]
    ConfigTomlWrite { source: io::Error, path: PathBuf },

    #[snafu(display("Could not serialize the registry's public configuration"))]
    ConfigJsonSerialize { source: serde_json::Error },

    #[snafu(display("Could not write the registry's public configuration to {}", path.display()))]
    ConfigJsonWrite { source: io::Error, path: PathBuf },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum OpenError {
    #[snafu(display("Could not open the registry's internal configuration at {}", path.display()))]
    Read { source: io::Error, path: PathBuf },

    #[snafu(display("Could not deserialize the registry's internal configuration at {}", path.display()))]
    Deserialize {
        source: toml::de::Error,
        path: PathBuf,
    },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum AddError {
    #[snafu(display("Could not read the crate package"))]
    ReadCrate { source: io::Error },

    #[snafu(transparent)]
    CargoTomlExtract { source: ExtractRootCargoTomlError },

    #[snafu(display("The crate package does not contain a Cargo.toml file"))]
    CargoTomlMissing,

    #[snafu(display("The crate's Cargo.toml is not valid UTF-8"))]
    CargoTomlUtf8 { source: std::string::FromUtf8Error },

    #[snafu(display("The crate's Cargo.toml is malformed"))]
    CargoTomlMalformed { source: toml::de::Error },

    #[snafu(display("Could not create the crate's index directory {}", path.display()))]
    IndexDir { source: io::Error, path: PathBuf },

    #[snafu(display("Could not serialize the crate's index entry"))]
    IndexEntrySerialize { source: serde_json::Error },

    #[snafu(display("Could not open the crate's index file {}", path.display()))]
    IndexOpen { source: io::Error, path: PathBuf },

    #[snafu(display("Could not write the crate's index file {}", path.display()))]
    IndexWrite { source: io::Error, path: PathBuf },

    #[snafu(display("Could not create the crate directory {}", path.display()))]
    CrateDir { source: io::Error, path: PathBuf },

    #[snafu(display("Could not write the crate {}", path.display()))]
    CrateWrite { source: io::Error, path: PathBuf },
}

fn extract_root_cargo_toml(
    crate_data: &[u8],
) -> Result<Option<Vec<u8>>, ExtractRootCargoTomlError> {
    use extract_root_cargo_toml_error::*;

    let crate_data = flate2::read::GzDecoder::new(crate_data);
    let mut crate_data = tar::Archive::new(crate_data);

    let entries = crate_data.entries().context(EntriesSnafu)?;

    let mut dirname = None;

    for entry in entries {
        let mut entry = entry.context(EntrySnafu)?;
        let path = entry.path().context(PathSnafu)?;

        let dirname = match &mut dirname {
            Some(v) => v,
            None => {
                let Some(Component::Normal(first)) = path.components().next() else {
                    return MalformedSnafu.fail();
                };

                dirname.insert(first.to_owned())
            }
        };

        let fname = path.strip_prefix(dirname).context(PrefixSnafu)?;

        if fname == Path::new("Cargo.toml") {
            let mut data = vec![];
            entry.read_to_end(&mut data).context(ReadSnafu)?;
            return Ok(Some(data));
        }
    }

    Ok(None)
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum ExtractRootCargoTomlError {
    #[snafu(display("Could not get the entries of the crate package"))]
    Entries { source: io::Error },

    #[snafu(display("Could not get the next crate package entry"))]
    Entry { source: io::Error },

    #[snafu(display("Could not get the path of the crate package entry"))]
    Path { source: io::Error },

    #[snafu(display("The crate package was malformed"))]
    Malformed,

    #[snafu(display("Could not remove the path prefix from the crate package entry"))]
    Prefix { source: std::path::StripPrefixError },

    #[snafu(display("Could not read the crate package entry for Cargo.toml"))]
    Read { source: io::Error },
}

fn adapt_cargo_toml_to_index_entry(
    global: &Global,
    config: &ConfigV1,
    cargo_toml: cargo_toml::Root,
    checksum_hex: String,
) -> index_entry::Root {
    let mut deps: Vec<_> = cargo_toml
        .dependencies
        .into_iter()
        .map(|(name, dep)| adapt_dependency(global, config, dep, name))
        .collect();

    let build_deps = cargo_toml
        .build_dependencies
        .into_iter()
        .map(|(name, dep)| {
            let mut dep = adapt_dependency(global, config, dep, name);
            dep.kind = index_entry::DependencyKind::Build;
            dep
        });
    deps.extend(build_deps);

    for (target, defn) in cargo_toml.target {
        let target_deps = defn.dependencies.into_iter().map(|(name, dep)| {
            let mut dep = adapt_dependency(global, config, dep, name);
            dep.target = Some(target.clone());
            dep
        });
        deps.extend(target_deps);
    }

    // FUTURE: Opt-in to checking that all dependencies already exist

    index_entry::Root {
        name: cargo_toml.package.name,
        vers: cargo_toml.package.version,
        deps,
        cksum: checksum_hex,
        features: cargo_toml.features,
        yanked: false,
        links: cargo_toml.package.links,
        v: 2,
        features2: Default::default(),
        rust_version: cargo_toml.package.rust_version,
    }
}

fn adapt_dependency(
    global: &Global,
    config: &ConfigV1,
    dep: cargo_toml::Dependency,
    name: String,
) -> index_entry::Dependency {
    let cargo_toml::Dependency {
        version,
        features,
        optional,
        default_features,
        registry_index,
        package,
    } = dep;

    index_entry::Dependency {
        name,
        req: version,
        features,
        optional,
        default_features,
        target: None,
        kind: index_entry::DependencyKind::Normal,
        registry: adapt_index(global, config, registry_index),
        package,
    }
}

fn adapt_index(global: &Global, config: &ConfigV1, registry_index: Option<Url>) -> Option<Url> {
    // The dependency is in...
    match registry_index {
        // ...crates.io
        None => Some(global.crates_io_index_url.clone()),

        // ...this registry
        Some(url) if url == config.base_url => None,

        // ...another registry
        r => r,
    }
}

/// Only intended for the normalized Cargo.toml created for the
/// packaged crate.
mod cargo_toml {
    use serde::Deserialize;
    use std::collections::BTreeMap;
    use url::Url;

    use crate::common::CrateName;

    pub type Dependencies = BTreeMap<String, Dependency>;

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub struct Root {
        pub package: Package,

        #[serde(default)]
        pub features: BTreeMap<String, Vec<String>>,

        #[serde(default)]
        pub dependencies: Dependencies,

        #[serde(default)]
        pub build_dependencies: Dependencies,

        #[serde(default)]
        pub target: BTreeMap<String, Target>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub struct Package {
        pub name: CrateName,

        pub version: String,

        #[serde(default)]
        pub links: Option<String>,

        #[serde(default)]
        pub rust_version: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub struct Dependency {
        pub version: String,

        #[serde(default)]
        pub features: Vec<String>,

        #[serde(default)]
        pub optional: bool,

        #[serde(default = "true_def")]
        pub default_features: bool,

        #[serde(default)]
        pub registry_index: Option<Url>,

        #[serde(default)]
        pub package: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    pub struct Target {
        #[serde(default)]
        pub dependencies: Dependencies,
    }

    fn true_def() -> bool {
        true
    }
}

const CONFIG_FILE_NAME: &str = "margo-config.toml";
const CRATE_DIR_NAME: &str = "crates";

const CRATES_IO_INDEX_URL: &str = "sparse+https://index.crates.io/";

#[derive(Debug)]
struct Global {
    crates_io_index_url: Url,
}

impl Global {
    fn new() -> Result<Self, GlobalError> {
        use global_error::*;

        Ok(Self {
            crates_io_index_url: CRATES_IO_INDEX_URL.parse().context(CratesIoIndexUrlSnafu)?,
        })
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum GlobalError {
    #[snafu(display("Could not parse the crates.io index URL"))]
    CratesIoIndexUrl { source: url::ParseError },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "version")]
enum Config {
    #[serde(rename = "1")]
    V1(ConfigV1),
}

#[derive(Debug, Serialize, Deserialize)]
struct ConfigV1 {
    base_url: Url,
}

mod config_json {
    use serde::Serialize;

    #[derive(Debug, Serialize)]
    #[serde(rename_all = "kebab-case")]
    pub struct Root {
        // This field cannot be a `url::Url` because that type
        // percent-escapes the `{` and `}` characters. Cargo performs
        // string-replacement on this value based on those literal `{`
        // and `}` characters.
        pub dl: String,

        pub api: Option<String>, // Modified

        /// A private registry requires all operations to be authenticated.
        ///
        /// This includes API requests, crate downloads and sparse
        /// index updates.
        pub auth_required: bool,
    }
}

mod index_entry {
    use serde::Serialize;
    use std::collections::BTreeMap;
    use url::Url;

    use crate::common::CrateName;

    #[derive(Debug, Serialize)]
    pub struct Root {
        /// The name of the package.
        pub name: CrateName,

        /// The version of the package this row is describing.
        ///
        /// This must be a valid version number according to the
        /// Semantic Versioning 2.0.0 spec at https://semver.org/.
        pub vers: String,

        /// Direct dependencies of the package.
        pub deps: Vec<Dependency>,

        /// A SHA256 checksum of the `.crate` file.
        pub cksum: String,

        /// Set of features defined for the package.
        ///
        /// Each feature maps to features or dependencies it enables.
        pub features: BTreeMap<String, Vec<String>>,

        /// Boolean of whether or not this version has been yanked.
        pub yanked: bool,

        /// The `links` value from the package's manifest.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub links: Option<String>,

        /// The schema version of this entry.
        //
        /// If this not specified, it should be interpreted as the default of 1.
        //
        /// Cargo (starting with version 1.51) will ignore versions it does not
        /// recognize. This provides a method to safely introduce changes to index
        /// entries and allow older versions of cargo to ignore newer entries it
        /// doesn't understand. Versions older than 1.51 ignore this field, and
        /// thus may misinterpret the meaning of the index entry.
        //
        /// The current values are:
        //
        /// * 1: The schema as documented here, not including newer additions.
        ///      This is honored in Rust version 1.51 and newer.
        /// * 2: The addition of the `features2` field.
        ///      This is honored in Rust version 1.60 and newer.
        pub v: u32,

        /// Features with new, extended syntax, such as namespaced
        /// features (`dep:`) and weak dependencies (`pkg?/feat`).
        //
        /// This is separated from `features` because versions older than 1.19
        /// will fail to load due to not being able to parse the new syntax, even
        /// with a `Cargo.lock` file.
        //
        /// Cargo will merge any values listed here with the "features" field.
        //
        /// If this field is included, the "v" field should be set to at least 2.
        //
        /// Registries are not required to use this field for extended feature
        /// syntax, they are allowed to include those in the "features" field.
        /// Using this is only necessary if the registry wants to support cargo
        /// versions older than 1.19, which in practice is only crates.io since
        /// those older versions do not support other registries.
        #[serde(skip_serializing_if = "BTreeMap::is_empty")]
        pub features2: BTreeMap<String, Vec<String>>,

        /// The minimal supported Rust version
        ///
        /// This must be a valid version requirement without an operator (e.g. no `=`)
        #[serde(skip_serializing_if = "Option::is_none")]
        pub rust_version: Option<String>,
    }

    #[derive(Debug, Serialize)]
    pub struct Dependency {
        /// Name of the dependency.
        ///
        /// If the dependency is renamed from the original package
        /// name, this is the new name. The original package name is
        /// stored in the `package` field.
        pub name: String,

        /// The SemVer requirement for this dependency.
        ///
        /// This must be a valid version requirement defined at
        /// https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html.
        pub req: String,

        /// Features enabled for this dependency.
        pub features: Vec<String>,

        /// Whether or not this is an optional dependency.
        pub optional: bool,

        /// Whether or not default features are enabled.
        pub default_features: bool,

        /// The target platform for the dependency.
        ///
        /// A string such as `cfg(windows)`.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub target: Option<String>,

        /// The dependency kind.
        ///
        /// Note: this is a required field, but a small number of entries
        /// exist in the crates.io index with either a missing or null
        /// `kind` field due to implementation bugs.
        pub kind: DependencyKind,

        /// The URL of the index of the registry where this dependency
        /// is from.
        ///
        /// If not specified or null, it is assumed the dependency is
        /// in the current registry.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub registry: Option<Url>,

        /// If the dependency is renamed, this is the actual package
        /// name.
        ///
        /// If not specified or null, this dependency is not renamed.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub package: Option<String>,
    }

    #[derive(Debug, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum DependencyKind {
        #[allow(unused)]
        // Stored in the index, but not actually used by Cargo
        Dev,
        Build,
        Normal,
    }
}

mod common {
    use ascii::{AsciiChar, AsciiStr, AsciiString};
    use serde::{de::Error, Deserialize, Serialize};
    use snafu::prelude::*;
    use std::{
        ops,
        path::{Path, PathBuf},
    };

    /// Contains only alphanumeric, `-`, or `_` characters.
    #[derive(Debug, Serialize)]
    pub struct CrateName(AsciiString);

    impl CrateName {
        pub fn len(&self) -> usize {
            self.0.len()
        }

        pub fn append_prefix_directories(&self, index_path: &mut PathBuf) {
            match self.len() {
                0 => unreachable!(),
                1 => index_path.push("1"),
                2 => index_path.push("2"),
                3 => {
                    let a = &self[0..1];

                    index_path.push("3");
                    index_path.push(a.as_str());
                }
                _ => {
                    let ab = &self[0..2];
                    let cd = &self[2..4];

                    index_path.push(ab.as_str());
                    index_path.push(cd.as_str());
                }
            };
        }
    }

    impl TryFrom<AsciiString> for CrateName {
        type Error = CrateNameError;

        fn try_from(value: AsciiString) -> Result<Self, Self::Error> {
            use crate_name_error::*;

            let first = value.first().context(EmptySnafu)?;
            ensure!(first.is_alphabetic(), InitialAlphaSnafu);

            if let Some(chr) = value.chars().find(|&chr| !valid_crate_name_char(chr)) {
                return ContainsInvalidCharSnafu { chr }.fail();
            }

            Ok(Self(value))
        }
    }

    #[derive(Debug, Snafu)]
    #[snafu(module)]
    pub enum CrateNameError {
        #[snafu(display("The crate name cannot be empty"))]
        Empty,

        #[snafu(display("The crate name must start with an alphabetic character"))]
        InitialAlpha,

        #[snafu(display("The crate name must only contain alphanumeric characters, hyphen (-) or underscore (_), not {chr}"))]
        ContainsInvalidChar { chr: char },
    }

    impl<'de> Deserialize<'de> for CrateName {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            let ascii: AsciiString = Deserialize::deserialize(deserializer)?;
            Self::try_from(ascii).map_err(D::Error::custom)
        }
    }

    impl ops::Index<ops::Range<usize>> for CrateName {
        type Output = AsciiStr;

        fn index(&self, index: ops::Range<usize>) -> &Self::Output {
            self.0.index(index)
        }
    }

    impl AsRef<Path> for CrateName {
        fn as_ref(&self) -> &Path {
            self.0.as_str().as_ref()
        }
    }

    fn valid_crate_name_char(chr: AsciiChar) -> bool {
        chr.is_alphanumeric() || chr == AsciiChar::UnderScore || chr == AsciiChar::Minus
    }
}
