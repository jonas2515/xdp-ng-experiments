use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, glib};

const APP_ID: &str = "com.example.XdpNgDynamicLauncher.DemoSubApp";

fn build_ui(app: &Application) {
    let label = gtk4::Label::new(Some("This is the app we just installed!"));

    let window = ApplicationWindow::builder()
        .application(app)
        .title("XDP-NG Demo App")
        .default_width(360)
        .default_height(600)
        .child(&label)
        .build();

    window.present();
}

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();

    app.connect_activate(build_ui);
    app.run()
}
