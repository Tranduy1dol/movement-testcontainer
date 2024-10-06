extern crate testcontainers;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;
use std::{fs, path};

use anyhow::{ensure, Error, Result};
use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use log::debug;
use testcontainers::core::{ExecCommand, IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::time::Instant;

use container_interface::config::EnvConfig;
use container_interface::error::MovementTestContainerError::{CommandFailed, DockerExecFailed};
use container_interface::utils::{generate_random_string, get_files};
use container_interface::TestContainer;

pub struct AptosContainer {
    node_url: String,
    inner_url: String,
    chain_id: u8,
    deploy_contract: bool,
    override_accounts: Option<Vec<String>>,
    container: ContainerAsync<GenericImage>,
    contract_path: String,
    contracts: Mutex<HashSet<String>>,
    accounts: RwLock<Vec<String>>,
    accounts_channel_rx: Mutex<Option<Receiver<String>>>,
    accounts_channel_tx: RwLock<Option<Sender<String>>>,
}

const APTOS_IMAGE: &str = "sotazklabs/aptos-tools";
const APTOS_IMAGE_TAG: &str = "mainnet";
const FILTER_PATTERN: &str = r"^(?:\.git|target\/|.idea|Cargo.lock|build\/|.aptos\/)";
const ACCOUNTS_ENV: &str = "ACCOUNTS";
const CONTENT_MAX_CHARS: usize = 120000;
const MOVE_TOML: &[u8] = &[0];

impl TestContainer for AptosContainer {
    async fn get_node_url(&self) -> String {
        self.node_url.clone()
    }

    async fn run_command(&self, command: &str) -> Result<(String, String)> {
        let mut result = self
            .container
            .exec(ExecCommand::new(vec!["/bin/sh", "-c", command]))
            .await?;

        // Check the exit code of the command.
        result
            .exit_code()
            .await?
            .map(|code| Err(Error::new(DockerExecFailed(code))))
            .unwrap_or(Ok(()))?;
        // Initialize empty strings for capturing stdout and stderr.
        let mut stdout = String::new();
        let mut stderr = String::new();

        // Read the command's stdout into the `stdout` string.
        result.stdout().read_to_string(&mut stdout).await?;
        // Read the command's stderr into the `stderr` string.
        result.stderr().read_to_string(&mut stderr).await?;
        Ok((stdout, stderr))
    }

    async fn lazy_init_accounts(&self) -> Result<()> {
        if self.override_accounts.is_some() {
            return Ok(());
        }

        let mut guard = self.accounts_channel_tx.write().await;

        if guard.is_some() {
            return Ok(());
        }

        let command = format!("echo ${}", ACCOUNTS_ENV);
        let (stdout, stderr) = self.run_command(&command).await?;
        ensure!(
            !stdout.is_empty(),
            CommandFailed {
                command,
                stderr: format!("stdout: {} \n\n stderr: {}", stdout, stderr)
            }
        );

        let accounts = stdout
            .trim()
            .split(",")
            .map(|s| s.to_string())
            .collect::<Vec<String>>();
        let (tx, rx) = mpsc::channel(accounts.len());
        for account in accounts.iter() {
            tx.send(account.to_string()).await?
        }
        *self.accounts.write().await = accounts;
        *self.accounts_channel_rx.lock().await = Some(rx);
        *guard = Some(tx);
        Ok(())
    }

    async fn copy_contracts(&self, local_dir: impl AsRef<Path>) -> Result<PathBuf> {
        let contract_path =
            Path::new(&self.contract_path).join(generate_random_string(6));
        let contract_path_str = contract_path.to_str().unwrap();

        let command = format!("rm -rf {}", contract_path_str);
        let (_, stderr) = self.run_command(&command).await?;
        ensure!(stderr.is_empty(), CommandFailed { command, stderr });

        let local_dir_str = local_dir.as_ref().to_str().unwrap();
        for entry in get_files(local_dir_str, FILTER_PATTERN) {
            let source_path = entry.path();
            let relative_path = source_path.strip_prefix(local_dir_str)?;
            let dest_path = contract_path.join(relative_path);
            let content = fs::read(source_path)?;
            let encoded_content = BASE64_STANDARD.encode(&content);
            for chunk in encoded_content
                .chars()
                .collect::<Vec<char>>()
                .chunks(CONTENT_MAX_CHARS)
            {
                let command = format!(
                    "mkdir -p \"$(dirname '{}')\" && (echo '{}' | base64 --decode >> '{}')",
                    dest_path.to_str().unwrap(),
                    chunk.iter().collect::<String>(),
                    dest_path.to_str().unwrap()
                );
                let (_, stderr) = self.run_command(&command).await?;
                ensure!(stderr.is_empty(), CommandFailed { command, stderr });
            }
        }
        Ok(contract_path)
    }

    async fn run(
        &self,
        number_of_accounts: usize,
        callback: impl FnOnce(Vec<String>) -> Pin<Box<dyn Future<Output=Result<()>>>>,
    ) -> Result<()> {
        self.lazy_init_accounts().await?;

        let accounts = match &self.override_accounts {
            Some(accounts) => accounts.clone(),
            None => {
                let mut result = vec![];
                self.accounts_channel_rx
                    .lock()
                    .await
                    .as_mut()
                    .unwrap()
                    .recv_many(&mut result, number_of_accounts)
                    .await;
                result
            }
        };

        let result = callback(accounts.clone()).await;

        if self.override_accounts.is_none() {
            let guard = self.accounts_channel_tx.read().await;
            for account in accounts {
                guard.as_ref().unwrap().send(account).await?;
            }
        }
        result
    }

    async fn upload_contracts(&self, local_dir: &str, private_key: &str, named_addresses: &HashMap<String, String>, sub_packages: Option<Vec<&str>>, override_contract: bool) -> Result<()> {
        if !self.deploy_contract {
            return Ok(());
        }

        let absolute = path::absolute(local_dir)?;
        let absolute_contract_path = absolute.to_str().unwrap();
        let contract_key = format!("{}:{}", private_key, absolute_contract_path);

        let mut inserted_contracts = self.contracts.lock().await;
        if !override_contract && inserted_contracts.contains(&contract_key) {
            return Ok(());
        }
        let now = Instant::now();
        let contract_path = self.copy_contracts(local_dir).await?;
        debug!("copy_contracts takes: {:.2?}", now.elapsed());

        let contract_path_str = contract_path.to_str().unwrap();

        if sub_packages.is_none() {
            let dest_path = contract_path.join("Move.toml");
            let encoded_content = BASE64_STANDARD.encode(MOVE_TOML);
            let command = format!(
                "mkdir -p \"$(dirname '{}')\" && (echo '{}' | base64 --decode > '{}')",
                dest_path.to_str().unwrap(),
                encoded_content,
                dest_path.to_str().unwrap()
            );
            let (_, stderr) = self.run_command(&command).await?;
            ensure!(stderr.is_empty(), CommandFailed { command, stderr });
        }

        let named_address_params = named_addresses
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .reduce(|acc, cur| format!("{},{}", acc, cur))
            .map(|named_addresses| format!("--named-addresses {}", named_addresses))
            .unwrap_or("".to_string());
        match sub_packages {
            None => {
                let command = format!(
                    "cd {} && aptos move publish --skip-fetch-latest-git-deps --private-key {} --assume-yes {} --url {} --included-artifacts none",
                    contract_path_str, private_key, named_address_params, self.inner_url
                );
                let (stdout, stderr) = self.run_command(&command).await?;
                ensure!(
                    stdout.contains(r#""vm_status": "Executed successfully""#),
                    CommandFailed {
                        command,
                        stderr: format!("stdout: {} \n\n stderr: {}", stdout, stderr)
                    }
                );
            }
            Some(sub_packages) => {
                for sub_package in sub_packages {
                    let command = format!(
                        "cd {}/{} && aptos move publish --skip-fetch-latest-git-deps --private-key {} --assume-yes {} --url {} --included-artifacts none",
                        contract_path_str, sub_package, private_key, named_address_params, self.inner_url
                    );
                    let (stdout, stderr) = self.run_command(&command).await?;
                    ensure!(
                        stdout.contains(r#""vm_status": "Executed successfully""#),
                        CommandFailed {
                            command,
                            stderr: format!("stdout: {} \n\n stderr: {}", stdout, stderr)
                        }
                    );
                }
            }
        }

        inserted_contracts.insert(contract_key);
        Ok(())
    }
}

impl AptosContainer {
    async fn init() -> Result<Self> {
        let config = EnvConfig::new();
        let enable_node = config.enable_node.unwrap_or(true);

        let (entrypoint, cmd, wait_for) = if enable_node {
            (
                "aptos",
                vec!["node", "run-localnet", "--performance", "--no-faucet"],
                WaitFor::message_on_stderr("Setup is complete, you can now use the localnet!"),
            )
        } else {
            ("/bin/sh", vec!["-c", "sleep infinity"], WaitFor::Nothing)
        };

        let container = GenericImage::new(APTOS_IMAGE, APTOS_IMAGE_TAG)
            .with_exposed_port(8080.tcp())
            .with_wait_for(wait_for)
            .with_entrypoint(entrypoint)
            .with_cmd(cmd)
            .with_startup_timeout(Duration::from_secs(10))
            .start()
            .await?;

        let (node_url, inner_url, deploy_contract, override_accounts, chain_id) = if enable_node {
            let node_url = format!(
                "http://{}:{}",
                container.get_host().await?,
                container.get_host_port_ipv4(8080).await?
            );
            (
                node_url.to_string(),
                "http://localhost:8080".to_string(),
                true,
                None,
                4,
            )
        } else {
            let node_url = config.node_url.unwrap().first().unwrap().to_string();
            (
                node_url.clone(),
                node_url,
                config.deploy_contract.unwrap_or(true),
                Some(config.accounts.unwrap()),
                config.chain_id.unwrap(),
            )
        };

        Ok(Self {
            node_url,
            inner_url,
            deploy_contract,
            chain_id,
            container,
            override_accounts,
            contract_path: "/contract".to_string(),
            contracts: Default::default(),
            accounts: Default::default(),
            accounts_channel_rx: Default::default(),
            accounts_channel_tx: Default::default(),
        })
    }
}
