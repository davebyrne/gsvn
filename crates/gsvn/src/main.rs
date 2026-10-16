use adw::prelude::*;
use gtk::glib;

const APP_ID: &str = "io.github.gsvn.Gsvn";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_start_window);
    app.run()
}

fn build_start_window(app: &adw::Application) {
    let page = adw::StatusPage::builder()
        .icon_name("folder-remote-symbolic")
        .title("gsvn")
        .description("Open a Subversion repository")
        .build();

    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&page));

    adw::ApplicationWindow::builder()
        .application(app)
        .title("gsvn")
        .default_width(900)
        .default_height(600)
        .content(&view)
        .build()
        .present();
}
