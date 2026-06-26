use serde::{Deserialize, Serialize};
use zlink::{
    ReplyError,
    introspect::{CustomType, Type},
};

#[derive(Debug, Clone, PartialEq, ReplyError, zlink::introspect::ReplyError)]
#[zlink(interface = "org.freedesktop.portal2.DynamicLauncher")]
pub enum DynamicLauncherError {
    Cancelled,
    InvalidArgument { message: String },
    Other { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Type)]
pub struct PrepareInstallFinalOutput {
    pub token: String,
    pub name: String,
    pub icon: serde_json::Value,
}

/// Output parameters for the PrepareInstall method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Type)]
pub struct PrepareInstallOutput {
    pub maybe: Option<PrepareInstallFinalOutput>,
}

/// Output parameters for the RequestInstallToken method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Type)]
pub struct RequestInstallTokenOutput {
    pub token: String,
}

/// Output parameters for the GetDesktopEntry method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Type)]
pub struct GetDesktopEntryOutput {
    pub contents: String,
}

/// Output parameters for the GetIcon method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Type)]
pub struct GetIconOutput {
    pub icon_v: serde_json::Value,
    pub icon_format: String,
    pub icon_size: i64,
}

#[allow(unused)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, CustomType)]
pub enum LauncherType {
    Application,
    Webapp,
}

/// Output parameters for the GetSupportedLauncherTypes method.
#[allow(unused)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Type)]
pub struct GetSupportedLauncherTypesOutput {
    pub supported_launcher_types: Vec<LauncherType>,
}
