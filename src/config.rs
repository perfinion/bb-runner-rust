use std::collections::HashMap;
use std::convert::AsRef;
use std::env;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use rsjsonnet_front::Session;
use rsjsonnet_lang::arena::Arena;
use rsjsonnet_lang::program::Value;
use serde::{Deserialize, Serialize};
use tracing::{self, error, info, warn};
// use serde_json::Result;
use std::net::Ipv4Addr;
use std::thread;

fn default_cgroup_path() -> String {
    "bb_runner".to_string()
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CgroupConfig {
    #[serde(default)]
    pub delegation: bool,
    #[serde(default = "default_cgroup_path")]
    pub path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NetInterfaceConfig {
    pub addr: String,
    #[serde(default)]
    pub multicast: bool,
    /// Parsed IPv4 address in host byte order. Populated by Configuration::new().
    #[serde(skip)]
    pub ip: u32,
    /// Parsed netmask in host byte order. Populated by Configuration::new().
    #[serde(skip)]
    pub netmask: u32,
}

/// Parse an "IP/prefix" string into (IPv4 address, netmask) both in host byte order.
fn parse_cidr(addr: &str) -> Option<(u32, u32)> {
    let (ip_str, prefix_str) = addr.split_once('/')?;
    let prefix_len: u32 = prefix_str.parse().ok()?;
    if prefix_len > 32 {
        return None;
    }

    let ip: Ipv4Addr = ip_str.parse().ok()?;
    let ip_host = u32::from_be_bytes(ip.octets());

    let mask_host = if prefix_len == 0 {
        0u32
    } else {
        !0u32 << (32 - prefix_len)
    };

    Some((ip_host, mask_host))
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Configuration {
    pub build_directory_path: PathBuf,
    pub grpc_listen_path: PathBuf,
    pub num_cpus: u32,
    pub memory_max: Option<NonZeroU64>,
    pub rw_paths: Vec<String>,
    #[serde(default)]
    pub hidden_paths: Vec<String>,
    #[serde(default)]
    pub net_interfaces: HashMap<String, NetInterfaceConfig>,
    #[serde(default)]
    pub user_namespace: bool,
    #[serde(default)]
    pub run_as_user: Option<u32>,
    #[serde(default)]
    pub run_as_group: Option<u32>,
    #[serde(default)]
    pub cgroup: Option<CgroupConfig>,
    #[serde(skip)]
    pub cgroup_root: Option<Arc<PathBuf>>,
    #[serde(skip)]
    pub cgroup_path: Option<String>,
}

fn add_var(session: &mut Session, name: &str, val: &str) -> Option<()> {
    let thunk = session.program_mut().value_to_thunk(&Value::string(val));
    let interned_name = session.program().intern_str(name);
    session.program_mut().add_ext_var(interned_name, &thunk);

    Some(())
}

impl Configuration {
    pub fn new<P: AsRef<Path>>(cfg: P) -> Option<Configuration> {
        warn!("Loading configuration from: {:?}", cfg.as_ref());

        let arena = Arena::new();
        let mut session = Session::new(&arena);
        if let Some(pwd) = env::current_dir().ok()?.to_str() {
            add_var(&mut session, "PWD", pwd);
        }

        let Some(thunk) = session.load_real_file(cfg.as_ref()) else {
            // `Session` printed the error for us
            return None;
        };

        let Some(value) = session.eval_value(&thunk) else {
            // `Session` printed the error for us
            return None;
        };

        // warn!("Config value: {:?}", value);

        // Manifest the value
        let Some(json_result) = session.manifest_json(&value, false) else {
            // `Session` printed the error for us
            return None;
        };

        warn!("Config json: {:?}", json_result);

        let mut config: Configuration = serde_json::from_str::<Configuration>(&json_result).ok()?;

        if config.num_cpus == 0 {
            config.num_cpus = thread::available_parallelism().map_or(1, |p| p.get() as u32);
            info!("Number of processors = {}", config.num_cpus);
        }

        for (name, iface) in config.net_interfaces.iter_mut() {
            let (ip, netmask) = match parse_cidr(&iface.addr) {
                Some(v) => v,
                None => {
                    error!("Invalid addr {:?} for interface {}", iface.addr, name);
                    return None;
                }
            };
            iface.ip = ip;
            iface.netmask = netmask;
        }

        warn!("Config obj: {:?}", config);
        Some(config)
    }
}
