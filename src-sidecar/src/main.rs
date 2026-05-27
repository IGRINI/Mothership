use std::{env, path::PathBuf};

use anyhow::{anyhow, Context, Result};
use mothership_core::Database;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let request = Request::parse(env::args().skip(1))?;

    match request {
        Request::Status { database_path } => {
            let database = Database::open(database_path).context("open SQLite database")?;
            let status = database
                .sidecar_status()
                .context("collect sidecar status")?;
            println!("{}", serde_json::to_string(&status)?);
        }
    }

    Ok(())
}

enum Request {
    Status { database_path: PathBuf },
}

impl Request {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        match args.next().as_deref() {
            Some("status") => Self::parse_status(args),
            Some(command) => Err(anyhow!("unsupported sidecar command: {command}")),
            None => Err(anyhow!("missing sidecar command")),
        }
    }

    fn parse_status(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let mut database_path = None;

        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--database" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--database requires a path"))?;
                    database_path = Some(PathBuf::from(value));
                }
                unknown => return Err(anyhow!("unsupported status argument: {unknown}")),
            }
        }

        let database_path = database_path.ok_or_else(|| anyhow!("missing --database path"))?;
        Ok(Request::Status { database_path })
    }
}
