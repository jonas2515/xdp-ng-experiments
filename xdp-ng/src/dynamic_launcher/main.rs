// See https://github.com/z-galaxy/zlink/issues/278
#![allow(unused)]

mod interface;
mod long_request;
mod util;

use async_stream::stream;
use futures::{FutureExt, Stream, StreamExt, pin_mut};
use long_request::long_request_server_on_thread;
use util::{
    get_flatpak_app_id, validate_desktop_file_id, validate_dynamic_launcher,
    validate_prepare_install_options, validate_serialized_icon,
};

use anyhow::{Context, anyhow};
use futures::channel::oneshot;
use gio_unix;
use gtk4::prelude::{
    AppInfoExt, AppLaunchContextExt, BoxExt, DialogExt, EditableExt, GtkWindowExt, NativeExt,
    WidgetExt, WindowGroupExt,
};
use gtk4::{gio, glib};
use gxdp::ExternalWindow;
use listen_fds::ListenFds;
use serde_json;
use std::collections::HashMap;
use std::fs::DirBuilder;
use std::os::fd::OwnedFd;
use std::os::unix::fs::DirBuilderExt;
use std::pin::pin;
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use tokio::runtime::Runtime;
use uuid::Uuid;
use zlink::connection::socket::FetchPeerCredentials;
use zlink::{Server, service};

use crate::interface::DynamicLauncherError;

const XDG_PORTAL_APPLICATIONS_DIR: &str = "xdg-desktop-portal/applications";
const XDG_PORTAL_ICONS_DIR: &str = "xdg-desktop-portal/icons";
const MAX_DESKTOP_SIZE_BYTES: usize = 1048576;
const TOKEN_TIMEOUT_SECS: u64 = 300;
const SUPPORTED_LAUNCHER_TYPES: &[interface::LauncherType] = &[
    interface::LauncherType::Application,
    interface::LauncherType::Webapp,
];

pub struct BackendPrepareInstallResponse {
    pub name: String,
    pub icon_bytes: glib::Bytes,
}

fn backend_handle_request_install_token(
    app_id: &str,
    _options: &HashMap<String, serde_json::Value>,
) -> bool {
    const ALLOWLIST: &[&str] = &[
        "org.gnome.Software",
        "org.gnome.SoftwareDevel",
        "com.example.XdpNgDynamicLauncher",
    ];

    ALLOWLIST.contains(&app_id)
}

async fn backend_handle_dialog_request(
    parent_window: String,
    name: String,
    icon_bytes: glib::Bytes,
    options: HashMap<String, serde_json::Value>,
    mut cancel_rx: oneshot::Receiver<()>,
) -> Result<Option<(String, glib::Bytes)>, interface::DynamicLauncherError> {
    // Put the logic into its own block, so that the compiler ends the lifetime of non-Send structs
    // like the gtk4::Window before we await the mpsc response. Otherwise the Send requirement of
    // glib::MainContext::default().spawn() in the calling function will fail.

    const DEFAULT_ICON_SIZE: i32 = 128;

    let external_parent = if !parent_window.is_empty() {
        Some(gxdp::ExternalWindow::new_from_handle(&parent_window).ok_or(
            interface::DynamicLauncherError::Other {
                message: format!("Couldn't get external parent from window handle"),
            },
        )?)
    } else {
        None
    };

    let fake_parent = gtk4::Window::new();

    let launcher_type = match options.get("launcher_type").and_then(|v| v.as_str()) {
        Some("webapp") => interface::LauncherType::Webapp,
        _ => interface::LauncherType::Application,
    };
    let url = if launcher_type == interface::LauncherType::Webapp {
        options
            .get("target")
            .and_then(|v| v.as_str())
            .map(String::from)
    } else {
        None
    };
    let modal = options
        .get("modal")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let editable_name = options
        .get("editable_name")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let title = match launcher_type {
        interface::LauncherType::Webapp => "Create Web App",
        interface::LauncherType::Application => "Create App",
    };

    let dialog = gtk4::Dialog::builder()
        .transient_for(&fake_parent)
        .title(title)
        .modal(modal)
        .use_header_bar(1)
        .destroy_with_parent(true)
        .build();
    dialog.add_button("_Cancel", gtk4::ResponseType::Cancel);
    dialog.add_button("C_reate", gtk4::ResponseType::Ok);
    dialog.set_default_response(gtk4::ResponseType::Ok);

    let content_area = dialog.content_area();
    let gtk_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(5)
        .hexpand(true)
        .margin_top(15)
        .margin_bottom(15)
        .margin_start(15)
        .margin_end(15)
        .build();
    content_area.append(&gtk_box);

    let image = gtk4::Image::builder()
        .pixel_size(DEFAULT_ICON_SIZE)
        .vexpand(true)
        .margin_bottom(10)
        .build();
    gtk_box.append(&image);

    let icon = gio::BytesIcon::new(&icon_bytes.clone());
    image.set_from_gicon(&icon);

    let entry = gtk4::Entry::builder()
        .text(name.as_str())
        .sensitive(editable_name)
        .activates_default(true)
        .build();
    gtk_box.append(&entry);

    if launcher_type == interface::LauncherType::Webapp {
        if let Some(ref url) = url {
            let escaped_address = glib::markup_escape_text(url).as_str().to_owned();
            let markup = format!("<small>{}</small>", escaped_address);
            let label = gtk4::Label::builder()
                .label(markup)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .max_width_chars(40)
                .build();
            label.add_css_class("dim-label");
            gtk_box.append(&label);
        }
    }

    let window_group = gtk4::WindowGroup::new();
    window_group.add_window(&dialog);

    // Can't use a oneshot channel here because dialog.connect_response() has an Fn (not FnOnce)
    // callback. Use unbounded(), where unbounded_send() takes a non-mutable reference for self
    // and therefore works inside Fn closures.
    let (tx, mut rx) = futures::channel::mpsc::unbounded();

    dialog.connect_response(move |d, response| {
        let response = if response == gtk4::ResponseType::Ok {
            let text = entry.text().to_string();
            if text.is_empty() {
                None
            } else {
                Some((text, icon_bytes.clone()))
            }
        } else {
            None
        };

        let _ = tx.unbounded_send(response);

        d.destroy();
    });

    gtk4::prelude::WidgetExt::realize(&dialog);

    let surface = dialog.surface();
    if let (Some(ext), Some(surf)) = (external_parent, surface) {
        ext.set_parent_of(&surf);
    }

    dialog.present();

    // We're running inside glib::MainContext::default().spawn(), so we're on the
    // main GTK thread here. spawn_local() is valid from the main thread.
    // This is how we await the cancel_rx.
    //        glib::MainContext::default().spawn_local(async move {
    // cancel_rx.await.expect("Dialog cancel channel closed without result");

    //   dialog.destroy();
    //          });

    //   let dialog_response =glib::MainContext::default().block_on(rx.next());
    //let dialog_response = rx.next().await.context("Dialog response channel closed without result")?;
    //Ok(dialog_response)

    futures::select!(
        r = cancel_rx => {
            let r = r.map_err(|_| interface::DynamicLauncherError::Other {
                message: format!("Dialog cancel channel closed without result")
            })?;
            dialog.destroy();
            Err(interface::DynamicLauncherError::Cancelled)
        },
        r = rx.next() => {
            let r = r.ok_or(interface::DynamicLauncherError::Other {
                message: format!("Dialog response channel closed without result")
            })?;
            Ok(r)
        }
    )
}

async fn backend_handle_prepare_install(
    parent_window: String,
    name: String,
    icon_bytes: glib::Bytes,
    options: HashMap<String, serde_json::Value>,
    mut cancel_rx: oneshot::Receiver<()>,
) -> Result<Option<BackendPrepareInstallResponse>, interface::DynamicLauncherError> {
    // Dialog is using gtk, so we need to spawn it on the main thread. Invoke the callback
    // on the global-default main context and send results back using a channel.
    let (tx, rx) = oneshot::channel();

    glib::MainContext::default().spawn(async move {
        /* glib::MainContext::default().spawn() requires the future to be Send, and gets annoyed as soon
         * as we want to await while anything non-Send is in scope.
         *
         * glib::MainContext::default().spawn_local() does not require Send, but it only works when
         * called from the main context thread :(
         *
         * Luckily we're on the main thread now, yay!
         */
        glib::MainContext::default().spawn_local(async move {
            tx.send(
                backend_handle_dialog_request(parent_window, name, icon_bytes, options, cancel_rx)
                    .await,
            );
        });
    });

    match rx.await {
        Ok(Ok(Some((chosen_name, icon_bytes)))) => Ok(Some(BackendPrepareInstallResponse {
            name: chosen_name,
            icon_bytes,
        })),
        Ok(Ok(None)) => Ok(None),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(interface::DynamicLauncherError::Other {
            message: format!("Dialog channel got dropped: {:#}", e),
        }),
    }
}

#[derive(Debug, Clone)]
pub struct LauncherData {
    pub name: String,
    pub icon_bytes: glib::Bytes,
    pub icon_format: String,
    pub icon_size: String,
    pub inserted_at: std::time::Instant,
}
struct DynamicLauncher {
    // Eh, needs to be wrapped in Arc<Mutex<>> because of zlink streaming replies:
    // in prepare_install() we return a stream (immediately), meaning we loose the lifetime guarantees and
    // self.transient_permissions can no longer be accessed.
    transient_permissions: Arc<Mutex<HashMap<String, LauncherData>>>,
}

impl DynamicLauncher {
    fn new() -> Self {
        Self {
            transient_permissions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn take_launcher_data(&mut self, token: &str) -> Option<LauncherData> {
        let mut transient_permissions = self.transient_permissions.lock().unwrap();
        let data = transient_permissions.remove(token)?;
        if data.inserted_at.elapsed().as_secs() > TOKEN_TIMEOUT_SECS {
            println!("Revoking expired install token {}", token);
            return None;
        }
        Some(data)
    }
}

async fn handle_prepare_install(
    parent_window: String,
    name: String,
    icon_v: serde_json::Value,
    options: HashMap<String, serde_json::Value>,
    rx: oneshot::Receiver<()>,
    transient_permissions: Arc<Mutex<HashMap<String, LauncherData>>>,
) -> Result<interface::PrepareInstallOutput, interface::DynamicLauncherError> {
    validate_prepare_install_options(&options, SUPPORTED_LAUNCHER_TYPES).map_err(|e| {
        interface::DynamicLauncherError::InvalidArgument {
            message: format!("Invalid PrepareInstall() option: {:#}", e),
        }
    })?;

    let (icon_format, icon_size, icon) = validate_serialized_icon(icon_v).map_err(|e| {
        interface::DynamicLauncherError::InvalidArgument {
            message: format!("Error validating icon: {:#}", e),
        }
    })?;

    let response = backend_handle_prepare_install(parent_window, name, icon, options, rx).await?;

    let response = response.ok_or(interface::DynamicLauncherError::Cancelled)?;

    let token = Uuid::new_v4().to_string();
    transient_permissions.lock().unwrap().insert(
        token.clone(),
        LauncherData {
            name: response.name.clone(),
            icon_bytes: response.icon_bytes.clone(),
            icon_format,
            icon_size,
            inserted_at: std::time::Instant::now(),
        },
    );

    Ok(interface::PrepareInstallOutput {
        maybe: Some(interface::PrepareInstallFinalOutput {
            token,
            name: response.name,
            icon: serde_json::Value::from(response.icon_bytes.to_vec()),
        }),
    })
}

#[service(
    interface = "org.freedesktop.portal2.DynamicLauncher",
    types = [interface::LauncherType],
    vendor = "jonas2515",
    product = "xdp-ng-experiments",
    version = "0.0.1",
    url = "https://github.com/jonas2515/xdp-ng-experiments"
)]
impl<Sock> DynamicLauncher
where
    Sock::ReadHalf: FetchPeerCredentials,
{
    async fn install(
        &mut self,
        token: String,
        desktop_file_id: String,
        desktop_entry: String,
        options: HashMap<String, serde_json::Value>,
        #[zlink(connection)] conn: &mut Connection<Sock>,
    ) -> Result<(), interface::DynamicLauncherError> {
        let app_id = get_flatpak_app_id(conn).await?;

        let data = self.take_launcher_data(&token).ok_or(
            interface::DynamicLauncherError::InvalidArgument {
                message: format!("Token given is invalid: {}", token),
            },
        )?;

        validate_desktop_file_id(&app_id, &desktop_file_id).map_err(|e| {
            interface::DynamicLauncherError::InvalidArgument {
                message: format!("Validating desktop file id failed: {:#}", e),
            }
        })?;

        // Checking for the suffix above, so can unwrap here
        let no_dot_desktop: &str = desktop_file_id.strip_suffix(".desktop").unwrap();
        let icon_name = format!("{}.{}", no_dot_desktop, data.icon_format);
        let subdir = if data.icon_format == "svg" {
            "scalable".to_string()
        } else {
            format!("{}x{}", data.icon_size, data.icon_size)
        };
        let data_dir = glib::user_data_dir();
        let icon_dir = data_dir.join(XDG_PORTAL_ICONS_DIR).join(&subdir);
        let icon_path = icon_dir.join(&icon_name);

        let key_file = glib::KeyFile::new();
        key_file
            .load_from_data(
                &desktop_entry,
                glib::KeyFileFlags::KEEP_COMMENTS | glib::KeyFileFlags::KEEP_TRANSLATIONS,
            )
            .map_err(|_| interface::DynamicLauncherError::Other {
                message: format!("Desktop entry given to Install() not a valid key file"),
            })?;

        // The desktop entry spec supports more than one group but we don't in case
        // there's a security risk.
        let groups = key_file.groups();
        if groups.len() != 1 || groups[0].as_str() != glib::KEY_FILE_DESKTOP_GROUP {
            return Err(interface::DynamicLauncherError::Other {
                message: format!("Desktop entry given to Install() must have exactly one group"),
            });
        }

        // Overwrite Name= and Icon= if they are present
        key_file.set_string(glib::KEY_FILE_DESKTOP_GROUP, "Name", &data.name);
        key_file.set_string(
            glib::KEY_FILE_DESKTOP_GROUP,
            "Icon",
            icon_path.to_str().unwrap_or_default(),
        );

        validate_dynamic_launcher(&app_id, &key_file)?;

        if gio_unix::DesktopAppInfo::from_keyfile(&key_file).is_none() {
            return Err(interface::DynamicLauncherError::Other {
                message: format!("Desktop entry given to Install() not valid"),
            });
        }

        let keyfile_data = key_file.to_data();
        if keyfile_data.len() > MAX_DESKTOP_SIZE_BYTES {
            return Err(interface::DynamicLauncherError::Other {
                message: format!(
                    "Desktop file exceeds max size ({}): {}",
                    MAX_DESKTOP_SIZE_BYTES, desktop_file_id
                ),
            });
        }

        // Write the files last so it's only on-disk if other checks passed
        DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&icon_dir)
            .map_err(|e| interface::DynamicLauncherError::Other {
                message: format!("Failed to create icon dir: {:#}", e),
            })?;
        std::fs::write(&icon_path, data.icon_bytes).map_err(|e| {
            interface::DynamicLauncherError::Other {
                message: format!("Failed to write icon data: {:#}", e),
            }
        })?;

        // Put the desktop file in ~/.local/share/xdg-desktop-portal/applications/ so
        // there's no ambiguity about which launchers were created by this portal.
        let applications_dir = data_dir.join(XDG_PORTAL_APPLICATIONS_DIR);
        DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&applications_dir)
            .map_err(|e| {
                let _ = std::fs::remove_file(&icon_path);
                interface::DynamicLauncherError::Other {
                    message: format!("Failed to create desktop dir: {:#}", e),
                }
            })?;
        let desktop_path = applications_dir.join(&desktop_file_id);
        key_file.save_to_file(&desktop_path).map_err(|e| {
            let _ = std::fs::remove_file(&icon_path);
            interface::DynamicLauncherError::Other {
                message: format!("Failed to save desktop file: {:#}", e),
            }
        })?;

        // Make a sym link in ~/.local/share/applications so the launcher shows up in
        // the desktop environment's menu.
        let link_dir = data_dir.join("applications");
        DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&link_dir)
            .map_err(|e| {
                let _ = std::fs::remove_file(&desktop_path);
                let _ = std::fs::remove_file(&icon_path);
                interface::DynamicLauncherError::Other {
                    message: format!("Failed to create link dir: {:#}", e),
                }
            })?;
        let link_path = link_dir.join(&desktop_file_id);
        let relative_path = format!("../{}/{}", XDG_PORTAL_APPLICATIONS_DIR, desktop_file_id);
        // Remove any existing file at the link path
        let _ = std::fs::remove_file(&link_path);
        std::os::unix::fs::symlink(&relative_path, &link_path).map_err(|e| {
            let _ = std::fs::remove_file(&desktop_path);
            let _ = std::fs::remove_file(&icon_path);
            interface::DynamicLauncherError::Other {
                message: format!("Failed to create symlink: {:#}", e),
            }
        })?;

        Ok(())
    }

    #[zlink(return_fds, more)]
    async fn prepare_install(
        &mut self,
        more: bool,
        parent_window: String,
        name: String,
        icon_v: serde_json::Value,
        options: HashMap<String, serde_json::Value>,
    ) -> impl Stream<
        Item = (
            Result<zlink::Reply<interface::PrepareInstallOutput>, interface::DynamicLauncherError>,
            Vec<OwnedFd>,
        ),
    > + Unpin {
        let (mut tx, rx) = futures::channel::mpsc::channel::<(
            Result<zlink::Reply<interface::PrepareInstallOutput>, interface::DynamicLauncherError>,
            Vec<OwnedFd>,
        )>(1);

        let transient_permissions = Arc::clone(&self.transient_permissions);
        tokio::spawn(async move {
            let cancel_rx = match long_request_server_on_thread() {
                Ok((fd, cancel_rx)) => {
                    let _ = tx.try_send((
                        Ok(zlink::Reply::new(Some(interface::PrepareInstallOutput {
                            maybe: None,
                        }))
                        .set_continues(Some(true))),
                        vec![fd],
                    ));
                    cancel_rx
                }
                Err(e) => {
                    eprintln!("create_sidechannel_fd() returned error {:?}", e);

                    let _ = tx.try_send((
                        Err(interface::DynamicLauncherError::Other {
                            message: e.to_string(),
                        }),
                        vec![],
                    ));
                    panic!();
                }
            };

            match handle_prepare_install(
                parent_window,
                name,
                icon_v,
                options,
                cancel_rx,
                transient_permissions,
            )
            .await
            {
                Ok(result) => {
                    let _ = tx.try_send((
                        Ok(zlink::Reply::new(Some(result)).set_continues(None)),
                        vec![],
                    ));
                }
                Err(e) => {
                    if e != DynamicLauncherError::Cancelled {
                        eprintln!("handle_prepare_install() returned error {:?}", e);
                    }

                    let _ = tx.try_send((Err(e), vec![]));
                }
            }
        });

        // Receiver implements Stream, so can just return that
        rx
    }

    async fn request_install_token(
        &mut self,
        name: String,
        icon_v: serde_json::Value,
        options: HashMap<String, serde_json::Value>,
        #[zlink(connection)] conn: &mut Connection<Sock>,
    ) -> Result<interface::RequestInstallTokenOutput, interface::DynamicLauncherError> {
        let app_id = get_flatpak_app_id(conn).await?;

        let allowed = backend_handle_request_install_token(&app_id, &options);

        if !allowed {
            return Err(interface::DynamicLauncherError::Other {
                message: format!("RequestInstallToken() not allowed for app id {}", app_id),
            });
        }

        let (icon_format, icon_size, icon_bytes) =
            validate_serialized_icon(icon_v).map_err(|e| {
                interface::DynamicLauncherError::Other {
                    message: format!("Icon failed validation: {:#}", e),
                }
            })?;

        let token = Uuid::new_v4().to_string();
        self.transient_permissions.lock().unwrap().insert(
            token.clone(),
            LauncherData {
                name,
                icon_bytes,
                icon_format,
                icon_size,
                inserted_at: std::time::Instant::now(),
            },
        );

        Ok(interface::RequestInstallTokenOutput { token })
    }

    async fn uninstall(
        &mut self,
        desktop_file_id: String,
        options: HashMap<String, serde_json::Value>,
        #[zlink(connection)] conn: &mut Connection<Sock>,
    ) -> Result<(), interface::DynamicLauncherError> {
        let app_id = get_flatpak_app_id(conn).await?;

        validate_desktop_file_id(&app_id, &desktop_file_id).map_err(|e| {
            interface::DynamicLauncherError::InvalidArgument {
                message: format!("Validating desktop file id failed: {:#}", e),
            }
        })?;

        let data_dir = glib::user_data_dir();
        let icon_dir = data_dir.join(XDG_PORTAL_ICONS_DIR);
        let desktop_dir = data_dir.join(XDG_PORTAL_APPLICATIONS_DIR);

        let link_path = data_dir.join("applications").join(&desktop_file_id);
        std::fs::remove_file(&link_path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                interface::DynamicLauncherError::Other {
                    message: format!(
                        "Uninstall() method failed because launcher '{}' does not exist",
                        desktop_file_id
                    ),
                }
            } else {
                interface::DynamicLauncherError::Other {
                    message: format!("{:#}", e),
                }
            }
        })?;

        let desktop_path = desktop_dir.join(&desktop_file_id);

        // Read the Icon path from the desktop file before deleting it.
        let key_file = glib::KeyFile::new();
        let icon_path = if key_file
            .load_from_file(&desktop_path, glib::KeyFileFlags::NONE)
            .is_ok()
            && let Ok(icon) = key_file.string(glib::KEY_FILE_DESKTOP_GROUP, "Icon")
        {
            Some(std::path::PathBuf::from(icon.as_str()))
        } else {
            None
        };

        let desktop_delete_result = std::fs::remove_file(&desktop_path);

        if let Some(icon_path) = icon_path
            && icon_path.starts_with(&icon_dir)
        {
            std::fs::remove_file(&icon_path).map_err(|e| {
                interface::DynamicLauncherError::Other {
                    message: format!("{:#}", e),
                }
            })?;
        }

        desktop_delete_result.map_err(|e| interface::DynamicLauncherError::Other {
            message: format!("{:#}", e),
        })?;

        Ok(())
    }

    async fn get_desktop_entry(
        &mut self,
        desktop_file_id: String,
        #[zlink(connection)] conn: &mut Connection<Sock>,
    ) -> Result<interface::GetDesktopEntryOutput, interface::DynamicLauncherError> {
        let app_id = get_flatpak_app_id(conn).await?;

        validate_desktop_file_id(&app_id, &desktop_file_id).map_err(|e| {
            interface::DynamicLauncherError::InvalidArgument {
                message: format!("Validating desktop file id failed: {:#}", e),
            }
        })?;

        let data_dir = glib::user_data_dir();
        let desktop_dir = data_dir.join(XDG_PORTAL_APPLICATIONS_DIR);

        let desktop_path = desktop_dir.join(&desktop_file_id);
        let metadata = std::fs::metadata(&desktop_path).map_err(|e| {
            interface::DynamicLauncherError::InvalidArgument {
                message: format!("Desktop file not found: {:#}", e),
            }
        })?;
        if metadata.len() as usize > MAX_DESKTOP_SIZE_BYTES {
            return Err(interface::DynamicLauncherError::Other {
                message: format!(
                    "Desktop file exceeds max size ({}): {}",
                    MAX_DESKTOP_SIZE_BYTES, desktop_file_id
                ),
            });
        }

        let contents = std::fs::read_to_string(&desktop_path).map_err(|e| {
            interface::DynamicLauncherError::Other {
                message: format!("Failed to read desktop file: {:#}", e),
            }
        })?;

        Ok(interface::GetDesktopEntryOutput { contents })
    }

    async fn get_icon(
        &mut self,
        desktop_file_id: &str,
        #[zlink(connection)] conn: &mut Connection<Sock>,
    ) -> Result<interface::GetIconOutput, interface::DynamicLauncherError> {
        let app_id = get_flatpak_app_id(conn).await?;

        validate_desktop_file_id(&app_id, desktop_file_id).map_err(|e| {
            interface::DynamicLauncherError::InvalidArgument {
                message: format!("Validating desktop file id failed: {:#}", e),
            }
        })?;

        let data_dir = glib::user_data_dir();
        let desktop_dir = data_dir.join(XDG_PORTAL_APPLICATIONS_DIR);
        let icon_dir = data_dir.join(XDG_PORTAL_ICONS_DIR);

        let desktop_path = desktop_dir.join(desktop_file_id);
        let metadata = std::fs::metadata(&desktop_path).map_err(|e| {
            interface::DynamicLauncherError::InvalidArgument {
                message: format!("Desktop file not found: {:#}", e),
            }
        })?;
        if metadata.len() as usize > MAX_DESKTOP_SIZE_BYTES {
            return Err(interface::DynamicLauncherError::Other {
                message: format!(
                    "Desktop file exceeds max size ({}): {}",
                    MAX_DESKTOP_SIZE_BYTES, desktop_file_id
                ),
            });
        }

        let key_file = glib::KeyFile::new();
        key_file
            .load_from_file(&desktop_path, glib::KeyFileFlags::NONE)
            .map_err(|e| interface::DynamicLauncherError::Other {
                message: format!("Failed to load desktop file: {:#}", e),
            })?;

        let icon_path = key_file
            .string(glib::KEY_FILE_DESKTOP_GROUP, "Icon")
            .ok()
            .map(|s| std::path::PathBuf::from(s.as_str()));

        let mut icon_format: Option<&'static str> = None;
        let mut icon_size: i32 = 0;

        if let Some(ref icon_path) = icon_path
            && icon_path.starts_with(&icon_dir)
        {
            let path_str = icon_path.to_string_lossy();
            if path_str.ends_with(".png") {
                icon_format = Some("png");
            } else if path_str.ends_with(".svg") {
                icon_format = Some("svg");
            } else if path_str.ends_with(".jpeg") || path_str.ends_with(".jpg") {
                icon_format = Some("jpeg");
            }

            // dir should be either scalable or e.g. 512x512
            if let Some(dir_basename) = icon_path
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy())
            {
                if dir_basename == "scalable" {
                    // An svg can have a width and height set, but it is probably not
                    // needed since it can be scaled to any size.
                    icon_size = 4096;
                } else if let Some(x) = dir_basename.find('x') {
                    icon_size = dir_basename[x + 1..]
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect::<String>()
                        .parse()
                        .unwrap_or(0);
                }
            }
        }

        let (icon_format, icon_path) = match (icon_format, icon_path) {
            (Some(fmt), Some(path)) if icon_size > 0 && icon_size <= 4096 => (fmt, path),
            _ => {
                println!(
                    "Desktop file '{}' icon at unrecognized path",
                    desktop_file_id
                );
                return Err(interface::DynamicLauncherError::Other {
                    message: format!(
                        "Desktop file '{}' icon at unrecognized path",
                        desktop_file_id
                    ),
                });
            }
        };

        // Icons are usually smaller than 1 MiB. Set a 10 MiB limit so we can't
        // use a huge amount of memory.
        const MAX_ICON_SIZE: u64 = 10 * 1024 * 1024;
        let metadata =
            std::fs::metadata(&icon_path).map_err(|e| interface::DynamicLauncherError::Other {
                message: format!("Failed to read icon metadata: {:#}", e),
            })?;
        if metadata.len() > MAX_ICON_SIZE {
            return Err(interface::DynamicLauncherError::Other {
                message: format!("Desktop file '{}' icon exceeds size limit", desktop_file_id),
            });
        }
        let icon_bytes =
            std::fs::read(&icon_path).map_err(|e| interface::DynamicLauncherError::Other {
                message: format!("Failed to read icon: {:#}", e),
            })?;

        let bytes_icon = gio::BytesIcon::new(&glib::Bytes::from_owned(icon_bytes));
        let icon_variant = gio::prelude::IconExt::serialize(&bytes_icon).ok_or(
            interface::DynamicLauncherError::Other {
                message: format!(
                    "Desktop file '{}' icon failed to serialize",
                    desktop_file_id
                ),
            },
        )?;
        let icon_v = serde_json::Value::String(icon_variant.to_string());

        Ok(interface::GetIconOutput {
            icon_v,
            icon_format: icon_format.to_string(),
            icon_size: icon_size as i64,
        })
    }

    async fn get_supported_launcher_types(
        &mut self,
    ) -> Result<interface::GetSupportedLauncherTypesOutput, interface::DynamicLauncherError> {
        Ok(interface::GetSupportedLauncherTypesOutput {
            supported_launcher_types: SUPPORTED_LAUNCHER_TYPES.to_vec(),
        })
    }

    async fn launch(
        &mut self,
        desktop_file_id: String,
        options: HashMap<String, serde_json::Value>,
        #[zlink(connection)] conn: &mut Connection<Sock>,
    ) -> Result<(), interface::DynamicLauncherError> {
        let app_id = get_flatpak_app_id(conn).await?;

        validate_desktop_file_id(&app_id, &desktop_file_id).map_err(|e| {
            interface::DynamicLauncherError::InvalidArgument {
                message: format!("Validating desktop file id failed: {:#}", e),
            }
        })?;

        let data_dir = glib::user_data_dir();
        let desktop_path = data_dir
            .join(XDG_PORTAL_APPLICATIONS_DIR)
            .join(&desktop_file_id);
        if !desktop_path.exists() {
            return Err(interface::DynamicLauncherError::InvalidArgument {
                message: format!("No dynamic launcher exists with id '{}'", desktop_file_id),
            });
        }

        let activation_token = options
            .get("activation_token")
            .and_then(|v| v.as_str())
            .map(String::from);

        let desktop_path = desktop_path.clone();
        let desktop_file_id_clone = desktop_file_id.clone();
        let (tx, rx) = oneshot::channel::<Result<(), anyhow::Error>>();

        // Now launch the app on the main thread, using the global-default main context
        glib::MainContext::default().invoke(move || {
            let launch_context = gio::AppLaunchContext::new();

            // Unset env var that we set before, so the child doesn't inherit it
            launch_context.unsetenv("GIO_USE_VFS");

            // Set activation token for focus stealing prevention
            // FIXME: need to subclass the app launch context for this

            let result = match gio_unix::DesktopAppInfo::from_filename(&desktop_path) {
                Some(gappinfo) => {
                    println!("Launching {}", desktop_file_id_clone);
                    gappinfo
                        .launch(&[], Some(&launch_context))
                        .context(format!("Failed to launch '{}'", desktop_file_id_clone))
                }
                None => Err(anyhow!(
                    "Failed to create GDesktopAppInfo for launcher with id '{}'",
                    desktop_file_id_clone
                )),
            };
            let _ = tx.send(result);
        });

        match rx.await {
            Ok(_) => Ok(()),
            Err(e) => Err(interface::DynamicLauncherError::Other {
                message: format!("Launch failed: {:#}", e),
            }),
        }
    }
}

pub async fn server_run(listener_fd: OwnedFd) -> Result<(), anyhow::Error> {
    // Use a pre-accepted stream (systemd-socket activation with Accept=true) like this:
    /*
    let std_stream: StdUnixStream = listener_fd.into();
    std_stream.set_nonblocking(true)?;
    let tokio_stream = TokioUnixStream::from_std(std_stream)?;
    let mut listener: ReadyListener<ZlinkUnixStream> = ReadyListener::new(tokio_stream.into());
    */
    let std_listener: std::os::unix::net::UnixListener = listener_fd.into();
    std_listener.set_nonblocking(true)?;
    let tokio_listener = tokio::net::UnixListener::from_std(std_listener)?;
    let listener = zlink::unix::Listener::from(tokio_listener);

    let server = Server::new(listener, DynamicLauncher::new());

    Ok(server.run().await?)
}

fn tokio_runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| Runtime::new().expect("Setting up tokio runtime needs to succeed."))
}

fn main() -> Result<(), anyhow::Error> {
    // Safety: Unsafe because we're only unsetting env variables in there.
    // Right now we're still single threaded, and Tokio/glib isn't initialized either, so
    // this is fine.
    let mut fds = unsafe { ListenFds::new()? };

    if let Some(fd) = fds.take("varlink").next() {
        gxdp::init_gtk(gxdp::ServiceClientType::PortalBackend, &[]).context("GXDP init failed")?;

        // Initialise gtk on the main thread
        gtk4::init()?;

        // Put zlink on its own thread so that Tokio's event loop does not block
        // the GLib main context on the main thread
        let _zlink_thread = std::thread::spawn(move || {
            if let Err(e) = tokio_runtime().block_on(server_run(fd)) {
                eprintln!("zlink error: {:#}", e);
            }
        });

        // Put the GLib main loop on the main thread, driving the global-default main context
        glib::MainLoop::new(None, false).run();
        Ok(())
    } else {
        Err(anyhow::anyhow!("No \"varlink\" FD passed"))
    }
}
