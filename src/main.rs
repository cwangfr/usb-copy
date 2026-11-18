#![cfg_attr(windows, windows_subsystem = "windows")]

mod ui;

fn main() -> eframe::Result {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/usb-copy.png"))
        .expect("embedded app icon is a valid PNG");
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_app_id("usb-copy")
            .with_icon(icon)
            .with_inner_size([1000.0, 680.0])
            .with_min_inner_size([740.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(
        "U 盘复制工具",
        options,
        Box::new(|creation| Ok(Box::new(ui::App::new(creation)))),
    )
}
