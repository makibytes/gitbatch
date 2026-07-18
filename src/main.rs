use gitbatch::{app, cli::Cli};

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let cli = Cli::parse();
    if let Err(error) = app::run(cli).await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
