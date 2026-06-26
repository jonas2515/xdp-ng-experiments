use std::collections::HashMap;

use anyhow::{Context, anyhow};
use command_fds::{CommandFdExt, FdMapping};
use gtk4::prelude::*;
use gtk4::{gio, glib};
use memfd::{FileSeal, Memfd, MemfdOptions};
use zlink::connection::socket::FetchPeerCredentials;

use crate::interface;

const DEFAULT_ICON_VALIDATOR: &str = "/usr/libexec/xdg-desktop-portal-validate-icon";
const VALIDATOR_INPUT_FD: i32 = 3;
const ICON_VALIDATOR_GROUP: &str = "Icon Validator";

/* Reads the Flatpak .flatpak-info metadata for a process with the given PID.
 * Returns None if the process is not a Flatpak app (no .flatpak-info, fuse rootfs, etc.).
 * Ports open_flatpak_info() from xdp-app-info-flatpak.c.
 */
fn read_flatpak_info(pid: u32) -> Option<glib::KeyFile> {
    let root_path = format!("/proc/{}/root", pid);

    // A fuse rootfs (e.g. from toolbox) means this is not a Flatpak.
    if !std::path::Path::new(&root_path).is_dir() {
        return None;
    }

    let info_path = format!("/proc/{}/root/.flatpak-info", pid);
    let info_data = std::fs::read_to_string(&info_path).ok()?;

    let key_file = glib::KeyFile::new();
    key_file
        .load_from_data(&info_data, glib::KeyFileFlags::NONE)
        .ok()?;

    Some(key_file)
}

// FIXME: this would need a lot more checks to match the one from xdg-app-info
// FIXME: we should be using the pidfd, but looks like xdp isn't doing that yet either
async fn app_id_from_pid(pid: u32) -> Result<String, anyhow::Error> {
    // Only allow the portal to be used from within flatpak.
    // This allows us to port only one backend (the flatpak backend for XdpAppInfo),
    // and not bother with the host backend.
    let flatpak_info = read_flatpak_info(pid).ok_or(anyhow!("Not a flatpak app"))?;

    flatpak_info
        .string("Application", "name")
        .or_else(|_| flatpak_info.string("Runtime", "name"))
        .map(|s| s.to_string())
        .map_err(|e| e.into())
}

pub async fn get_flatpak_app_id<Sock>(
    connection: &mut zlink::Connection<Sock>
) -> Result<String, interface::DynamicLauncherError>
where
    Sock: zlink::connection::Socket,
    Sock::ReadHalf: FetchPeerCredentials,
{
    let creds = connection.peer_credentials().await
        .map_err(|e| interface::DynamicLauncherError::Other {
            message: format!("Getting peer credentials failed: {}", e),
        })?;

    // DEBUG: allow calling the portal from the host and hardcode the flatpak ID of the demo-client
    //return Ok("com.example.XdpNgDynamicLauncher".to_string());

    let app_id = app_id_from_pid(creds.process_id().as_raw_pid() as u32).await
        .map_err(|e| interface::DynamicLauncherError::Other {
            message: format!("Getting app id failed: {}", e),
        })?;

    assert!(!app_id.is_empty());

    Ok(app_id)
}

pub fn sealed_fd_new_from_bytes(bytes: glib::Bytes) -> Result<Memfd, anyhow::Error> {
    let opts = MemfdOptions::default().allow_sealing(true);
    let mfd = opts.create("xdp-sealed-fd")?;

    mfd.as_file().set_len((*bytes).len() as u64)?;

    std::io::copy(&mut (*bytes).as_ref(), &mut mfd.as_file())?;

    mfd.add_seals(&[
        FileSeal::SealGrow,
        FileSeal::SealWrite,
        FileSeal::SealShrink,
    ])?;

    Ok(mfd)
}

pub enum IconType {
    Desktop,
    #[allow(dead_code)]
    Webapp,
}

impl IconType {
    fn to_ruleset(&self) -> &'static str {
        match self {
            IconType::Desktop => "desktop",
            IconType::Webapp => "webapp",
        }
    }
}

fn parse_validator_output(output: &str) -> Result<(String, String), anyhow::Error> {
    let key_file = glib::KeyFile::new();
    key_file
        .load_from_data(output, glib::KeyFileFlags::NONE)
        .map_err(|e| anyhow!("Icon validation: {}", e))?;

    let icon_format = key_file
        .string(ICON_VALIDATOR_GROUP, "format")
        .map_err(|e| anyhow!("Icon validation: {}", e))?;
    let width = key_file
        .integer(ICON_VALIDATOR_GROUP, "width")
        .map_err(|e| anyhow!("Icon validation: {}", e))?;
    Ok((icon_format.to_string(), format!("{width}")))
}

pub fn xdp_validate_icon(
    icon: Memfd,
    icon_type: IconType,
) -> Result<(String, String), anyhow::Error> {
    let validator =
        std::env::var("XDP_VALIDATE_ICON").unwrap_or_else(|_| DEFAULT_ICON_VALIDATOR.to_string());

    if !std::path::Path::new(&validator).exists() {
        anyhow::bail!(
            "Icon validation: {} not found, rejecting icon by default.",
            validator
        );
    }

    let mut cmd = std::process::Command::new(&validator);
    if std::env::var_os("XDP_VALIDATE_ICON_INSECURE").is_none() {
        cmd.arg("--sandbox");
    }
    cmd.arg("--fd")
        .arg(VALIDATOR_INPUT_FD.to_string())
        .arg("--ruleset")
        .arg(icon_type.to_ruleset());

    cmd.fd_mappings(vec![FdMapping {
        parent_fd: icon.into_file().into(),
        child_fd: VALIDATOR_INPUT_FD,
    }])?;

    let output = cmd.output().map_err(|e| {
        anyhow!(
            "Icon validation: Rejecting icon: Couldn't run validator: {}",
            e
        )
    })?;

    let output_str = String::from_utf8_lossy(&output.stdout);

    if !output.status.success() {
        anyhow::bail!(
            "Icon validation: Rejecting icon because validator failed: {}",
            output_str
        );
    }

    parse_validator_output(&output_str)
}

pub fn validate_serialized_icon(
    icon_v: serde_json::Value,
) -> Result<(String, String, gio::BytesIcon), anyhow::Error> {
    let string = icon_v
        .as_str()
        .ok_or(anyhow!("JSON value is not a string"))?;

    let variant_type = glib::VariantTy::new("(sv)")?;
    let variant = glib::variant::Variant::parse(Some(&variant_type), string)?;

    let icon = gio::Icon::deserialize(&variant).ok_or(anyhow!("Failed to deserialize icon"))?;

    let bytes_icon = icon
        .downcast::<gio::BytesIcon>()
        .map_err(|_| anyhow!("Icon is not a BytesIcon"))?;

    let sealed = sealed_fd_new_from_bytes(bytes_icon.bytes())?;
    let (icon_format, icon_size) = xdp_validate_icon(sealed, IconType::Desktop)?;

    Ok((icon_format, icon_size, bytes_icon))
}

/** FROM C DOCS:
 *
 * Checks if @string is a valid application name.
 *
 * App names are composed of 3 or more elements separated by a period
 * ('.') character. All elements must contain at least one character.
 *
 * Each element must only contain the ASCII characters
 * "[A-Z][a-z][0-9]_-". Elements may not begin with a digit.
 * Additionally "-" is only allowed in the last element.
 *
 * App names must not begin with a '.' (period) character.
 *
 * App names must not exceed 255 characters in length.
 *
 * The above means that any app name is also a valid DBus well known
 * bus name, but not all DBus names are valid app names. The difference are:
 * 1) DBus name elements may contain '-' in the non-last element.
 * 2) DBus names require only two elements
 */
fn flatpak_is_valid_name(s: &str) -> Result<(), anyhow::Error> {
    if s.is_empty() || s.len() > 255 {
        anyhow::bail!("Name empty or too long");
    }
    if s.starts_with('.') {
        anyhow::bail!("Name starts with \".\"");
    }

    let Some(last_dot) = s.rfind('.') else {
        anyhow::bail!("Name must have at least 3 segments");
    };
    let mut dot_count = 0;

    let mut segment_start = true;

    for (i, c) in s.char_indices() {
        if c == '.' {
            dot_count += 1;
            segment_start = true;
            continue;
        }

        if segment_start {
            // Initial character of a segment: [A-Za-z_]
            if !c.is_ascii_alphabetic() && c != '_' {
                anyhow::bail!("Initial character of segment is not alphabetic");
            }
            segment_start = false;
        } else {
            // Subsequent characters: [A-Za-z0-9_] plus '-' in the last element only
            let in_last_element = i > last_dot;

            if !c.is_ascii_alphanumeric() && c != '_' && !(in_last_element && c == '-') {
                anyhow::bail!("Character of segment is not alphanumeric");
            }
        }
    }

    if s.ends_with('.') {
        anyhow::bail!("Name may not end on a dot");
    }

    if dot_count < 2 {
        anyhow::bail!("Name must have at least 3 segments");
    }

    Ok(())
}

pub fn validate_desktop_file_id(app_id: &str, desktop_file_id: &str) -> Result<(), anyhow::Error> {
    let Some(stem) = desktop_file_id.strip_suffix(".desktop") else {
        anyhow::bail!(
            "Desktop file id missing .desktop suffix: {}",
            desktop_file_id
        );
    };

    /* In the original C code this is a vfunc that can check appIds for various
     * backends, here we only implement the checks that
     * xdp_app_info_flatpak_is_valid_sub_app_id() does.
     */
    assert!(!app_id.is_empty());

    let expected_prefix = format!("{}.", app_id);
    if !stem.starts_with(&expected_prefix) {
        anyhow::bail!("Flatpak sub app id validation: Desktop file id isn't suffixed with app id");
    }

    flatpak_is_valid_name(stem).context("Flatpak sub app id validation failed")?;

    Ok(())
}

pub fn validate_prepare_install_options(
    options: &HashMap<String, serde_json::Value>,
    supported_launcher_types: &[interface::LauncherType],
) -> Result<(), anyhow::Error> {
    for key in ["modal", "editable_name", "editable_icon"] {
        if let Some(v) = options.get(key) {
            if !v.is_boolean() {
                anyhow::bail!("Option '{}' must be a boolean", key);
            }
        }
    }

    if let Some(v) = options.get("launcher_type") {
        let launcher_type: interface::LauncherType =
            serde_json::from_value(v.clone()).context("Invalid launcher_type")?;

        if !supported_launcher_types.contains(&launcher_type) {
            anyhow::bail!("Unsupported launcher_type: {:?}", launcher_type);
        }

        if let Some(v) = options.get("target") {
            let uri = v.as_str().context("Option 'target' must be a string")?;

            glib::Uri::parse(uri, glib::UriFlags::NONE).context("Given URI is invalid")?;
        }
    }

    Ok(())
}

pub fn validate_dynamic_launcher(
    app_id: &str,
    key_file: &glib::KeyFile,
) -> Result<(), interface::DynamicLauncherError> {
    /* In theory xdp_app_info_validate_dynamic_launcher() calls backend-specific
     * vfuncs, but we limit ourselves to the flatpak impl here.
     */
    assert!(!app_id.is_empty());

    let exec = key_file
        .string(
            glib::KEY_FILE_DESKTOP_GROUP,
            glib::KEY_FILE_DESKTOP_KEY_EXEC,
        )
        .map_err(|_| interface::DynamicLauncherError::InvalidArgument {
            message: format!("Desktop entry given to Install() has no Exec line"),
        })?;

    let exec_argv = glib::shell_parse_argv(&exec).map_err(|e| {
        interface::DynamicLauncherError::InvalidArgument {
            message: format!(
                "Desktop entry given to Install() has invalid Exec line: {}",
                e
            ),
        }
    })?;

    // Don't let the app give itself access to host files
    if exec_argv
        .iter()
        .any(|arg| arg.to_string_lossy() == "--file-forwarding")
    {
        return Err(interface::DynamicLauncherError::InvalidArgument {
            message: "Desktop entry given to Install() must not use --file-forwarding".to_string(),
        });
    }

    // Rewrite the Exec= commandline to prepend `flatpak run`
    let mut parts = vec!["flatpak".to_string(), "run".to_string()];
    if !exec_argv.is_empty() {
        parts.push(format!(
            "--command={}",
            glib::shell_quote(&exec_argv[0]).to_string_lossy()
        ));
        parts.push(glib::shell_quote(app_id).to_string_lossy().to_string());
        for arg in &exec_argv[1..] {
            parts.push(glib::shell_quote(arg).to_string_lossy().to_string());
        }
    } else {
        parts.push(glib::shell_quote(app_id).to_string_lossy().to_string());
    }
    key_file.set_value(
        glib::KEY_FILE_DESKTOP_GROUP,
        glib::KEY_FILE_DESKTOP_KEY_EXEC,
        &parts.join(" "),
    );

    // NB: Not setting the TryExec line here, too complicated to port (and doesn't seem
    // like we want it anyway in the future), let's leave it out for now.

    // Flatpak checks for this key
    key_file.set_value(glib::KEY_FILE_DESKTOP_GROUP, "X-Flatpak", app_id);
    // Flatpak removes this one for security
    let _ = key_file.remove_key(
        glib::KEY_FILE_DESKTOP_GROUP,
        "X-GNOME-Bugzilla-ExtraInfoScript",
    );

    Ok(())
}