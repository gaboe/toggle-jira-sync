use crate::{cli::StatusArgs, local_api::LocalServer};

pub async fn run(args: StatusArgs) -> anyhow::Result<()> {
    let server = LocalServer::start(args.paths, None, args.limit).await?;
    let report = server.client().status().await?;

    if args.json {
        println!("{}", report.to_json_string()?);
    } else {
        println!("{}", report.to_human_string());
    }

    Ok(())
}
