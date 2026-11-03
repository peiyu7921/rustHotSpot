#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod dhcp;
mod hotspot;
mod netcfg;
mod tray;
mod util;
mod worker;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    if !util::is_admin() {
        log::info!("需要管理员权限，正在提权…");
        util::relaunch_as_admin()?;
        return Ok(());
    }

    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|arg| arg == "--headless") {
        let seconds = args
            .get(pos + 1)
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(25);
        app::run_headless(seconds)?;
        return Ok(());
    }

    app::run().map_err(|err| anyhow::anyhow!("界面启动失败: {err}"))
}
