use super::{
    directories::RimDir,
    parser::{
        cargo_config::CargoConfig,
        fingerprint::{InstallationRecord, ToolRecord},
        toolset_manifest::{ToolInfo, ToolsetManifest},
        TomlParser,
    },
    rustup::ToolchainInstaller,
    tools::Tool,
    CARGO_HOME, RUSTUP_DIST_SERVER, RUSTUP_HOME, RUSTUP_UPDATE_ROOT,
};
use crate::{
    core::os::add_to_path,
    toolset_manifest::{Proxy, ToolMap},
    utils::{self, Extractable, MultiThreadProgress},
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use tempfile::TempDir;
use url::Url;

macro_rules! declare_unfallible_url {
    ($($name:ident($global:ident) -> $val:literal);+) => {
        $(
            static $global: std::sync::OnceLock<url::Url> = std::sync::OnceLock::new();
            pub(crate) fn $name() -> &'static url::Url {
                $global.get_or_init(|| {
                    url::Url::parse($val).expect(
                        &format!("Internal Error: static variable '{}' cannot be parse to URL", $val)
                    )
                })
            }
        )*
    };
}

declare_unfallible_url!(
    default_rustup_dist_server(DEFAULT_RUSTUP_DIST_SERVER) -> "https://mirrors.tuna.tsinghua.edu.cn/rustup";
    default_rustup_update_root(DEFAULT_RUSTUP_UPDATE_ROOT) -> "https://mirrors.tuna.tsinghua.edu.cn/rustup/rustup"
);

pub(crate) const DEFAULT_CARGO_REGISTRY: (&str, &str) =
    ("rsproxy", "sparse+https://rsproxy.cn/index/");

/// Contains definition of installation steps, including pre-install configs.
///
/// Make sure to always call `init()` as it creates essential folders to
/// hold the installation files.
pub trait EnvConfig {
    /// Configure environment variables.
    ///
    /// This will set persistent environment variables including
    /// `RUSTUP_DIST_SERVER`, `RUSTUP_UPDATE_ROOT`, `CARGO_HOME`, `RUSTUP_HOME`, etc.
    fn config_env_vars(&self, manifest: &ToolsetManifest) -> Result<()>;
}

#[derive(Debug, Deserialize, Serialize)]
pub struct InstallConfiguration {
    pub cargo_registry: Option<(String, String)>,
    /// Path to install everything.
    ///
    /// Note that this folder will includes `.cargo` and `.rustup` folders as well.
    /// And the default location will be `$HOME` directory (`%USERPROFILE%` on windows).
    /// So, even if the user didn't specify any install path, a pair of env vars will still
    /// be written (CARGO_HOME and RUSTUP_HOME), as they will be located in a sub folder of `$HOME`,
    /// which is [`installer_home`](utils::installer_home).
    pub install_dir: PathBuf,
    pub rustup_dist_server: Url,
    pub rustup_update_root: Url,
    /// Indicates whether `cargo` was already installed, useful when installing third-party tools.
    pub cargo_is_installed: bool,
    install_record: InstallationRecord,
}

impl RimDir for InstallConfiguration {
    fn install_dir(&self) -> &Path {
        self.install_dir.as_path()
    }
}

impl InstallConfiguration {
    /// Creating install diretory and other preperations related to filesystem.
    ///
    /// If `lite` is set to true, this won't make modifications on environment, and
    /// won't write manager binary as well.
    pub fn init(install_dir: &Path, lite: bool) -> Result<Self> {
        if install_dir.parent().is_none() {
            bail!("unable to install in root directory");
        }
        // Create a new folder to hold installation
        utils::ensure_dir(install_dir)?;

        if !lite {
            // Create a copy of this binary
            let self_exe = std::env::current_exe()?;
            // promote this installer to manager
            let manager_name = format!("{}-manager{}", t!("vendor_en"), utils::EXE_EXT);

            // Add this manager to the `PATH` environment
            let manager_exe = install_dir.join(manager_name);
            utils::copy_as(self_exe, &manager_exe)?;
            add_to_path(install_dir)?;

            // Create a copy of the baked manifest which is later used for component management.
            let manifest_out_path = install_dir.join("toolset-manifest.toml");
            let baked_manfest = include_str!("../../resources/toolset_manifest.toml");
            utils::write_file(manifest_out_path, baked_manfest, false)?;

            #[cfg(windows)]
            // Create registry entry to add this program into "installed programs".
            super::os::windows::do_add_to_programs(&manager_exe)?;
        }

        Ok(Self {
            install_dir: install_dir.to_path_buf(),
            install_record: InstallationRecord::load(install_dir)?,
            cargo_registry: Some(DEFAULT_CARGO_REGISTRY)
                .map(|(n, v)| (n.to_string(), v.to_string())),
            rustup_dist_server: default_rustup_dist_server().clone(),
            rustup_update_root: default_rustup_update_root().clone(),
            cargo_is_installed: false,
        })
    }

    pub fn cargo_registry<N, V>(mut self, name: N, value: V) -> Self
    where
        N: ToString,
        V: ToString,
    {
        self.cargo_registry = Some((name.to_string(), value.to_string()));
        self
    }

    pub fn rustup_dist_server(mut self, url: Url) -> Self {
        self.rustup_dist_server = url;
        self
    }

    pub fn rustup_update_root(mut self, url: Url) -> Self {
        self.rustup_update_root = url;
        self
    }

    pub(crate) fn env_vars(
        &self,
        manifest: &ToolsetManifest,
    ) -> Result<HashMap<&'static str, String>> {
        let cargo_home = self
            .cargo_home()
            .to_str()
            .map(ToOwned::to_owned)
            .context("`install-dir` cannot contains invalid unicodes")?;
        // This `unwrap` is safe here because we've already make sure the `install_dir`'s path can be
        // converted to string with the `cargo_home` variable.
        let rustup_home = self.rustup_home().to_str().unwrap().to_string();

        let mut env_vars = HashMap::from([
            (RUSTUP_DIST_SERVER, self.rustup_dist_server.to_string()),
            (RUSTUP_UPDATE_ROOT, self.rustup_update_root.to_string()),
            (CARGO_HOME, cargo_home),
            (RUSTUP_HOME, rustup_home),
        ]);

        // Add proxy settings if has
        if let Some(proxy) = &manifest.proxy {
            if let Some(url) = &proxy.http {
                env_vars.insert("http_proxy", url.to_string());
            }
            if let Some(url) = &proxy.https {
                env_vars.insert("https_proxy", url.to_string());
            }
            if let Some(s) = &proxy.no_proxy {
                env_vars.insert("no_proxy", s.to_string());
            }
        }

        Ok(env_vars)
    }

    pub fn install_tools_with_progress(
        &mut self,
        manifest: &ToolsetManifest,
        tools: &ToolMap,
        mt_prog: &mut MultiThreadProgress,
    ) -> Result<()> {
        // Ignore tools that need to be installed using `cargo install`
        let to_install = tools
            .into_iter()
            .filter(|(_, t)| !t.is_cargo_tool())
            .collect::<Vec<_>>();
        let sub_progress_delta = if to_install.is_empty() {
            return mt_prog.send_progress();
        } else {
            mt_prog.val / to_install.len()
        };

        for (name, tool) in to_install {
            println!("{}", t!("installing_tool_info", name = name));
            self.install_tool(name, tool, manifest.proxy.as_ref(), mt_prog)?;

            mt_prog.update_progress(sub_progress_delta)?;
        }

        self.install_record.write()?;

        Ok(())
    }

    pub fn install_rust_with_progress(
        &mut self,
        manifest: &ToolsetManifest,
        optional_components: &[String],
        mt_prog: &mut MultiThreadProgress,
    ) -> Result<()> {
        println!("{}", t!("installing_toolchain_info"));

        ToolchainInstaller::init().install(self, manifest, optional_components)?;
        add_to_path(self.cargo_bin())?;
        self.cargo_is_installed = true;

        // Add the rust info to the fingerprint.
        self.install_record
            .add_rust_record(manifest.rust_version(), optional_components);
        // record meta info
        // TODO(?): Maybe this should be moved as a separate step?
        self.install_record
            .clone_toolkit_meta_from_manifest(manifest);
        // write changes
        self.install_record.write()?;

        mt_prog.send_progress()
    }

    pub fn cargo_install_with_progress(
        &mut self,
        tools: &ToolMap,
        mt_prog: &mut MultiThreadProgress,
    ) -> Result<()> {
        let to_install = tools
            .into_iter()
            .filter(|(_, t)| t.is_cargo_tool())
            .collect::<Vec<_>>();
        let sub_progress_delta = if to_install.is_empty() {
            return mt_prog.send_progress();
        } else {
            mt_prog.val / to_install.len()
        };

        for (name, tool) in to_install {
            println!("{}", t!("installing_via_cargo_info", name = name));
            self.install_tool(name, tool, None, mt_prog)?;

            mt_prog.update_progress(sub_progress_delta)?;
        }

        self.install_record.write()?;

        Ok(())
    }

    // TODO: Write version info after installing each tool,
    // which is later used for updating.
    fn install_tool(
        &mut self,
        name: &str,
        tool: &ToolInfo,
        proxy: Option<&Proxy>,
        mt_prog: &mut MultiThreadProgress,
    ) -> Result<()> {
        let record = match tool {
            ToolInfo::PlainVersion(version) | ToolInfo::DetailedVersion { ver: version, .. } => {
                Tool::cargo_tool(name, Some(vec![name, "--version", version]))
                    .install(self, mt_prog)?
            }
            ToolInfo::Git {
                git,
                branch,
                tag,
                rev,
                ..
            } => {
                let mut args = vec!["--git", git.as_str()];
                if let Some(s) = &branch {
                    args.extend(["--branch", s]);
                }
                if let Some(s) = &tag {
                    args.extend(["--tag", s]);
                }
                if let Some(s) = &rev {
                    args.extend(["--rev", s]);
                }

                Tool::cargo_tool(name, Some(args)).install(self, mt_prog)?
            }
            ToolInfo::Path { path, .. } => self.try_install_from_path(name, path, mt_prog)?,
            // TODO: Have a dedicated download folder, do not use temp dir to store downloaded artifacts,
            // so then we can have the `resume download` feature.
            ToolInfo::Url { url, .. } => {
                let temp_dir = self.create_temp_dir("download")?;
                let downloaded_file_name = url
                    .path_segments()
                    .ok_or_else(|| anyhow!("unsupported url format '{url}'"))?
                    .last()
                    // Sadly, a path segment could be empty string, so we need to filter that out
                    .filter(|seg| !seg.is_empty())
                    .ok_or_else(|| anyhow!("'{url}' doesn't appear to be a downloadable file"))?;
                let dest = temp_dir.path().join(downloaded_file_name);
                utils::download(name, url, &dest, proxy)?;

                self.try_install_from_path(name, &dest, mt_prog)?
            }
        };

        self.install_record.add_tool_record(name, record);

        Ok(())
    }

    fn try_install_from_path(
        &self,
        name: &str,
        path: &Path,
        mt_prog: &mut MultiThreadProgress,
    ) -> Result<ToolRecord> {
        if !path.exists() {
            bail!(
                "unable to install '{name}' because the path to it's installer '{}' does not exist.",
                path.display()
            );
        }

        let temp_dir = self.create_temp_dir(name)?;
        let tool_installer_path = extract_or_copy_to(path, temp_dir.path())?;
        let tool_installer = Tool::from_path(name, &tool_installer_path)
            .with_context(|| format!("no install method for tool '{name}'"))?;
        tool_installer.install(self, mt_prog)
    }

    /// Configuration options for `cargo`.
    ///
    /// This will write a `config.toml` file to `CARGO_HOME`.
    pub fn config_cargo(&self) -> Result<()> {
        let mut config = CargoConfig::new();
        if let Some((name, url)) = &self.cargo_registry {
            config.add_source(name, url, true);
        }

        let config_toml = config.to_toml()?;
        if !config_toml.trim().is_empty() {
            let config_path = self.cargo_home().join("config.toml");
            utils::write_file(config_path, &config_toml, false)?;
        }

        Ok(())
    }

    /// Creates a temporary directory under `install_dir/temp`, with a certain prefix.
    pub(crate) fn create_temp_dir(&self, prefix: &str) -> Result<TempDir> {
        let root = self.temp_root();

        tempfile::Builder::new()
            .prefix(&format!("{prefix}_"))
            .tempdir_in(root)
            .with_context(|| format!("unable to create temp directory under '{}'", root.display()))
    }
}

pub fn default_install_dir() -> PathBuf {
    utils::home_dir().join(&*t!("vendor_en"))
}

/// Perform extraction or copy action base on the given path.
///
/// If `maybe_file` is a path to compressed file, this will try to extract it to `dest`;
/// otherwise this will copy that file into dest.
fn extract_or_copy_to(maybe_file: &Path, dest: &Path) -> Result<PathBuf> {
    if let Ok(mut extractable) = Extractable::load(maybe_file) {
        extractable.extract_to(dest)?;
        Ok(dest.to_path_buf())
    } else {
        utils::copy_into(maybe_file, dest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint;

    #[test]
    fn declare_unfallible_url_macro() {
        let default_dist_server = default_rustup_dist_server();
        let default_update_root = default_rustup_update_root();

        assert_eq!(
            default_dist_server.as_str(),
            "https://mirrors.tuna.tsinghua.edu.cn/rustup"
        );
        assert_eq!(
            default_update_root.as_str(),
            "https://mirrors.tuna.tsinghua.edu.cn/rustup/rustup"
        );
    }

    #[test]
    fn init_install_config() {
        let mut cache_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        cache_dir.push("tests");
        cache_dir.push("cache");

        std::fs::create_dir_all(&cache_dir).unwrap();

        let install_root = tempfile::Builder::new().tempdir_in(&cache_dir).unwrap();
        let config = InstallConfiguration::init(install_root.path(), true).unwrap();

        assert!(config.install_record.name.is_none());
        assert!(install_root.path().join(fingerprint::FILENAME).is_file());
    }
}
