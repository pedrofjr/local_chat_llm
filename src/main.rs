mod crypto;
mod media;
mod net;
mod room;
mod store;
mod sys;
mod tui;
mod update;

fn main() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(tui::run())
}
