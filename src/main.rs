use build::BuildArgs;
use failure::{err_msg, Error};
use log::{error, info};
use new::NewArgs;
use std::result::Result;
use structopt::StructOpt;

/// The various kinds of commands that `iroha_wasm_pack` can execute.
#[derive(Debug, StructOpt)]
pub enum SubCommand {
    /// 🏗️  build your wasm package!
    #[structopt(name = "build")]
    Build(BuildArgs),

    #[structopt(name = "new")]
    /// 🐑 create a new project
    New(NewArgs),
}

/// 📦 ✨  build and release your wasm!
#[derive(Debug, StructOpt)]
pub struct Args {
    /// The subcommand to run.
    #[structopt(subcommand)] // Note that we mark a field as a subcommand
    pub subcommand: SubCommand,
}

/// Runs subcommand
pub trait RunArgs {
    /// Runs command
    ///
    /// # Errors
    /// if inner command errors
    fn run(self) -> Result<(), Error>;
}

macro_rules! match_run_all {
    (($self:ident), { $($variants:path),* $(,)?}) => {
        match $self {
            $($variants(variant) => RunArgs::run(variant),)*
        }
    };
}

impl RunArgs for SubCommand {
    fn run(self) -> Result<(), Error> {
        use SubCommand::*;
        match_run_all!((self), { Build, New })
    }
}

fn main() {
    let args = Args::from_args();
    if let Err(err) = args.subcommand.run() {
        error!("{}", err);
    }
}

mod build {
    use super::*;
    use serde_derive::Deserialize;
    use std::{
        env::current_dir,
        fs,
        path::{Path, PathBuf},
        str::FromStr,
    };
    use structopt::clap::AppSettings;

    /// Everything required to configure and run the `iroha_wasm_pack build` command.
    #[derive(Debug, StructOpt)]
    #[structopt(
        // Allows unknown `--option`s to be parsed as positional arguments, so we can forward it to `cargo`.
        setting = AppSettings::AllowLeadingHyphen,

        // Allows `--` to be parsed as an argument, so we can forward it to `cargo`.
        setting = AppSettings::TrailingVarArg,
    )]
    pub struct BuildArgs {
        #[structopt(allow_hyphen_values = true)]
        /// List of extra options to pass to `iroha_wasm_pack build`
        pub extra_options: Vec<String>,
    }

    pub struct BuildContext {
        crate_type: String,
        wasm_in: PathBuf,
        wasm_out: PathBuf,
    }

    // Construct this context to reuse in multi build steps
    impl BuildContext {
        fn new(args: &BuildArgs) -> Result<Self, Error> {
            let root = root(current_dir()?)?;
            let config = pasre_cargo_config(&root)?;
            let is_release = args.extra_options.iter().any(|x| x == "--release");
            let profile = if is_release { "release" } else { "debug" };
            let wasm_folder = root
                .join("target")
                .join("wasm32-unknown-unknown")
                .join(profile);
            let wasm_name = &config.package.name;
            let wasm_in = wasm_folder.join(format!("{}{}", wasm_name, ".wasm"));
            let wasm_out = wasm_folder.join(format!("{}{}", wasm_name, "_optimized.wasm"));
            let crate_type = config.lib.crate_type.first().unwrap().to_owned();
            Ok(BuildContext {
                crate_type: crate_type,
                wasm_in: wasm_in,
                wasm_out: wasm_out,
            })
        }
    }

    impl RunArgs for BuildArgs {
        fn run(self) -> Result<(), Error> {
            let ctx = BuildContext::new(&self)?;
            for step in [
                step_check_rustc_version,
                step_check_crate_config,
                step_check_for_wasm_target,
                step_build_wasm,
                step_wasm_opt,
                step_iroha_binary_size_check,
            ] {
                step(&self, &ctx)?
            }
            Ok(())
        }
    }

    /// Find the project root directory.
    fn root(mut cur: PathBuf) -> Result<PathBuf, Error> {
        while !cur.join("Cargo.toml").exists() {
            if !cur.pop() {
                return Err(err_msg("No Cargo.toml found from current dir or parent, you should init a project by `iroha_wasm_pack new` first"));
            }
        }
        Ok(cur)
    }

    /// Fetch rustc version by command
    fn rustc_minor_version() -> Result<u32, Error> {
        use duct::cmd;
        let stdout = cmd!("rustc", "--version").read()?;
        info!("Checked rustc version {}", stdout);
        let mut pieces = stdout.split('.');
        if pieces.next() == Some("rustc 1") {
            if let Some(version) = pieces.next() {
                return Ok(version.parse()?);
            }
        }
        Err(err_msg("We can't figure out what your Rust version is- which means you might not have Rust installed. Please install Rust version 1.30.0 or higher."))
    }

    pub fn step_check_rustc_version(_: &BuildArgs, _: &BuildContext) -> Result<(), Error> {
        // Ensure that `rustc` is present and that it is >= 1.30.0
        let local_minor_version = rustc_minor_version()?;
        if local_minor_version < 30 {
            return Err(err_msg(format!("Your version of Rust, '1.{}', is not supported. Please install Rust version 1.30.0 or higher.", local_minor_version.to_string())));
        }
        Ok(())
    }

    /// Cargo.toml Deserialize
    #[derive(Deserialize)]
    struct Package {
        name: String,
    }

    #[derive(Deserialize)]
    struct Lib {
        #[serde(alias = "crate-type")]
        crate_type: Vec<String>,
    }

    #[derive(Deserialize)]
    struct CargoConfig {
        package: Package,
        lib: Lib,
    }

    /// Parse the cargo toml
    fn pasre_cargo_config(root: &PathBuf) -> Result<CargoConfig, Error> {
        let path = root.join("Cargo.toml");
        let cargo_xml = fs::read_to_string(path.to_str().unwrap()).unwrap();
        match toml::from_str(&cargo_xml) {
            Ok(config) => Ok(config),
            Err(err) => Err(err_msg(format!("parse cargo toml failed, error = {}", err))),
        }
    }

    /// Check crate-type
    pub fn step_check_crate_config(_: &BuildArgs, ctx: &BuildContext) -> Result<(), Error> {
        if ctx.crate_type == "cdylib" {
            Ok(())
        } else {
            let msg = format!("crate-type must be cdylib to compile to wasm32-unknown-unknown. Add the following to your \
                Cargo.toml file:\n\n\
                [lib]\n\
                crate-type = [\"cdylib\"]");
            Err(err_msg(msg))
        }
    }

    /// Get rustc's sysroot as a PathBuf
    fn get_rustc_sysroot() -> Result<PathBuf, Error> {
        use duct::cmd;
        let result = cmd!("rustc", "--print", "sysroot").read();
        if result.is_err() {
            return Err(err_msg(format!(
                "Getting rustc's sysroot wasn't successful. Got {}",
                result.unwrap()
            )));
        }
        let stdout = result?;
        info!("Rustc sysroot: {}", stdout);
        Ok(PathBuf::from_str(&stdout).unwrap())
    }

    /// Checks if the wasm32-unknown-unknown is present in rustc's sysroot.
    fn is_wasm32_target_in_sysroot(sysroot: &Path) -> bool {
        let wasm32_target = "wasm32-unknown-unknown";

        let rustlib_path = sysroot.join("lib/rustlib");

        info!("Looking for {} in {:?}", wasm32_target, rustlib_path);

        if rustlib_path.join(wasm32_target).exists() {
            info!("Found {} in {:?}", wasm32_target, rustlib_path);
            true
        } else {
            info!("Failed to find {} in {:?}", wasm32_target, rustlib_path);
            false
        }
    }

    /// Add wasm32-unknown-unknown using `rustup`.
    fn rustup_add_wasm_target() -> Result<(), Error> {
        use duct::cmd;
        let result = cmd!("rustup", "target", "add", "wasm32-unknown-unknown").run();
        if let Err(err) = result {
            return Err(err_msg(format!(
                "Adding the wasm32-unknown-unknown target with rustup failed, error = {}",
                err
            )));
        }
        Ok(())
    }

    pub fn step_check_for_wasm_target(_: &BuildArgs, _: &BuildContext) -> Result<(), Error> {
        let sysroot = get_rustc_sysroot()?;

        // If wasm32-unknown-unknown already exists we're ok.
        if is_wasm32_target_in_sysroot(&sysroot) {
            Ok(())
        // If it doesn't exist, then we need to check if we're using rustup.
        } else {
            // If sysroot contains "rustup", then we can assume we're using rustup
            // and use rustup to add the wasm32-unknown-unknown target.
            if sysroot.to_string_lossy().contains("rustup") {
                rustup_add_wasm_target()
            } else {
                Ok(())
            }
        }
    }

    pub fn step_build_wasm(args: &BuildArgs, _: &BuildContext) -> Result<(), Error> {
        use duct::cmd;
        let extra_args: Vec<&str> = args.extra_options.iter().map(|s| &s[..]).collect();
        let mut args = vec![
            "+nightly",
            "build",
            "-Z",
            "build-std",
            "-Z",
            "build-std-features=panic_immediate_abort",
            "--target",
            "wasm32-unknown-unknown",
        ];
        extra_args.iter().for_each(|x| args.push(x));
        let result = cmd("cargo", args).run();
        if let Err(err) = result {
            return Err(err_msg(format!("build wasm failed, error = {}", err)));
        }
        Ok(())
    }

    pub fn step_wasm_opt(_: &BuildArgs, ctx: &BuildContext) -> Result<(), Error> {
        use wasm_opt::OptimizationOptions;
        OptimizationOptions::new_optimize_for_size().run(&ctx.wasm_in, &ctx.wasm_out)?;
        Ok(())
    }

    pub fn step_iroha_binary_size_check(_: &BuildArgs, ctx: &BuildContext) -> Result<(), Error> {
        let len = fs::metadata(&ctx.wasm_out)?.len();
        if len > 4194304 {
            return Err(err_msg(format!(
                "Wasm binary too large, max size is 4194304, but got {}",
                len
            )));
        }
        Ok(())
    }
}

mod new {
    use super::*;
    use std::{env::current_dir, fs, path::Path};

    /// Everything required to configure and run the `iroha_wasm_pack new` command.
    #[derive(Debug, StructOpt)]
    pub struct NewArgs {
        /// Name of the new project
        pub name: String,
    }

    impl RunArgs for NewArgs {
        fn run(self) -> Result<(), Error> {
            for step in [step_cargo_new, step_cargo_xml, step_main_entrypoint] {
                step(&self)?;
            }
            Ok(())
        }
    }

    /// Writes a file to disk.
    pub fn write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, contents: C) -> Result<(), Error> {
        let path = path.as_ref();
        if let Err(err) = fs::write(path, contents.as_ref()) {
            return Err(err_msg(format!(
                "write to {} failed, error = {}",
                path.display(),
                err
            )));
        }
        Ok(())
    }

    /// Init project by `cargo new --lib`
    pub fn step_cargo_new(args: &NewArgs) -> Result<(), Error> {
        use duct::cmd;
        if let Err(err) = cmd!("cargo", "new", &args.name, "--lib").run() {
            return Err(err_msg(format!("init project failed, error = {}", err)));
        }
        Ok(())
    }

    /// Cargo xml release profile for reducing the size of wasm binary
    pub fn step_cargo_xml(args: &NewArgs) -> Result<(), Error> {
        let mut cargo_xml = format!(
            r#"[package]
name = "{}"
version = "0.1.0"
edition = "2021"
"#,
            args.name
        );
        cargo_xml.push_str(
            r#"
[lib]
# A smart contract should be linked dynamically so that it may link to functions exported
# from the host environment. The host environment executes a smart contract by
# calling the function that smart contract exports (entry point of execution)
crate-type = ['cdylib']

[profile.release]
strip = "debuginfo" # Remove debugging info from the binary
panic = "abort"     # Panics are transcribed to Traps when compiling for WASM
lto = true          # Link-time-optimization produces notable decrease in binary size
opt-level = "z"     # Optimize for size vs speed with "s"/"z" (removes vectorization)
codegen-units = 1   # Further reduces binary size but increases compilation time

[dependencies]
iroha_data_model = { git = "https://github.com/hyperledger/iroha/", branch = "iroha2-dev", default-features = false }
iroha_wasm = { git = "https://github.com/hyperledger/iroha/", branch = "iroha2-dev" }

[dev-dependencies]
webassembly-test-runner = { version = "0.1.0" }
"#);
        let path = current_dir().unwrap().join(&args.name).join("Cargo.toml");
        write(path.as_path(), cargo_xml.as_bytes())
    }

    /// Iroha boilerplate main entrypoint
    pub fn step_main_entrypoint(args: &NewArgs) -> Result<(), Error> {
        let entrypoint = r#"//! Smartcontract which creates new nft for every user
//!
//! This module isn't included in the build-tree,
//! but instead it is being built by a `client/build.rs`

#![no_std]
#![no_main]
#![allow(clippy::all)]

//! Sample smartcontract which mints 1 rose for it's authority

use core::str::FromStr as _;

use iroha_wasm::{data_model::prelude::*, DebugExpectExt};

/// Mint 1 rose for authority
#[iroha_wasm::entrypoint(params = "[authority]")]
fn trigger_entrypoint(authority: <Account as Identifiable>::Id) {
    let rose_definition_id = <AssetDefinition as Identifiable>::Id::from_str("token#open")
        .dbg_expect("Failed to parse `token#open` asset definition id");
    let rose_id = <Asset as Identifiable>::Id::new(rose_definition_id, authority);

    Instruction::Mint(MintBox::new(1_u32, rose_id)).execute();
}    
"#;
        let path = current_dir()
            .unwrap()
            .join(&args.name)
            .join("src")
            .join("lib.rs");
        write(path.as_path(), entrypoint.as_bytes())
    }
}
