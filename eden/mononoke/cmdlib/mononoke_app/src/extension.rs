/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use clap::{App, ArgMatches, Args, FromArgMatches};
use environment::MononokeEnvironment;

/// Trait implemented by things that need o extend arguments and modify the
/// environment before it is used to start Mononoke.
pub trait ArgExtension {
    /// Argument type to extend Mononoke arguments with.
    type Args: clap::Args;

    /// Obtain default values for these arguments.
    fn arg_defaults(&self) -> Vec<(&'static str, String)> {
        Vec::new()
    }

    /// Hook executed after creating the environment before initializing Mononoke.
    fn environment_hook(&self, _args: &Self::Args, _env: &mut MononokeEnvironment) -> Result<()> {
        Ok(())
    }
}

/// Internal trait to hide the associated args type.
pub(crate) trait ArgExtensionBox {
    fn augment_args<'help>(&self, app: App<'help>) -> App<'help>;
    fn arg_defaults(&self) -> Vec<(&'static str, String)>;
    fn environment_hook(&self, args: &ArgMatches, env: &mut MononokeEnvironment) -> Result<()>;
}

impl<Ext> ArgExtensionBox for Ext
where
    Ext: ArgExtension,
{
    fn augment_args<'help>(&self, app: App<'help>) -> App<'help> {
        Ext::Args::augment_args_for_update(app)
    }

    fn arg_defaults(&self) -> Vec<(&'static str, String)> {
        self.arg_defaults()
    }

    fn environment_hook(&self, args: &ArgMatches, env: &mut MononokeEnvironment) -> Result<()> {
        let args = Ext::Args::from_arg_matches(args)?;
        self.environment_hook(&args, env)
    }
}
