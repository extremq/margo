use common::CrateName;
use semver::Version;
use serde::{Deserialize, Serialize};
use snafu::prelude::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fmt,
    fs::{self, File},
    io::{self, BufRead, BufReader, BufWriter, Read, Write},
    path::{Component, Path, PathBuf},
    str,
};
use url::Url;

#[cfg(feature = "html")]
mod html;

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
    Remove(RemoveArgs),
    Yank(YankArgs),
    List(ListArgs),
    GenerateHtml(GenerateHtmlArgs),
}

/// Initialize a new registry
#[derive(Debug, argh::FromArgs)]
#[argh(subcommand)]
#[argh(name = "init")]
struct InitArgs {
    /// the URL that the registry is hosted at
    #[argh(option)]
    base_url: Option<Url>,

    /// use default values where possible, instead of prompting for them
    #[argh(switch)]
    defaults: bool,

    /// require HTTP authentication to access crates
    #[argh(option)]
    auth_required: Option<bool>,

    /// generate an HTML file showing crates in the index
    #[argh(option)]
    html: Option<bool>,

    /// name you'd like to suggest other people call your registry
    #[argh(option)]
    html_suggested_registry_name: Option<String>,

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
    registry: Option<PathBuf>,

    #[argh(positional)]
    path: Vec<PathBuf>,
}

/// Remove a crate from the registry
#[derive(Debug, argh::FromArgs)]
#[argh(subcommand)]
#[argh(name = "rm")]
struct RemoveArgs {
    /// path to the registry to modify
    #[argh(option)]
    registry: Option<PathBuf>,

    // FUTURE: Allow removing all versions at once?
    /// the version of the crate
    #[argh(option)]
    version: Version,

    #[argh(positional)]
    name: CrateName,
}

/// Generate an HTML index for the registry
#[derive(Debug, argh::FromArgs)]
#[argh(subcommand)]
#[argh(name = "generate-html")]
struct GenerateHtmlArgs {
    /// path to the registry to modify
    #[argh(option)]
    registry: Option<PathBuf>,
}

/// Yank a version of a crate from the registry
#[derive(Debug, argh::FromArgs)]
#[argh(subcommand)]
#[argh(name = "yank")]
struct YankArgs {
    /// path to the registry to modify
    #[argh(option)]
    registry: Option<PathBuf>,

    /// undo a previous yank
    #[argh(switch)]
    undo: bool,

    /// the version of the crate
    #[argh(option)]
    version: Version,

    /// the name of the crate
    #[argh(positional)]
    name: CrateName,
}

/// List all crates and their versions in the registry
#[derive(Debug, argh::FromArgs)]
#[argh(subcommand)]
#[argh(name = "list")]
struct ListArgs {
    /// path to the registry to list
    #[argh(option)]
    registry: Option<PathBuf>,
}

#[snafu::report]
fn main() -> Result<(), Error> {
    let args: Args = argh::from_env();

    let global = Global::new()?;
    let global = Box::leak(Box::new(global));

    match args.subcommand {
        Subcommand::Init(init) => do_init(global, init)?,
        Subcommand::Add(add) => do_add(global, add)?,
        Subcommand::Remove(rm) => do_remove(global, rm)?,
        Subcommand::Yank(yank) => do_yank(global, yank)?,
        Subcommand::List(list) => do_list(global, list)?,
        Subcommand::GenerateHtml(html) => do_generate_html(global, html)?,
    }

    Ok(())
}

#[derive(Debug, Snafu)]
enum Error {
    #[snafu(display("Could not initialize global variables"))]
    #[snafu(context(false))]
    Global {
        #[snafu(source(from(GlobalError, Box::new)))]
        source: Box<GlobalError>,
    },

    #[snafu(transparent)]
    Initialize {
        #[snafu(source(from(DoInitializeError, Box::new)))]
        source: Box<DoInitializeError>,
    },

    #[snafu(transparent)]
    Open {
        #[snafu(source(from(DiscoverRegistryError, Box::new)))]
        source: Box<DiscoverRegistryError>,
    },

    #[snafu(transparent)]
    Add {
        #[snafu(source(from(AddError, Box::new)))]
        source: Box<AddError>,
    },

    #[snafu(transparent)]
    Remove {
        #[snafu(source(from(RemoveError, Box::new)))]
        source: Box<RemoveError>,
    },

    #[snafu(transparent)]
    Html {
        #[snafu(source(from(HtmlError, Box::new)))]
        source: Box<HtmlError>,
    },

    #[snafu(transparent)]
    Yank {
        #[snafu(source(from(YankError, Box::new)))]
        source: Box<YankError>,
    },
}

trait UnwrapOrDialog<T> {
    fn apply_default(self, use_default: bool, value: impl Into<T>) -> Self;

    fn unwrap_or_dialog(self, f: impl FnOnce() -> dialoguer::Result<T>) -> dialoguer::Result<T>;
}

impl<T> UnwrapOrDialog<T> for Option<T> {
    fn apply_default(self, use_default: bool, value: impl Into<T>) -> Self {
        if self.is_none() && use_default {
            Some(value.into())
        } else {
            self
        }
    }

    fn unwrap_or_dialog(self, f: impl FnOnce() -> dialoguer::Result<T>) -> dialoguer::Result<T> {
        match self {
            Some(v) => Ok(v),
            None => f(),
        }
    }
}

fn do_init(_global: &Global, init: InitArgs) -> Result<(), DoInitializeError> {
    use do_initialize_error::*;

    let base_url = init
        .base_url
        .unwrap_or_dialog(|| {
            dialoguer::Input::new()
                .with_prompt("What URL will the registry be served from")
                .interact()
        })
        .context(BaseUrlSnafu)?;

    let auth_required = init
        .auth_required
        .apply_default(init.defaults, ConfigV1::USER_DEFAULT_AUTH_REQUIRED)
        .unwrap_or_dialog(|| {
            dialoguer::Confirm::new()
                .default(ConfigV1::USER_DEFAULT_AUTH_REQUIRED)
                .show_default(true)
                .with_prompt("Require HTTP authentication to access crates?")
                .interact()
        })
        .context(AuthRequiredSnafu)?;

    let enabled = init
        .html
        .apply_default(init.defaults, ConfigV1Html::USER_DEFAULT_ENABLED)
        .unwrap_or_dialog(|| {
            dialoguer::Confirm::new()
                .default(ConfigV1Html::USER_DEFAULT_ENABLED)
                .show_default(true)
                .with_prompt("Enable HTML index generation?")
                .interact()
        })
        .context(HtmlEnabledSnafu)?;

    let suggested_registry_name = if enabled {
        let name = init
            .html_suggested_registry_name
            .apply_default(
                init.defaults,
                ConfigV1Html::USER_DEFAULT_SUGGESTED_REGISTRY_NAME,
            )
            .unwrap_or_dialog(|| {
                dialoguer::Input::new()
                    .default(ConfigV1Html::USER_DEFAULT_SUGGESTED_REGISTRY_NAME.to_owned())
                    .show_default(true)
                    .with_prompt("Name you'd like to suggest other people call your registry")
                    .interact()
            })
            .context(HtmlSuggestedRegistryNameSnafu)?;

        Some(name)
    } else {
        None
    };

    let config = ConfigV1 {
        base_url,
        auth_required,
        html: ConfigV1Html {
            enabled,
            suggested_registry_name,
        },
    };

    let r = Registry::initialize(config, &init.path)?;

    if r.config.html.enabled {
        let res = r.generate_html();

        if cfg!(feature = "html") {
            res?;
        } else if let Err(e) = res {
            eprintln!("Warning: {e}");
        }
    }

    Ok(())
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum DoInitializeError {
    #[snafu(display("Could not determine the base URL"))]
    BaseUrl { source: dialoguer::Error },

    #[snafu(display("Could not determine if HTTP authorization is required"))]
    AuthRequired { source: dialoguer::Error },

    #[snafu(display("Could not determine if HTML generation is enabled"))]
    HtmlEnabled { source: dialoguer::Error },

    #[snafu(display("Could not determine the suggested registry name"))]
    HtmlSuggestedRegistryName { source: dialoguer::Error },

    #[snafu(transparent)]
    Initialize { source: InitializeError },

    #[snafu(transparent)]
    Html { source: HtmlError },
}

fn do_add(global: &Global, add: AddArgs) -> Result<(), Error> {
    let r = discover_registry(add.registry)?;

    for i in add.path {
        r.add(global, i)?;
    }
    r.maybe_generate_html()?;

    Ok(())
}

fn do_remove(_global: &Global, rm: RemoveArgs) -> Result<(), Error> {
    let r = discover_registry(rm.registry)?;

    r.remove(rm.name, rm.version)?;
    r.maybe_generate_html()?;

    Ok(())
}

fn do_generate_html(_global: &Global, html: GenerateHtmlArgs) -> Result<(), Error> {
    let r = discover_registry(html.registry)?;
    r.generate_html()?;
    Ok(())
}

fn do_yank(_global: &Global, yank: YankArgs) -> Result<(), Error> {
    let r = discover_registry(yank.registry)?;

    r.yank(yank.name, yank.version, !yank.undo)?;
    r.maybe_generate_html()?;

    Ok(())
}

fn do_list(_global: &Global, list: ListArgs) -> Result<(), Error> {
    let r = discover_registry(list.registry)?;

    let crates = r.list_all().unwrap();

    #[derive(Default)]
    struct Max(usize, String);

    impl Max {
        fn push(&mut self, v: impl fmt::Display) {
            use std::fmt::Write;

            let Self(m, s) = self;

            s.clear();
            _ = write!(s, "{v}");
            *m = usize::max(*m, s.len());
        }

        fn max(&self) -> usize {
            self.0
        }
    }

    let mut max_c = Max::default();
    let mut max_v = Max::default();

    for (crate_, versions) in &crates {
        max_c.push(crate_);
        for version in versions.keys() {
            max_v.push(version);
        }
    }

    let max_c = max_c.max();
    let max_v = max_v.max();

    for (crate_, versions) in crates {
        for version in versions.keys() {
            println!("{crate_:<max_c$} {version:<max_v$}");
        }
    }

    Ok(())
}

fn discover_registry(path: Option<PathBuf>) -> Result<Registry, DiscoverRegistryError> {
    use discover_registry_error::*;

    match path {
        Some(p) => Registry::open(p).context(OpenSnafu),
        None => {
            let cwd = env::current_dir().context(CurrentDirSnafu)?;

            match Registry::open(cwd) {
                Ok(r) => Ok(r),
                Err(e) if e.is_not_found() => FallbackNotFoundSnafu.fail(),
                Err(e) => Err(e).context(FallbackOpenSnafu)?,
            }
        }
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum DiscoverRegistryError {
    #[snafu(display("Could not open the specified registry"))]
    Open { source: OpenError },

    #[snafu(display("Could not determine the current directory, {}", Self::TRY_THIS))]
    CurrentDir { source: io::Error },

    #[snafu(display(
        "The current directory does not contain a registry, {}",
        Self::TRY_THIS,
    ))]
    FallbackNotFound,

    #[snafu(display("Could not open the registry in the current directory"))]
    FallbackOpen { source: OpenError },
}

impl DiscoverRegistryError {
    const TRY_THIS: &'static str = "please use the `--registry` command line option";
}

#[derive(Debug)]
struct Registry {
    path: PathBuf,
    config: ConfigV1,
}

type Index = BTreeMap<Version, index_entry::Root>;
type ListAll = BTreeMap<CrateName, Index>;

impl Registry {
    fn initialize(config: ConfigV1, path: impl Into<PathBuf>) -> Result<Self, InitializeError> {
        use initialize_error::*;

        let config = config.normalize();
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
        let auth_required = config.auth_required;

        let this = Self { path, config };

        let config_json_path = this.config_json_path();
        let config_json = config_json::Root {
            dl,
            api: None,
            auth_required,
        };
        let config_json = serde_json::to_string(&config_json).context(ConfigJsonSerializeSnafu)?;
        fs::write(&config_json_path, config_json).context(ConfigJsonWriteSnafu {
            path: &config_json_path,
        })?;

        Ok(this)
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

        let index_path = self.index_file_path_for(&index_entry.name);
        if let Some(path) = index_path.parent() {
            fs::create_dir_all(path).context(IndexDirSnafu { path })?;
        }

        let crate_file_path = self.crate_file_path_for(&index_entry.name, &index_entry.vers);
        if let Some(path) = crate_file_path.parent() {
            fs::create_dir_all(path).context(CrateDirSnafu { path })?;
        }

        // FUTURE: Stronger file system consistency (atomic file overwrites, rollbacks on error)
        // FUTURE: "transactional" adding of multiple crates

        self.read_modify_write(&index_entry.name.clone(), |index_file| {
            index_file.insert(index_entry.vers.clone(), index_entry);
            Ok::<_, AddError>(())
        })?;

        println!("Wrote crate index to `{}`", index_path.display());

        fs::write(&crate_file_path, &crate_file).context(CrateWriteSnafu {
            path: &crate_file_path,
        })?;
        println!("Wrote crate to `{}`", crate_file_path.display());

        Ok(())
    }

    fn remove(&self, name: CrateName, version: Version) -> Result<(), RemoveError> {
        use remove_error::*;

        self.read_modify_write(&name, |index| {
            index.remove(&version);
            Ok::<_, RemoveError>(())
        })?;

        let crate_file = self.crate_file_path_for(&name, &version);
        match fs::remove_file(&crate_file) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).context(DeleteSnafu { path: crate_file }),
        }
    }

    #[cfg(feature = "html")]
    fn generate_html(&self) -> Result<(), HtmlError> {
        html::write(self)
    }

    #[cfg(not(feature = "html"))]
    fn generate_html(&self) -> Result<(), HtmlError> {
        Err(HtmlError)
    }

    fn maybe_generate_html(&self) -> Result<(), HtmlError> {
        if self.config.html.enabled {
            self.generate_html()
        } else {
            Ok(())
        }
    }

    fn yank(&self, name: CrateName, version: Version, yanked: bool) -> Result<(), YankError> {
        use yank_error::*;

        self.read_modify_write(&name, |index| {
            let entry = index.get_mut(&version).context(VersionSnafu)?;
            entry.yanked = yanked;
            Ok(())
        })
    }

    fn read_modify_write<T, E>(
        &self,
        name: &CrateName,
        modify: impl FnOnce(&mut Index) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<ReadModifyWriteError>,
    {
        use read_modify_write_error::*;

        let path = self.index_file_path_for(name);
        let mut index = Self::parse_index_file(&path).context(IndexParseSnafu { path: &path })?;

        let val = modify(&mut index)?;

        Self::write_index_file(index, &path).context(IndexWriteSnafu { path })?;

        Ok(val)
    }

    fn list_crate_files(
        crate_dir: &Path,
    ) -> impl Iterator<Item = walkdir::Result<walkdir::DirEntry>> {
        walkdir::WalkDir::new(crate_dir)
            .into_iter()
            .flat_map(|entry| {
                let Ok(entry) = entry else { return Some(entry) };

                let fname = entry.path().file_name()?;
                let fname = Path::new(fname);

                let extension = fname.extension()?;
                if extension == "crate" {
                    Some(Ok(entry))
                } else {
                    None
                }
            })
    }

    fn list_index_files(&self) -> Result<BTreeSet<PathBuf>, ListIndexFilesError> {
        use list_index_files_error::*;

        let crate_dir = self.crate_dir();

        let index_files = Self::list_crate_files(&crate_dir)
            .map(|entry| {
                let entry = entry.context(WalkdirSnafu { path: &crate_dir })?;

                let mut path = entry.into_path();
                path.pop();

                let subdir = path.strip_prefix(&crate_dir).context(PrefixSnafu {
                    path: &path,
                    prefix: &crate_dir,
                })?;
                let index_path = self.path.join(subdir);
                Ok(index_path)
            })
            .collect::<Result<BTreeSet<_>, ListIndexFilesError>>();

        match index_files {
            Err(e) if e.is_not_found() => Ok(Default::default()),
            r => r,
        }
    }

    fn list_all(&self) -> Result<ListAll, ListAllError> {
        use list_all_error::*;

        let mut crates = BTreeMap::new();

        for path in self.list_index_files()? {
            let index = Self::parse_index_file(&path).context(ParseSnafu { path })?;

            if let Some(entry) = index.values().next() {
                crates.insert(entry.name.clone(), index);
            }
        }

        Ok(crates)
    }

    fn parse_index_file(path: &Path) -> Result<Index, ParseIndexError> {
        use parse_index_error::*;

        let index_file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Default::default()),
            Err(e) => Err(e).context(OpenSnafu)?,
        };
        let index_file = BufReader::new(index_file);

        let mut index = BTreeMap::new();

        for (i, line) in index_file.lines().enumerate() {
            let line = line.context(ReadSnafu { line: i })?;
            let entry =
                serde_json::from_str::<index_entry::Root>(&line).context(ParseSnafu { line: i })?;

            index.insert(entry.vers.clone(), entry);
        }

        Ok(index)
    }

    fn write_index_file(index_file: Index, path: &Path) -> Result<(), WriteIndexError> {
        use write_index_error::*;

        let file = File::create(path).context(OpenSnafu)?;
        let mut file = BufWriter::new(file);

        for entry in index_file.values() {
            serde_json::to_writer(&mut file, entry).context(EntrySerializeSnafu)?;
            file.write_all(b"\n").context(EntryNewlineSnafu)?;
        }

        Ok(())
    }

    fn crate_dir(&self) -> PathBuf {
        self.path.join(CRATE_DIR_NAME)
    }

    #[cfg(test)]
    fn margo_config_toml_path(&self) -> PathBuf {
        self.path.join(CONFIG_FILE_NAME)
    }

    fn config_json_path(&self) -> PathBuf {
        self.path.join("config.json")
    }

    fn index_file_path_for(&self, name: &CrateName) -> PathBuf {
        let mut index_path = self.path.clone();
        name.append_prefix_directories(&mut index_path);
        index_path.push(name);
        index_path
    }

    fn crate_dir_for(&self, name: &CrateName) -> PathBuf {
        let mut crate_dir = self.crate_dir();
        name.append_prefix_directories(&mut crate_dir);
        crate_dir.push(name);
        crate_dir
    }

    fn crate_file_path_for(&self, name: &CrateName, version: &Version) -> PathBuf {
        let mut crate_file_path = self.crate_dir_for(name);
        crate_file_path.push(format!("{}.crate", version));
        crate_file_path
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

impl OpenError {
    fn is_not_found(&self) -> bool {
        match self {
            Self::Read { source, .. } => source.kind() == io::ErrorKind::NotFound,
            Self::Deserialize { .. } => false,
        }
    }
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

    #[snafu(transparent)]
    IndexModify { source: ReadModifyWriteError },

    #[snafu(display("Could not create the crate directory {}", path.display()))]
    CrateDir { source: io::Error, path: PathBuf },

    #[snafu(display("Could not write the crate {}", path.display()))]
    CrateWrite { source: io::Error, path: PathBuf },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum RemoveError {
    #[snafu(transparent)]
    IndexModify { source: ReadModifyWriteError },

    #[snafu(display("Could not delete the crate file {}", path.display()))]
    Delete { source: io::Error, path: PathBuf },
}

#[cfg(feature = "html")]
use html::Error as HtmlError;

#[cfg(not(feature = "html"))]
#[derive(Debug, Snafu)]
#[snafu(display("Margo was not compiled with the HTML feature enabled. This binary will not be able to generate HTML files"))]
struct HtmlError;

#[derive(Debug, Snafu)]
#[snafu(module)]
enum YankError {
    #[snafu(display("The version does not exist in the index"))]
    Version,

    #[snafu(transparent)]
    Modify { source: ReadModifyWriteError },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum ReadModifyWriteError {
    #[snafu(display("Could not parse the crate's index file {}", path.display()))]
    IndexParse {
        source: ParseIndexError,
        path: PathBuf,
    },

    #[snafu(display("Could not write the crate's index file {}", path.display()))]
    IndexWrite {
        source: WriteIndexError,
        path: PathBuf,
    },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum ListIndexFilesError {
    #[snafu(display("Could not enumerate the crate directory `{}`", path.display()))]
    Walkdir {
        source: walkdir::Error,
        path: PathBuf,
    },

    #[snafu(display(
        "Could not remove the path prefix `{prefix}` from the crate package entry `{path}`",
        prefix = prefix.display(),
        path = path.display(),
    ))]
    Prefix {
        source: std::path::StripPrefixError,
        path: PathBuf,
        prefix: PathBuf,
    },
}

impl ListIndexFilesError {
    fn is_not_found(&self) -> bool {
        if let Self::Walkdir { source, .. } = self {
            if let Some(e) = source.io_error() {
                if e.kind() == io::ErrorKind::NotFound {
                    return true;
                }
            }
        }

        false
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum ListAllError {
    #[snafu(display("Unable to list the crate index files"))]
    #[snafu(context(false))]
    ListIndex { source: ListIndexFilesError },

    #[snafu(display("Unable to parse the crate index file at `{}`", path.display()))]
    Parse {
        source: ParseIndexError,
        path: PathBuf,
    },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum ParseIndexError {
    #[snafu(display("Could not open the file"))]
    Open { source: io::Error },

    #[snafu(display("Could not read line {line}"))]
    Read { source: io::Error, line: usize },

    #[snafu(display("Could not parse line {line}"))]
    Parse {
        source: serde_json::Error,
        line: usize,
    },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum WriteIndexError {
    #[snafu(display("Could not open the file"))]
    Open { source: io::Error },

    #[snafu(display("Could not serialize the entry"))]
    EntrySerialize { source: serde_json::Error },

    #[snafu(display("Could not write the entry's newline"))]
    EntryNewline { source: io::Error },
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
    mut cargo_toml: cargo_toml::Root,
    checksum_hex: String,
) -> index_entry::Root {
    // Remove features that refer to dev-dependencies as we don't
    // track those anyway.
    {
        // Ignore dependencies that also occur as a regular or build
        // dependency, as we *do* track those.
        let reg_dep_names = cargo_toml.dependencies.keys();
        let build_dep_names = cargo_toml.build_dependencies.keys();
        let mut only_dev_dep_names = cargo_toml.dev_dependencies.keys().collect::<BTreeSet<_>>();
        for name in reg_dep_names.chain(build_dep_names) {
            only_dev_dep_names.remove(name);
        }

        for name in only_dev_dep_names {
            // We don't care about the official package name here as the
            // feature syntax has to match the user-specified dependency
            // name.
            let prefix = format!("{name}/");

            for enabled in cargo_toml.features.values_mut() {
                enabled.retain(|enable| !enable.starts_with(&prefix));
            }
        }
    }

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
    use semver::{Version, VersionReq};
    use serde::Deserialize;
    use std::collections::BTreeMap;
    use url::Url;

    use crate::common::{CrateName, RustVersion};

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
        pub dev_dependencies: Dependencies,

        #[serde(default)]
        pub target: BTreeMap<String, Target>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub struct Package {
        pub name: CrateName,

        pub version: Version,

        #[serde(default)]
        pub links: Option<String>,

        #[serde(default)]
        pub rust_version: Option<RustVersion>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub struct Dependency {
        pub version: VersionReq,

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

const CRATES_IO_INDEX_URL: &str = "https://github.com/rust-lang/crates.io-index";

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

    #[serde(default)]
    auth_required: bool,

    #[serde(default)]
    html: ConfigV1Html,
}

impl ConfigV1 {
    const USER_DEFAULT_AUTH_REQUIRED: bool = false;

    fn normalize(mut self) -> ConfigV1 {
        ensure_last_segment_empty(&mut self.base_url);

        self
    }
}

fn ensure_last_segment_empty(url: &mut Url) {
    if let Ok(mut s) = url.path_segments_mut() {
        s.pop_if_empty().push("");
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConfigV1Html {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    suggested_registry_name: Option<String>,
}

impl ConfigV1Html {
    const USER_DEFAULT_ENABLED: bool = true;
    const USER_DEFAULT_SUGGESTED_REGISTRY_NAME: &'static str = "my-awesome-registry";

    fn suggested_registry_name(&self) -> &str {
        self.suggested_registry_name
            .as_deref()
            .unwrap_or(Self::USER_DEFAULT_SUGGESTED_REGISTRY_NAME)
    }
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
    use semver::{Version, VersionReq};
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;
    use url::Url;

    use crate::common::{CrateName, RustVersion};

    #[derive(Debug, Serialize, Deserialize)]
    pub struct Root {
        /// The name of the package.
        pub name: CrateName,

        /// The version of the package this row is describing.
        ///
        /// This must be a valid version number according to the
        /// Semantic Versioning 2.0.0 spec at https://semver.org/.
        pub vers: Version,

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
        ///   This is honored in Rust version 1.51 and newer.
        /// * 2: The addition of the `features2` field.
        ///   This is honored in Rust version 1.60 and newer.
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
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        pub features2: BTreeMap<String, Vec<String>>,

        /// The minimal supported Rust version
        ///
        /// This must be a valid version requirement without an operator (e.g. no `=`)
        #[serde(skip_serializing_if = "Option::is_none")]
        pub rust_version: Option<RustVersion>,
    }

    #[derive(Debug, Serialize, Deserialize)]
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
        pub req: VersionReq,

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

    #[derive(Debug, Serialize, Deserialize)]
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
    use semver::Version;
    use serde::{de::Error, Deserialize, Serialize};
    use snafu::prelude::*;
    use std::{
        borrow::Cow,
        fmt, ops,
        path::{Path, PathBuf},
        str::FromStr,
    };

    /// Contains only alphanumeric, `-`, or `_` characters.
    #[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
    pub struct CrateName(AsciiString);

    impl CrateName {
        pub fn as_str(&self) -> &str {
            self.0.as_str()
        }

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

    impl fmt::Display for CrateName {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.fmt(f)
        }
    }

    impl FromStr for CrateName {
        type Err = CrateNameError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            s.try_into()
        }
    }

    impl TryFrom<&str> for CrateName {
        type Error = CrateNameError;

        fn try_from(value: &str) -> Result<Self, Self::Error> {
            value.to_owned().try_into()
        }
    }

    impl TryFrom<String> for CrateName {
        type Error = CrateNameError;

        fn try_from(value: String) -> Result<Self, Self::Error> {
            AsciiString::from_ascii(value)
                .map_err(|e| e.ascii_error())?
                .try_into()
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

        #[snafu(transparent)]
        NotAscii { source: ascii::AsAsciiStrError },
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

    #[derive(Debug)]
    pub struct RustVersion(Version);

    impl FromStr for RustVersion {
        type Err = RustVersionError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            use rust_version_error::*;

            let v: Version = match s.parse() {
                Ok(v) => v,
                Err(e) => {
                    let version = [s, ".0"].concat();
                    match version.parse() {
                        Ok(v) => v,
                        Err(_) => return Err(e)?,
                    }
                }
            };

            ensure!(v.pre.is_empty(), PrereleaseSnafu);
            ensure!(v.build.is_empty(), BuildSnafu);

            Ok(Self(v))
        }
    }

    #[derive(Debug, Snafu)]
    #[snafu(module)]
    pub enum RustVersionError {
        #[snafu(transparent)]
        Semver { source: semver::Error },

        #[snafu(display("May not specify a prerelease version"))]
        Prerelease,

        #[snafu(display("May not specify a version with build metadata"))]
        Build,
    }

    impl From<RustVersion> for Version {
        fn from(value: RustVersion) -> Self {
            value.0
        }
    }

    impl<'de> serde::Deserialize<'de> for RustVersion {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            let version = Cow::<str>::deserialize(deserializer)?;
            version.parse().map_err(D::Error::custom)
        }
    }

    impl serde::Serialize for RustVersion {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            self.0.serialize(serializer)
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use registry_conformance::{Crate, ScratchSpace};

    fn default_config() -> ConfigV1 {
        ConfigV1 {
            base_url: "http://example.com".parse().unwrap(),
            auth_required: false,
            html: ConfigV1Html {
                enabled: false,
                suggested_registry_name: None,
            },
        }
    }

    #[tokio::test]
    async fn adding_duplicate_crate() {
        let global = Global::new().unwrap();
        let scratch = ScratchSpace::new().await.unwrap();

        let config = default_config();

        let r = Registry::initialize(config, scratch.registry()).unwrap();

        let c = Crate::new("duplicated", "1.0.0")
            .lib_rs(r#"pub const ID: u8 = 1;"#)
            .create_in(&scratch)
            .await
            .unwrap();
        let p = c.package().await.unwrap();

        r.add(&global, &p).unwrap();
        r.add(&global, &p).unwrap();

        let name = CrateName::try_from(c.name()).unwrap();
        let index_file_path = r.index_file_path_for(&name);
        let index_contents = fs::read_to_string(index_file_path).unwrap();

        assert_eq!(1, index_contents.lines().count());
    }

    #[tokio::test]
    async fn base_url_requires_trailing_slash() {
        let scratch = ScratchSpace::new().await.unwrap();

        let config = ConfigV1 {
            base_url: "http://example.com/path/to/index".parse().unwrap(),
            ..default_config()
        };

        let r = Registry::initialize(config, scratch.registry()).unwrap();

        let paths = [r.config_json_path(), r.margo_config_toml_path()];

        for path in paths {
            let contents = fs::read_to_string(&path).unwrap();

            assert!(
                contents.contains("/path/to/index/"),
                "{path} does not have the trailing slash:\n{contents}",
                path = path.display(),
            );
        }
    }

    #[tokio::test]
    async fn removing_a_crate_deletes_from_disk() {
        let global = Global::new().unwrap();
        let scratch = ScratchSpace::new().await.unwrap();

        let config = default_config();

        let r = Registry::initialize(config, scratch.registry()).unwrap();

        let name = "to-go-away";
        let version = "1.0.0";

        let c = Crate::new(name, version)
            .lib_rs(r#"pub const ID: u8 = 1;"#)
            .create_in(&scratch)
            .await
            .unwrap();
        let p = c.package().await.unwrap();

        let name = name.parse().unwrap();
        let version = version.parse().unwrap();
        let crate_path = r.crate_file_path_for(&name, &version);

        r.add(&global, p).unwrap();

        assert!(
            crate_path.exists(),
            "The crate file should be in the registry at {}",
            crate_path.display(),
        );

        r.remove(name, version).unwrap();

        assert!(
            !crate_path.exists(),
            "The crate file should not be in the registry at {}",
            crate_path.display(),
        );
    }
}
