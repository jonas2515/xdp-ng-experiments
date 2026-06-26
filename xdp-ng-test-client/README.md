# Example Varlink Dynamic Launcher Client

An example client to the new Varlink-based Dynamic Launcher portal.

## Build

Client needs to be run as a Flatpak, since the new portal doesn't support apps running on the host.

### Install the GNOME and Rust SDK from Flathub

`flatpak install org.gnome.Sdk/x86_64/50`

`flatpak install org.freedesktop.Sdk.Extension.rust-stable/x86_64/25.08`

### Build and Install the Flatpak as User

`flatpak-builder --user --force-clean --install build-dir com.example.XdpNgDynamicLauncher.yml`

## Running

`flatpak run com.example.XdpNgDynamicLauncher`