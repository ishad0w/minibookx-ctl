/// Documentation for the running release; links are version-pinned.
macro_rules! doc_url {
    ($path:literal) => {
        concat!(
            "https://github.com/ishad0w/minibookx-ctl/blob/v",
            env!("CARGO_PKG_VERSION"),
            "/docs/",
            $path
        )
    };
}

pub mod broker;
pub mod build_info;
pub mod doctor;
pub mod ec;
pub mod hardware;
pub mod install;
pub mod ipc;
pub(crate) mod lid;
pub mod paths;
pub(crate) mod platform;
pub mod policy;
pub mod power;
pub mod presets;
pub(crate) mod process;
pub(crate) mod rapl;
pub mod runtime;
pub mod sensor;
pub mod service;
pub mod status;
pub mod systemd;
pub mod tablet;
#[cfg(feature = "dev-tools")]
pub mod thermal;
