pub mod config;
pub mod error;
pub mod utils;

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use anyhow::Result;

pub trait TestContainer {
    async fn get_node_url(&self) -> String;
    async fn run_command(&self, command: &str) -> Result<(String, String)>;
    async fn lazy_init_accounts(&self) -> Result<()>;
    async fn copy_contracts(&self, local_dir: impl AsRef<Path>) -> Result<PathBuf>;
    async fn run(
        &self,
        number_of_accounts: usize,
        callback: impl FnOnce(Vec<String>) -> Pin<Box<dyn Future<Output=Result<()>>>>,
    ) -> Result<()>;
    async fn upload_contracts(
        &self,
        local_dir: &str,
        private_key: &str,
        named_addresses: &HashMap<String, String>,
        sub_packages: Option<Vec<&str>>,
        override_contract: bool,
    ) -> Result<()>;
}