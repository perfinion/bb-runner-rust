use std::convert::AsRef;
use std::env;
use std::path::{Path, PathBuf};
//use std::sync::Arc;
use tracing::{self, info, warn};
use rsjsonnet_front::Session;
use rsjsonnet_lang::arena::Arena;
use rsjsonnet_lang::program::Value;
use serde::{Deserialize, Serialize};
// use serde_json::Result;
use std::thread;

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Configuration {
    pub build_directory_path: PathBuf,
    pub grpc_listen_path: PathBuf,
    pub num_cpus: u32,
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

        let mut config: Configuration =
            serde_json::from_str::<Configuration>(&json_result).ok()?;

        if config.num_cpus == 0 {
            config.num_cpus = match thread::available_parallelism() {
                Ok(p) => p.get() as u32,
                _ => 1,
            };
            info!("Number of processors = {}", config.num_cpus);
        }

        warn!("Config obj: {:?}", config);
        Some(config)
    }
}
