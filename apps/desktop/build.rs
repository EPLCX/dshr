// Windows：把应用图标编进可执行文件。资源 ID 1 是 GPUI 创建窗口时读取的图标，资源管理器和任务栏也用它。
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=app.rc");
        println!("cargo:rerun-if-changed=../../assets/icon.ico");
        embed_resource::compile("app.rc", embed_resource::NONE).manifest_required().unwrap();
    }
}
