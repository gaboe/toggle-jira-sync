use crate::{app, cli::StatusArgs};

pub async fn run(args: StatusArgs) -> anyhow::Result<()> {
    let (_, _, report) = app::status_report(args.paths, args.limit)?;

    if args.json {
        println!("{}", report.to_json_string()?);
    } else {
        println!("{}", report.to_human_string());
    }

    Ok(())
}
