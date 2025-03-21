/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::collections::HashMap;
use std::collections::HashSet;

use allocative::Allocative;
use anyhow::Context;
use buck2_core::cells::alias::NonEmptyCellAlias;
use buck2_core::cells::cell_root_path::CellRootPathBuf;
use buck2_core::cells::CellResolver;
use buck2_core::cells::CellsAggregator;
use buck2_core::env_helper::EnvHelper;
use buck2_core::fs::paths::abs_norm_path::AbsNormPath;
use buck2_core::fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_core::fs::paths::file_name::FileNameBuf;
use buck2_core::fs::paths::forward_rel_path::ForwardRelativePath;
use buck2_core::fs::paths::RelativePath;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use gazebo::prelude::*;
use once_cell::unsync::OnceCell;
use serde::Deserialize;
use serde::Serialize;

use crate::legacy_configs::path::BuckConfigFile;
use crate::legacy_configs::path::DEFAULT_BUCK_CONFIG_FILES;
use crate::legacy_configs::push_all_files_from_a_directory;
use crate::legacy_configs::BuckConfigParseOptions;
use crate::legacy_configs::CellResolutionState;
use crate::legacy_configs::ConfigParserFileOps;
use crate::legacy_configs::DefaultConfigParserFileOps;
use crate::legacy_configs::LegacyBuckConfig;
use crate::legacy_configs::LegacyBuckConfigs;
use crate::legacy_configs::LegacyConfigCmdArg;
use crate::legacy_configs::MainConfigFile;

#[derive(Debug, thiserror::Error)]
enum CellsError {
    #[error(
        "Repository root buckconfig must have `[repositories]` section with a pointer to itself \
        like `root = .` which defines the root cell name"
    )]
    MissingRootCellName,
}

/// Used for creating a CellResolver in a buckv1-compatible way based on values
/// in .buckconfig in each cell.
///
/// We'll traverse the structure of the `[repositories]` sections starting from
/// the root .buckconfig. All aliases found in the root config will also be
/// available in all other cells (v1 provides that same behavior).
///
/// We don't (currently) enforce that all aliases appear in the root config, but
/// unlike v1, our cells implementation works just fine if that isn't the case.
pub struct BuckConfigBasedCells {
    pub configs_by_name: LegacyBuckConfigs,
    pub cell_resolver: CellResolver,
    pub config_paths: HashSet<AbsNormPathBuf>,
}

impl BuckConfigBasedCells {
    /// Performs a parse of the root `.buckconfig` for the cell _only_ without following includes
    /// and without parsing any configs for any referenced cells. This means this function might return
    /// an empty mapping if the root `.buckconfig` does not contain the cell definitions.
    pub fn parse_immediate_config(project_fs: &ProjectRoot) -> anyhow::Result<ImmediateConfig> {
        Self::parse_immediate_config_with_file_ops(project_fs, &mut DefaultConfigParserFileOps {})
    }

    /// Private function with semantics of `parse_immediate_config` but usable for testing.
    pub(crate) fn parse_immediate_config_with_file_ops(
        project_fs: &ProjectRoot,
        file_ops: &mut dyn ConfigParserFileOps,
    ) -> anyhow::Result<ImmediateConfig> {
        let opts = BuckConfigParseOptions {
            follow_includes: false,
        };
        let cells = Self::parse_with_file_ops_and_options(
            project_fs,
            file_ops,
            &[],
            ProjectRelativePath::empty(),
            opts,
        )?;

        let root_config = cells
            .configs_by_name
            .get(cells.cell_resolver.root_cell())
            .context("No config for root cell")?;

        Ok(ImmediateConfig {
            cell_resolver: cells.cell_resolver,
            daemon_startup_config: DaemonStartupConfig::new(root_config)
                .context("Error loading daemon startup config")?,
        })
    }

    pub fn parse(project_fs: &ProjectRoot) -> anyhow::Result<Self> {
        Self::parse_with_file_ops(
            project_fs,
            &mut DefaultConfigParserFileOps {},
            &[],
            ProjectRelativePath::empty(),
        )
    }

    pub fn parse_with_config_args(
        project_fs: &ProjectRoot,
        config_args: &[LegacyConfigCmdArg],
        cwd: &ProjectRelativePath,
    ) -> anyhow::Result<Self> {
        Self::parse_with_file_ops(
            project_fs,
            &mut DefaultConfigParserFileOps {},
            config_args,
            cwd,
        )
    }

    pub fn parse_with_file_ops(
        project_fs: &ProjectRoot,
        file_ops: &mut dyn ConfigParserFileOps,
        config_args: &[LegacyConfigCmdArg],
        cwd: &ProjectRelativePath,
    ) -> anyhow::Result<Self> {
        let opts = BuckConfigParseOptions {
            follow_includes: true,
        };
        Self::parse_with_file_ops_and_options(project_fs, file_ops, config_args, cwd, opts)
    }

    fn parse_with_file_ops_and_options(
        project_fs: &ProjectRoot,
        file_ops: &mut dyn ConfigParserFileOps,
        config_args: &[LegacyConfigCmdArg],
        cwd: &ProjectRelativePath,
        options: BuckConfigParseOptions,
    ) -> anyhow::Result<Self> {
        // Tracing file ops to record config file accesses on command invocation.
        struct TracingFileOps<'a> {
            inner: &'a mut dyn ConfigParserFileOps,
            trace: HashSet<AbsNormPathBuf>,
        }

        impl ConfigParserFileOps for TracingFileOps<'_> {
            fn read_file_lines(
                &mut self,
                path: &AbsNormPath,
            ) -> anyhow::Result<Box<dyn Iterator<Item = Result<String, std::io::Error>>>>
            {
                self.trace.insert(path.to_buf());
                self.inner.read_file_lines(path)
            }

            fn file_exists(&self, path: &AbsNormPath) -> bool {
                self.inner.file_exists(path)
            }

            fn file_id(&self, path: &AbsNormPath) -> String {
                self.inner.file_id(path)
            }
        }

        let mut file_ops = TracingFileOps {
            inner: file_ops,
            trace: Default::default(),
        };

        let mut buckconfigs = HashMap::new();
        let mut work = vec![CellRootPathBuf::new(ProjectRelativePathBuf::try_from(
            "".to_owned(),
        )?)];
        let mut cells_aggregator = CellsAggregator::new();
        let mut root_aliases = HashMap::new();

        // By definition, cell resolution should be happening against the cell mapping defined
        // by the .buckconfig of the project root.
        let cell_resolution = CellResolutionState {
            project_filesystem: project_fs,
            cell_resolver: OnceCell::new(),
            cwd: &project_fs.resolve(cwd),
        };
        // NOTE: This will _not_ perform IO unless it needs to.
        let processed_config_args = LegacyBuckConfig::process_config_args(
            config_args,
            Some(&cell_resolution),
            &mut file_ops,
        )?;

        static SKIP_DEFAULT_EXTERNAL_CONFIG: EnvHelper<bool> =
            EnvHelper::<bool>::new("BUCK2_TEST_SKIP_DEFAULT_EXTERNAL_CONFIG");

        static EXTRA_EXTERNAL_CONFIG: EnvHelper<String> =
            EnvHelper::<String>::new("BUCK2_TEST_EXTRA_EXTERNAL_CONFIG");

        let skip_default_external_config = SKIP_DEFAULT_EXTERNAL_CONFIG
            .get()?
            .copied()
            .unwrap_or_default();

        while let Some(path) = work.pop() {
            if buckconfigs.contains_key(&path) {
                continue;
            }

            let mut buckconfig_paths: Vec<MainConfigFile> = Vec::new();

            for buckconfig in DEFAULT_BUCK_CONFIG_FILES {
                if skip_default_external_config && buckconfig.is_external() {
                    continue;
                }

                match buckconfig {
                    BuckConfigFile::ProjectRelativeFile(file) => {
                        let buckconfig_path = ForwardRelativePath::new(file)?;
                        buckconfig_paths.push(MainConfigFile {
                            path: project_fs
                                .resolve(&path.project_relative_path().join(buckconfig_path)),
                            owned_by_project: true,
                        });
                    }

                    BuckConfigFile::ProjectRelativeFolder(folder) => {
                        let buckconfig_folder_path = ForwardRelativePath::new(folder)?;
                        let buckconfig_folder_abs_path = project_fs
                            .resolve(&path.project_relative_path().join(buckconfig_folder_path));
                        push_all_files_from_a_directory(
                            &mut buckconfig_paths,
                            &buckconfig_folder_abs_path,
                            true,
                        )?;
                    }
                    BuckConfigFile::UserFile(file) => {
                        let home_dir = dirs::home_dir();
                        if let Some(home_dir_path) = home_dir {
                            let buckconfig_path = ForwardRelativePath::new(file)?;
                            buckconfig_paths.push(MainConfigFile {
                                path: AbsNormPath::new(&home_dir_path)?
                                    .join_normalized(buckconfig_path)?,
                                owned_by_project: false,
                            });
                        }
                    }
                    BuckConfigFile::UserFolder(folder) => {
                        let home_dir = dirs::home_dir();
                        if let Some(home_dir_path) = home_dir {
                            let buckconfig_path = ForwardRelativePath::new(folder)?;
                            let buckconfig_folder_abs_path = AbsNormPath::new(&home_dir_path)?
                                .join_normalized(buckconfig_path)?;
                            push_all_files_from_a_directory(
                                &mut buckconfig_paths,
                                &buckconfig_folder_abs_path,
                                false,
                            )?;
                        }
                    }
                    BuckConfigFile::GlobalFile(file) => {
                        buckconfig_paths.push(MainConfigFile {
                            path: AbsNormPathBuf::from(String::from(*file))?,
                            owned_by_project: false,
                        });
                    }
                    BuckConfigFile::GlobalFolder(folder) => {
                        let buckconfig_folder_abs_path =
                            AbsNormPathBuf::from(String::from(*folder))?;
                        push_all_files_from_a_directory(
                            &mut buckconfig_paths,
                            &buckconfig_folder_abs_path,
                            false,
                        )?;
                    }
                }
            }

            if let Some(f) = EXTRA_EXTERNAL_CONFIG.get()? {
                buckconfig_paths.push(MainConfigFile {
                    path: AbsNormPathBuf::from(f.to_owned())?,
                    owned_by_project: false,
                });
            }

            let existing_configs: Vec<MainConfigFile> = buckconfig_paths
                .into_iter()
                .filter(|main_config_file| file_ops.file_exists(&main_config_file.path))
                .collect();

            // Must contains a buckconfig owned by project, otherwise no cell can be found.
            // This also check if existing_configs is empty
            let has_project_owned_config = existing_configs
                .iter()
                .any(|main_config_file| main_config_file.owned_by_project);

            if !has_project_owned_config {
                buckconfigs.insert(path, LegacyBuckConfig::empty());
                continue;
            };

            let config = LegacyBuckConfig::parse_with_file_ops_with_includes(
                existing_configs.as_slice(),
                &mut file_ops,
                &processed_config_args,
                options.follow_includes,
            )?;

            let is_root = path.is_repo_root();

            let repositories = config.get_section("repositories");
            if let Some(repositories) = repositories {
                let mut seen_dot = false;
                for (alias, alias_path) in repositories.iter() {
                    if alias_path.as_str() == "." {
                        seen_dot = true;
                    }

                    let alias_path = CellRootPathBuf::new(path
                        .join_normalized(RelativePath::new(alias_path.as_str()))
                        .with_context(|| {
                            format!(
                                "expected alias path to be a relative path, but found `{}` for `{}` in buckconfig `{}`",
                                alias_path.as_str(),
                                alias,
                                path
                            )
                        })?);
                    let alias = NonEmptyCellAlias::new(alias.to_owned())?;
                    if is_root {
                        root_aliases.insert(alias.clone(), alias_path.clone());
                    }
                    cells_aggregator.add_cell_entry(path.clone(), alias, alias_path.clone())?;
                    work.push(alias_path);
                }

                if is_root && !seen_dot {
                    return Err(CellsError::MissingRootCellName.into());
                }
            } else if is_root {
                return Err(CellsError::MissingRootCellName.into());
            }

            if let Some(aliases) = config.get_section("repository_aliases") {
                for (alias, destination) in aliases.iter() {
                    let alias = NonEmptyCellAlias::new(alias.to_owned())?;
                    let destination = NonEmptyCellAlias::new(destination.as_str().to_owned())?;
                    let alias_path = cells_aggregator.add_cell_alias(
                        path.clone(),
                        alias.clone(),
                        destination,
                    )?;
                    if path.as_str() == "" {
                        root_aliases.insert(alias, alias_path.clone());
                    }
                }
            }

            if let Some(buildfiles) = Self::parse_buildfile_name(&config)? {
                cells_aggregator.set_buildfiles(path.clone(), buildfiles);
            }

            buckconfigs.insert(path, config);
        }

        for cell_path in buckconfigs.keys() {
            for (alias, alias_path) in &root_aliases {
                cells_aggregator.add_cell_entry(
                    cell_path.clone(),
                    alias.clone(),
                    alias_path.clone(),
                )?;
            }
        }

        let cell_resolver = cells_aggregator.make_cell_resolver()?;
        let configs_by_name = buckconfigs
            .into_iter()
            .map(|(path, config)| Ok((cell_resolver.find(path.project_relative_path())?, config)))
            .collect::<anyhow::Result<_>>()?;

        Ok(Self {
            configs_by_name: LegacyBuckConfigs::new(configs_by_name),
            cell_resolver,
            config_paths: file_ops.trace,
        })
    }

    /// Deal with the `buildfile.name` key (and `name_v2`)
    fn parse_buildfile_name(config: &LegacyBuckConfig) -> anyhow::Result<Option<Vec<FileNameBuf>>> {
        // For buck2, we support a slightly different mechanism for setting the buildfile to
        // assist with easier migration from v1 to v2.
        // First, we check the key `buildfile.name_v2`, if this is provided, we use it.
        // Second, if that wasn't provided, we will use `buildfile.name` like buck1 does,
        // but for every entry `FOO` we will insert a preceding `FOO.v2`.
        // If neither of those is provided, we will use the default of `["BUCK.v2", "BUCK"]`.
        // This scheme provides a natural progression to buckv2, with the ability to use separate
        // buildfiles for the two where necessary.
        if let Some(buildfiles_value) = config.parse_list::<String>("buildfile", "name_v2")? {
            Ok(Some(buildfiles_value.into_try_map(FileNameBuf::try_from)?))
        } else if let Some(buildfiles_value) = config.parse_list::<String>("buildfile", "name")? {
            let mut buildfiles = Vec::new();
            for buildfile in buildfiles_value {
                buildfiles.push(FileNameBuf::try_from(format!("{}.v2", buildfile))?);
                buildfiles.push(FileNameBuf::try_from(buildfile)?);
            }
            Ok(Some(buildfiles))
        } else {
            Ok(None)
        }
    }
}

/// Limited view of the root config. This does not follow includes.
pub struct ImmediateConfig {
    pub cell_resolver: CellResolver,
    pub daemon_startup_config: DaemonStartupConfig,
}

/// Configurations that are used at startup by the daemon. Those are actually read by the client,
/// and passed on to the daemon.
///
/// The fields here are often raw String we get from the buckconfig, the daemon will do
/// deserialization once it receives them. That said, this is not a requirement.
///
/// Backwards compatibility on Serialize / Deserialize is not required: if the client cannot read
/// the DaemonStartupConfig provided by the daemon when it tries to connect, it will reject that
/// daemon and restart (and in fact it will probably not get that far since a version check is done
/// before parsing DaemonStartupConfig).
#[derive(Allocative, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonStartupConfig {
    pub daemon_buster: Option<String>,
    pub digest_algorithms: Option<String>,
    pub source_digest_algorithm: Option<String>,
    pub allow_vpnless: bool,
    pub allow_vpnless_for_logging: bool,
    pub paranoid: bool,
    pub use_tonic_rt: Option<String>,
}

impl DaemonStartupConfig {
    fn new(config: &LegacyBuckConfig) -> anyhow::Result<Self> {
        // NOTE: We purposefully still evaluate the config here when the env var is set, to check
        // it's right.
        static PARANOID: EnvHelper<bool> = EnvHelper::new("BUCK_PARANOID");
        let paranoid = PARANOID
            .get_copied()?
            .or(config.parse("buck2", "paranoid")?)
            .unwrap_or_default();

        // Intepreted client side because we need the value here.
        let allow_vpnless = config.parse("buck2", "allow_vpnless")?.unwrap_or_default();
        let allow_vpnless_for_logging = config
            .parse("buck2", "allow_vpnless_for_logging")?
            .unwrap_or(allow_vpnless);

        Ok(Self {
            daemon_buster: config.get("buck2", "daemon_buster").map(ToOwned::to_owned),
            digest_algorithms: config
                .get("buck2", "digest_algorithms")
                .map(ToOwned::to_owned),
            source_digest_algorithm: config
                .get("buck2", "source_digest_algorithm")
                .map(ToOwned::to_owned),
            allow_vpnless,
            allow_vpnless_for_logging,
            paranoid,
            use_tonic_rt: config.get("buck2", "use_tonic_rt").map(ToOwned::to_owned),
        })
    }

    pub fn serialize(&self) -> String {
        // This only contains String, so it'll successfully serialize to JSON.
        serde_json::to_string(&self).unwrap()
    }

    pub fn deserialize(s: &str) -> anyhow::Result<Self> {
        serde_json::from_str::<Self>(s).context("Error deserializing DaemonStartupConfig")
    }

    pub fn testing_empty() -> Self {
        Self {
            daemon_buster: None,
            digest_algorithms: None,
            source_digest_algorithm: None,
            allow_vpnless: false,
            allow_vpnless_for_logging: false,
            paranoid: false,
            use_tonic_rt: None,
        }
    }
}

#[cfg(test)]
mod tests {

    use buck2_core::cells::name::CellName;
    use buck2_core::fs::paths::abs_norm_path::AbsNormPathBuf;
    use buck2_core::fs::project::ProjectRoot;
    use buck2_core::fs::project_rel_path::ProjectRelativePath;
    use gazebo::prelude::*;
    use indoc::indoc;

    use crate::legacy_configs::cells::BuckConfigBasedCells;
    use crate::legacy_configs::testing::TestConfigParserFileOps;
    use crate::legacy_configs::tests::assert_config_value;
    use crate::legacy_configs::LegacyConfigCmdArg;

    fn create_project_filesystem() -> ProjectRoot {
        #[cfg(not(windows))]
        let root_path = "/".to_owned();
        #[cfg(windows)]
        let root_path = "C:/".to_owned();
        ProjectRoot::new_unchecked(AbsNormPathBuf::try_from(root_path).unwrap())
    }

    #[test]
    fn test_cells() -> anyhow::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[
            (
                "/.buckconfig",
                indoc!(
                    r#"
                            [repositories]
                                root = .
                                other = other/
                                other_alias = other/
                                third_party = third_party/
                        "#
                ),
            ),
            (
                "/other/.buckconfig",
                indoc!(
                    r#"
                            [repositories]
                                root = ..
                                other = .
                                third_party = ../third_party/
                            [buildfile]
                                name = TARGETS
                        "#
                ),
            ),
            (
                "/third_party/.buckconfig",
                indoc!(
                    r#"
                            [repositories]
                                third_party = .
                            [buildfile]
                                name_v2 = OKAY
                                name = OKAY_v1
                        "#
                ),
            ),
        ])?;

        let project_fs = create_project_filesystem();
        let cells = BuckConfigBasedCells::parse_with_file_ops(
            &project_fs,
            &mut file_ops,
            &[],
            ProjectRelativePath::empty(),
        )?;

        let resolver = &cells.cell_resolver;

        let root_instance = resolver.get(CellName::testing_new("root"))?;
        let other_instance = resolver.get(CellName::testing_new("other"))?;
        let tp_instance = resolver.get(CellName::testing_new("third_party"))?;

        assert_eq!(
            vec!["BUCK.v2", "BUCK"],
            root_instance.buildfiles().map(|n| n.as_str())
        );
        assert_eq!(
            vec!["TARGETS.v2", "TARGETS"],
            other_instance.buildfiles().map(|n| n.as_str())
        );
        assert_eq!(vec!["OKAY"], tp_instance.buildfiles().map(|n| n.as_str()));

        assert_eq!(
            "other",
            root_instance
                .cell_alias_resolver()
                .resolve("other_alias")?
                .as_str()
        );

        assert_eq!(
            "other",
            tp_instance
                .cell_alias_resolver()
                .resolve("other_alias")?
                .as_str()
        );

        assert_eq!("", root_instance.path().as_str());
        assert_eq!("other", other_instance.path().as_str());
        assert_eq!("third_party", tp_instance.path().as_str());

        Ok(())
    }

    #[test]
    fn test_multi_cell_with_config_file() -> anyhow::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[
            (
                "/.buckconfig",
                indoc!(
                    r#"
                            [repositories]
                                root = .
                                other = other/
                                other_alias = other/
                                third_party = third_party/
                        "#
                ),
            ),
            (
                "/other/.buckconfig",
                indoc!(
                    r#"
                            [repositories]
                                root = ..
                                other = .
                                third_party = ../third_party/
                            [buildfile]
                                name = TARGETS
                        "#
                ),
            ),
            (
                "/third_party/.buckconfig",
                indoc!(
                    r#"
                            [repositories]
                                third_party = .
                            [buildfile]
                                name_v2 = OKAY
                                name = OKAY_v1
                        "#
                ),
            ),
            (
                "/other/cli-conf",
                indoc!(
                    r#"
                            [foo]
                                bar = blah
                        "#
                ),
            ),
        ])?;

        let project_fs = create_project_filesystem();
        #[cfg(not(windows))]
        let file_arg = "/other/cli-conf";
        #[cfg(windows)]
        let file_arg = "C:/other/cli-conf";
        let cells = BuckConfigBasedCells::parse_with_file_ops(
            &project_fs,
            &mut file_ops,
            &[LegacyConfigCmdArg::file(file_arg)?],
            ProjectRelativePath::empty(),
        )?;

        let configs = &cells.configs_by_name;
        let root_config = configs.get(CellName::testing_new("root")).unwrap();
        let other_config = configs.get(CellName::testing_new("other")).unwrap();
        let tp_config = configs.get(CellName::testing_new("third_party")).unwrap();

        assert_eq!(root_config.get("foo", "bar"), Some("blah"));
        assert_eq!(other_config.get("foo", "bar"), Some("blah"));
        assert_eq!(tp_config.get("foo", "bar"), Some("blah"));

        Ok(())
    }

    #[test]
    fn test_multi_cell_no_repositories_in_non_root_cell() -> anyhow::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[
            (
                "/.buckconfig",
                indoc!(
                    r#"
                            [repositories]
                                root = .
                                other = other/
                        "#
                ),
            ),
            (
                "/other/.buckconfig",
                indoc!(
                    r#"
                            [foo]
                                bar = baz
                        "#
                ),
            ),
        ])?;

        let project_fs = create_project_filesystem();
        let cells = BuckConfigBasedCells::parse_with_file_ops(
            &project_fs,
            &mut file_ops,
            &[],
            ProjectRelativePath::empty(),
        )?;

        let configs = &cells.configs_by_name;

        let other_config = configs.get(CellName::testing_new("other")).unwrap();

        assert_eq!(other_config.get("foo", "bar"), Some("baz"));

        Ok(())
    }

    #[test]
    fn test_multi_cell_with_cell_relative() -> anyhow::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[
            (
                "/.buckconfig",
                indoc!(
                    r#"
                            [repositories]
                                root = .
                                other = other/
                        "#
                ),
            ),
            (
                "/global-conf",
                indoc!(
                    r#"
                            [apple]
                                test_tool = xctool
                        "#
                ),
            ),
            (
                "/other/.buckconfig",
                indoc!(
                    r#"
                            [repositories]
                                root = ..
                                other = .
                            [buildfile]
                                name = TARGETS
                        "#
                ),
            ),
            (
                "/other/app-conf",
                indoc!(
                    r#"
                            [apple]
                                ide = Xcode
                        "#
                ),
            ),
        ])?;

        let project_fs = create_project_filesystem();
        let cells = BuckConfigBasedCells::parse_with_file_ops(
            &project_fs,
            &mut file_ops,
            &[
                LegacyConfigCmdArg::file("other//app-conf")?,
                LegacyConfigCmdArg::file("//global-conf")?,
            ],
            ProjectRelativePath::empty(),
        )?;

        let configs = &cells.configs_by_name;
        let other_config = configs.get(CellName::testing_new("other")).unwrap();

        assert_eq!(other_config.get("apple", "ide"), Some("Xcode"));
        assert_eq!(other_config.get("apple", "test_tool"), Some("xctool"));

        Ok(())
    }

    #[test]
    fn test_local_config_file_overwrite_config_file() -> anyhow::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[
            (
                "/.buckconfig",
                indoc!(
                    r#"
                            [repositories]
                                root = .
                            [apple]
                                key = value1
                                key2 = value2
                        "#
                ),
            ),
            (
                "/.buckconfig.local",
                indoc!(
                    r#"
                            [orange]
                                key = value3
                            [apple]
                                key2 = value5
                                key3 = value4
                        "#
                ),
            ),
        ])?;

        let project_fs = create_project_filesystem();
        let cells = BuckConfigBasedCells::parse_with_file_ops(
            &project_fs,
            &mut file_ops,
            &[],
            ProjectRelativePath::empty(),
        )?;

        let configs = &cells.configs_by_name;
        let config = configs.get(CellName::testing_new("root")).unwrap();
        // No local override
        assert_config_value(config, "apple", "key", "value1");
        // local override to new value
        assert_config_value(config, "apple", "key2", "value5");
        // local override new field
        assert_config_value(config, "apple", "key3", "value4");
        // local override new section
        assert_config_value(config, "orange", "key", "value3");

        Ok(())
    }

    #[test]
    fn test_multi_cell_local_config_file_overwrite_config_file() -> anyhow::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[
            (
                "/.buckconfig",
                indoc!(
                    r#"
                            [repositories]
                                root = .
                                other = other/
                            [apple]
                                key = value1
                                key2 = value2
                        "#
                ),
            ),
            (
                "/.buckconfig.local",
                indoc!(
                    r#"
                            [orange]
                                key = value3
                            [apple]
                                key2 = value5
                                key3 = value4
                        "#
                ),
            ),
            (
                "/other/.buckconfig",
                indoc!(
                    r#"
                            [repositories]
                                root = ..
                                other = .
                            [apple]
                                key = othervalue1
                                key2 = othervalue2
                        "#
                ),
            ),
            (
                "/other/.buckconfig.local",
                indoc!(
                    r#"
                            [orange]
                                key = othervalue3
                            [apple]
                                key2 = othervalue5
                                key3 = othervalue4
                        "#
                ),
            ),
        ])?;

        let project_fs = create_project_filesystem();
        let cells = BuckConfigBasedCells::parse_with_file_ops(
            &project_fs,
            &mut file_ops,
            &[],
            ProjectRelativePath::empty(),
        )?;

        let configs = &cells.configs_by_name;
        let root_config = configs.get(CellName::testing_new("root")).unwrap();
        let other_config = configs.get(CellName::testing_new("other")).unwrap();

        // No local override
        assert_config_value(root_config, "apple", "key", "value1");
        // local override to new value
        assert_config_value(root_config, "apple", "key2", "value5");
        // local override new field
        assert_config_value(root_config, "apple", "key3", "value4");
        // local override new section
        assert_config_value(root_config, "orange", "key", "value3");

        // No local override
        assert_config_value(other_config, "apple", "key", "othervalue1");
        // local override to new value
        assert_config_value(other_config, "apple", "key2", "othervalue5");
        // local override new field
        assert_config_value(other_config, "apple", "key3", "othervalue4");
        // local override new section
        assert_config_value(other_config, "orange", "key", "othervalue3");

        Ok(())
    }
}
