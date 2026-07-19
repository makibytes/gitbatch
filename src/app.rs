use crate::{
    Result, cli::Cli, config::AppConfig, discovery::discover_repositories, git::GitRunner, quick,
    tui,
};

pub async fn run(cli: Cli) -> Result<()> {
    let config = AppConfig::load(cli)?;
    let trace_path = config
        .trace
        .then(|| {
            std::env::current_dir()
                .ok()
                .map(|dir| dir.join("gitbatch.log"))
        })
        .flatten();
    let runner = GitRunner::with_trace(std::time::Duration::from_secs(90), trace_path);
    let repositories = discover_repositories(&config.directories, config.depth)?;

    if config.quick {
        quick::run(&runner, repositories, config.mode).await
    } else {
        tui::run(runner, &config, repositories).await
    }
}
