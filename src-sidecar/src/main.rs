use std::{env, path::PathBuf};

use anyhow::{anyhow, Context, Result};
use mothership_core::{
    auth::{
        CompleteAuthRequest, FileCredentialVault, MockProviderAuthAdapter, ProviderAuthService,
        ProviderConnectionId, StartAuthRequest, StaticProviderAuthAdapterRegistry,
    },
    Database,
};
use serde_json::json;

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
        Request::AuthProviders { database_path } => {
            with_auth_service(database_path, |auth| {
                println!("{}", serde_json::to_string(&auth.list_providers())?);
                Ok(())
            })?;
        }
        Request::AuthConnectMock {
            database_path,
            account_label,
        } => {
            with_auth_service(database_path, |auth| {
                let session = auth.start_auth(StartAuthRequest {
                    provider_id: MockProviderAuthAdapter::PROVIDER_ID.into(),
                    auth_method_id: MockProviderAuthAdapter::AUTH_METHOD_ID.into(),
                })?;
                let connection = auth.complete_auth(CompleteAuthRequest {
                    session_id: session.id,
                    payload: json!({ "accountLabel": account_label }),
                })?;
                println!("{}", serde_json::to_string(&connection)?);
                Ok(())
            })?;
        }
        Request::AuthConnections { database_path } => {
            with_auth_service(database_path, |auth| {
                println!("{}", serde_json::to_string(&auth.list_connections()?)?);
                Ok(())
            })?;
        }
        Request::AuthDisconnect {
            database_path,
            connection_id,
        } => {
            with_auth_service(database_path, |auth| {
                auth.disconnect(&ProviderConnectionId::from(connection_id))?;
                println!("{}", serde_json::to_string(&json!({ "ok": true }))?);
                Ok(())
            })?;
        }
    }

    Ok(())
}

enum Request {
    Status {
        database_path: PathBuf,
    },
    AuthProviders {
        database_path: PathBuf,
    },
    AuthConnectMock {
        database_path: PathBuf,
        account_label: String,
    },
    AuthConnections {
        database_path: PathBuf,
    },
    AuthDisconnect {
        database_path: PathBuf,
        connection_id: String,
    },
}

impl Request {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        match args.next().as_deref() {
            Some("status") => Self::parse_status(args),
            Some("auth") => Self::parse_auth(args),
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

    fn parse_auth(mut args: impl Iterator<Item = String>) -> Result<Self> {
        match args.next().as_deref() {
            Some("providers") => {
                let database_path = parse_database_path(args)?;
                Ok(Request::AuthProviders { database_path })
            }
            Some("connect-mock") => {
                let (database_path, account_label) = parse_mock_connect_args(args)?;
                Ok(Request::AuthConnectMock {
                    database_path,
                    account_label,
                })
            }
            Some("connections") => {
                let database_path = parse_database_path(args)?;
                Ok(Request::AuthConnections { database_path })
            }
            Some("disconnect") => {
                let (database_path, connection_id) = parse_disconnect_args(args)?;
                Ok(Request::AuthDisconnect {
                    database_path,
                    connection_id,
                })
            }
            Some(command) => Err(anyhow!("unsupported auth command: {command}")),
            None => Err(anyhow!("missing auth command")),
        }
    }
}

fn with_auth_service<T>(
    database_path: PathBuf,
    run: impl FnOnce(&ProviderAuthService<'_>) -> Result<T>,
) -> Result<T> {
    let database = Database::open(database_path).context("open SQLite database")?;
    let vault = FileCredentialVault::new(auth_store_path(&database));
    let registry = StaticProviderAuthAdapterRegistry::with_mock_adapter();
    let service = ProviderAuthService::new(&database, &vault, &registry);
    run(&service)
}

fn auth_store_path(database: &Database) -> PathBuf {
    database
        .path()
        .parent()
        .map(|path| path.join("auth"))
        .unwrap_or_else(|| PathBuf::from("auth"))
}

fn parse_database_path(args: impl Iterator<Item = String>) -> Result<PathBuf> {
    let mut database_path = None;

    parse_named_args(args, |name, value| match name {
        "--database" => {
            database_path = Some(PathBuf::from(value));
            Ok(())
        }
        unknown => Err(anyhow!("unsupported auth argument: {unknown}")),
    })?;

    database_path.ok_or_else(|| anyhow!("missing --database path"))
}

fn parse_mock_connect_args(args: impl Iterator<Item = String>) -> Result<(PathBuf, String)> {
    let mut database_path = None;
    let mut account_label = None;

    parse_named_args(args, |name, value| match name {
        "--database" => {
            database_path = Some(PathBuf::from(value));
            Ok(())
        }
        "--label" => {
            account_label = Some(value);
            Ok(())
        }
        unknown => Err(anyhow!("unsupported auth connect-mock argument: {unknown}")),
    })?;

    let database_path = database_path.ok_or_else(|| anyhow!("missing --database path"))?;
    let account_label = account_label.unwrap_or_else(|| "Mock Account".to_string());
    Ok((database_path, account_label))
}

fn parse_disconnect_args(args: impl Iterator<Item = String>) -> Result<(PathBuf, String)> {
    let mut database_path = None;
    let mut connection_id = None;

    parse_named_args(args, |name, value| match name {
        "--database" => {
            database_path = Some(PathBuf::from(value));
            Ok(())
        }
        "--connection" => {
            connection_id = Some(value);
            Ok(())
        }
        unknown => Err(anyhow!("unsupported auth disconnect argument: {unknown}")),
    })?;

    let database_path = database_path.ok_or_else(|| anyhow!("missing --database path"))?;
    let connection_id = connection_id.ok_or_else(|| anyhow!("missing --connection id"))?;
    Ok((database_path, connection_id))
}

fn parse_named_args(
    mut args: impl Iterator<Item = String>,
    mut handle: impl FnMut(&str, String) -> Result<()>,
) -> Result<()> {
    while let Some(argument) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| anyhow!("{argument} requires a value"))?;
        handle(argument.as_str(), value)?;
    }

    Ok(())
}
