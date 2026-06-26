# XDG Desktop Portal Next Gen

Prototype of the Dynamic Launcher portal, ported to Rust, using Varlink, and unifying the portal backend and frontend into a single codebase.

## Build

Building requires the [libgxdp bindings for Rust](https://github.com/jonas2515/gxdp-rs) to be present in the parent folder.

Once the bindings are set up, build as usual with cargo.

## Run

Run it using `systemd-socket-activate`, forwarding env variables necessary for GTK to run:

```
systemd-socket-activate -E XDG_SESSION_TYPE=$XDG_SESSION_TYPE -E WAYLAND_DISPLAY=$WAYLAND_DISPLAY -E XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR --fdname "varlink" -l /tmp/xdp-ng-example.socket target/debug/dynamic_launcher
```
